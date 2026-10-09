//! The crawl engine: a queue per host, robots.txt and TDMRep checks before
//! a site's first request, per-host concurrency and adaptive delays, and
//! deduplication. The caller submits requests and takes events in batches.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use url::Url;

use crate::canonical::{Fingerprint, fingerprint};
use crate::fetch::{FetchError, FetchOptions, Fetcher, Response};
use crate::robots::Robots;
use crate::tdmrep::{self, Reservation, TdmFile};

#[derive(Debug, Clone)]
pub struct CrawlSettings {
    /// Requests in flight across all sites.
    pub concurrency: usize,
    /// Requests in flight to one host.
    pub per_domain: usize,
    pub obey_robots: bool,
    /// The product token matched against robots.txt user-agent lines.
    pub robots_agent: String,
    pub obey_tdmrep: bool,
    /// Adjust each host's delay to its response times (AutoThrottle).
    pub throttle: bool,
    /// Each host's delay before its first response is timed.
    pub start_delay: Duration,
    pub min_delay: Duration,
    pub max_delay: Duration,
    /// Requests the throttle aims to have in flight to one host.
    pub target_concurrency: f64,
}

impl Default for CrawlSettings {
    fn default() -> Self {
        CrawlSettings {
            concurrency: 64,
            per_domain: 8,
            obey_robots: true,
            robots_agent: "netweir".to_string(),
            obey_tdmrep: true,
            throttle: true,
            start_delay: Duration::from_secs(1),
            min_delay: Duration::ZERO,
            max_delay: Duration::from_secs(60),
            target_concurrency: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrawlRequest {
    /// The caller's handle for this request; events carry it back.
    pub id: u64,
    pub url: String,
    /// Higher goes first among a host's waiting requests.
    pub priority: i32,
    pub headers: Vec<(String, String)>,
    /// Fetch even if an equal request was seen before.
    pub dont_filter: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// An equal request was already submitted.
    Duplicate,
    /// Not an http(s) URL.
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// robots.txt disallows it.
    Robots,
    /// The site reserves text and data mining rights for it (TDMRep).
    TdmReserved,
}

#[derive(Debug, Clone)]
pub enum Event {
    Fetched { id: u64, response: Response },
    Failed { id: u64, error: FetchError },
    Dropped { id: u64, reason: DropReason },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Waiting in a host's queue.
    pub queued: usize,
    pub in_flight: usize,
    pub fetched: u64,
    pub failed: u64,
    pub duplicates: u64,
    pub dropped_robots: u64,
    pub dropped_tdm: u64,
    pub bytes: u64,
}

/// A crawl in progress. Dropping it stops the scheduler; fetches already
/// under way finish on their own.
pub struct Crawler {
    shared: Arc<Shared>,
}

struct Shared {
    fetcher: Fetcher,
    settings: CrawlSettings,
    state: Mutex<State>,
    /// Woken when there may be something to schedule.
    schedule: Notify,
    /// Woken when an event arrives or the crawl may have finished.
    events: Notify,
}

#[derive(Default)]
struct State {
    seq: u64,
    seen: HashSet<Fingerprint>,
    hosts: HashMap<String, Host>,
    origins: HashMap<String, Origin>,
    events: VecDeque<Event>,
    /// robots.txt and tdmrep.json fetches under way.
    gate_fetches: usize,
    stats: Stats,
    closed: bool,
}

struct Host {
    queue: BinaryHeap<Queued>,
    in_flight: usize,
    next_at: Instant,
    delay: Duration,
    /// The robots.txt Crawl-delay, below which the delay never goes.
    floor: Duration,
}

#[derive(Default)]
struct Origin {
    robots: Gate<Robots>,
    tdm: Gate<Option<TdmFile>>,
}

#[derive(Default)]
enum Gate<T> {
    #[default]
    Unknown,
    Fetching,
    Ready(T),
}

struct Queued {
    priority: i32,
    seq: u64,
    request: CrawlRequest,
    origin: String,
    path: String,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    /// Higher priority first, then first submitted first.
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

impl Crawler {
    /// Starts the scheduler on the current tokio runtime.
    pub fn new(fetch: FetchOptions, settings: CrawlSettings) -> Result<Crawler, FetchError> {
        let shared = Arc::new(Shared {
            fetcher: Fetcher::new(fetch)?,
            settings,
            state: Mutex::new(State::default()),
            schedule: Notify::new(),
            events: Notify::new(),
        });
        tokio::spawn(schedule_loop(shared.clone()));
        Ok(Crawler { shared })
    }

    pub fn submit(&self, request: CrawlRequest) -> Submitted {
        let Ok(url) = Url::parse(&request.url) else {
            return Submitted::Invalid;
        };
        let (Some(host), true) = (url.host_str(), matches!(url.scheme(), "http" | "https")) else {
            return Submitted::Invalid;
        };
        let Some(fp) = fingerprint("GET", &request.url) else {
            return Submitted::Invalid;
        };
        let origin = url.origin().ascii_serialization();
        let path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        let host = host.to_string();
        let mut state = self.shared.lock();
        if !state.seen.insert(fp) && !request.dont_filter {
            state.stats.duplicates += 1;
            return Submitted::Duplicate;
        }
        state.seq += 1;
        let seq = state.seq;
        let settings = &self.shared.settings;
        let entry = state.hosts.entry(host).or_insert_with(|| Host {
            queue: BinaryHeap::new(),
            in_flight: 0,
            next_at: Instant::now(),
            delay: if settings.throttle {
                settings.start_delay
            } else {
                settings.min_delay
            },
            floor: Duration::ZERO,
        });
        entry.queue.push(Queued {
            priority: request.priority,
            seq,
            request,
            origin,
            path,
        });
        state.stats.queued += 1;
        drop(state);
        self.shared.schedule.notify_one();
        Submitted::Queued
    }

    /// Waits for at least one event and returns up to `max`. Returns an
    /// empty batch once nothing is queued, in flight or being checked: the
    /// crawl is finished unless more is submitted.
    pub async fn next(&self, max: usize) -> Vec<Event> {
        next_events(self.shared.clone(), max).await
    }

    /// `next`, as a future that owns what it needs, for callers that can't
    /// hold a borrow across the await (such as Python bindings).
    pub fn next_owned(
        &self,
        max: usize,
    ) -> impl std::future::Future<Output = Vec<Event>> + Send + 'static {
        next_events(self.shared.clone(), max)
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().stats.clone()
    }
}

async fn next_events(shared: Arc<Shared>, max: usize) -> Vec<Event> {
    loop {
        let wake = shared.events.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        {
            let mut state = shared.lock();
            if !state.events.is_empty() {
                let n = max.max(1).min(state.events.len());
                return state.events.drain(..n).collect();
            }
            if state.stats.queued == 0 && state.stats.in_flight == 0 && state.gate_fetches == 0 {
                return Vec::new();
            }
        }
        wake.await;
    }
}

impl Drop for Crawler {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.schedule.notify_one();
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push_event(&self, state: &mut State, event: Event) {
        match &event {
            Event::Fetched { response, .. } => {
                state.stats.fetched += 1;
                state.stats.bytes += response.body.len() as u64;
            }
            Event::Failed { .. } => state.stats.failed += 1,
            Event::Dropped {
                reason: DropReason::Robots,
                ..
            } => state.stats.dropped_robots += 1,
            Event::Dropped {
                reason: DropReason::TdmReserved,
                ..
            } => state.stats.dropped_tdm += 1,
        }
        state.events.push_back(event);
        self.events.notify_waiters();
    }
}

/// What the scheduler decided to do with a host's next request.
enum Action {
    Fetch(Queued),
    CheckRobots(String),
    CheckTdm(String),
}

/// Runs until the Crawler is dropped, which sets `closed` and wakes it.
async fn schedule_loop(s: Arc<Shared>) {
    loop {
        let wake = s.schedule.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let (actions, sleep_until) = {
            let mut state = s.lock();
            if state.closed {
                return;
            }
            plan(&s.settings, &mut state, &s)
        };
        for action in actions {
            match action {
                Action::Fetch(q) => {
                    tokio::spawn(fetch(s.clone(), q));
                }
                Action::CheckRobots(origin) => {
                    tokio::spawn(check_robots(s.clone(), origin));
                }
                Action::CheckTdm(origin) => {
                    tokio::spawn(check_tdm(s.clone(), origin));
                }
            }
        }
        match sleep_until {
            Some(at) => {
                tokio::select! {
                    _ = wake => {}
                    _ = tokio::time::sleep_until(at.into()) => {}
                }
            }
            None => wake.await,
        }
    }
}

/// Starts everything that may start now; returns what to do and when to
/// look again.
fn plan(
    settings: &CrawlSettings,
    state: &mut State,
    shared: &Shared,
) -> (Vec<Action>, Option<Instant>) {
    let now = Instant::now();
    let mut actions = Vec::new();
    let mut sleep_until: Option<Instant> = None;
    let hosts: Vec<String> = state.hosts.keys().cloned().collect();
    for name in hosts {
        loop {
            if state.stats.in_flight >= settings.concurrency {
                return (actions, sleep_until);
            }
            let (origin, path) = {
                let host = &state.hosts[&name];
                let Some(next) = host.queue.peek() else { break };
                (next.origin.clone(), next.path.clone())
            };
            // Robots and TDMRep must be known for this origin first.
            let o = state.origins.entry(origin.clone()).or_default();
            if settings.obey_robots {
                match o.robots {
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        state.gate_fetches += 1;
                        actions.push(Action::CheckRobots(origin));
                        break;
                    }
                    Gate::Fetching => break,
                    Gate::Ready(_) => {}
                }
            }
            if settings.obey_tdmrep {
                match o.tdm {
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        state.gate_fetches += 1;
                        actions.push(Action::CheckTdm(origin));
                        break;
                    }
                    Gate::Fetching => break,
                    Gate::Ready(_) => {}
                }
            }
            let blocked = match (&o.robots, &o.tdm) {
                (Gate::Ready(r), _)
                    if settings.obey_robots && !r.allowed(&settings.robots_agent, &path) =>
                {
                    Some(DropReason::Robots)
                }
                (_, Gate::Ready(Some(f)))
                    if settings.obey_tdmrep && f.reservation(&path).reserved == Some(true) =>
                {
                    Some(DropReason::TdmReserved)
                }
                _ => None,
            };
            let floor = match &o.robots {
                Gate::Ready(r) => r.crawl_delay(&settings.robots_agent).unwrap_or_default(),
                _ => Duration::ZERO,
            };
            let host = state.hosts.get_mut(&name).expect("host listed above");
            if let Some(reason) = blocked {
                let q = host.queue.pop().expect("peeked above");
                state.stats.queued -= 1;
                shared.push_event(
                    state,
                    Event::Dropped {
                        id: q.request.id,
                        reason,
                    },
                );
                continue;
            }
            host.floor = floor;
            if host.in_flight >= settings.per_domain {
                break;
            }
            if now < host.next_at {
                sleep_until = Some(sleep_until.map_or(host.next_at, |s| s.min(host.next_at)));
                break;
            }
            let q = host.queue.pop().expect("peeked above");
            host.in_flight += 1;
            host.next_at = now + host.delay.max(host.floor);
            state.stats.queued -= 1;
            state.stats.in_flight += 1;
            actions.push(Action::Fetch(q));
        }
    }
    state
        .hosts
        .retain(|_, h| !h.queue.is_empty() || h.in_flight > 0 || h.next_at > now);
    (actions, sleep_until)
}

async fn fetch(shared: Arc<Shared>, q: Queued) {
    let started = Instant::now();
    let result = shared.fetcher.get(&q.request.url, &q.request.headers).await;
    let latency = started.elapsed();
    let settings = &shared.settings;
    let mut state = shared.lock();
    state.stats.in_flight -= 1;
    let host_name = Url::parse(&q.request.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if let Some(host) = state.hosts.get_mut(&host_name) {
        host.in_flight -= 1;
        if settings.throttle {
            let status = result.as_ref().map(|r| r.status).ok();
            host.delay = adjust_delay(settings, host.delay, latency, status);
        }
    }
    let event = match result {
        Ok(response) => {
            let reserved = settings.obey_tdmrep && page_reserved(&state, &q, &response);
            if reserved {
                Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                }
            } else {
                Event::Fetched {
                    id: q.request.id,
                    response,
                }
            }
        }
        Err(error) => Event::Failed {
            id: q.request.id,
            error,
        },
    };
    shared.push_event(&mut state, event);
    drop(state);
    shared.schedule.notify_one();
}

/// The file's word for this path, overridden by the response's headers and
/// then its `<meta>`, as TDMRep orders them.
fn page_reserved(state: &State, q: &Queued, response: &Response) -> bool {
    let from_file = match state.origins.get(&q.origin).map(|o| &o.tdm) {
        Some(Gate::Ready(Some(f))) => f.reservation(&q.path),
        _ => Reservation::default(),
    };
    let headers = tdmrep::from_headers(
        response
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    let html = tdmrep::from_html(&response.body);
    from_file
        .overridden_by(headers)
        .overridden_by(html)
        .reserved
        == Some(true)
}

/// AutoThrottle: aim for `latency / target_concurrency` between requests,
/// moving halfway there each response. Only a successful response may lower
/// the delay; a 429 doubles it.
fn adjust_delay(
    s: &CrawlSettings,
    current: Duration,
    latency: Duration,
    status: Option<u16>,
) -> Duration {
    let target = latency.div_f64(s.target_concurrency.max(0.01));
    let next = match status {
        Some(429) => current.saturating_mul(2).max(Duration::from_millis(500)),
        Some(200) => (current + target) / 2,
        _ => current.max((current + target) / 2),
    };
    next.clamp(s.min_delay, s.max_delay)
}

async fn check_robots(shared: Arc<Shared>, origin: String) {
    let url = format!("{origin}/robots.txt");
    let robots = match shared.fetcher.get(&url, &[]).await {
        // The parser reads at most robots::MAX_BYTES of it.
        Ok(r) if (200..300).contains(&r.status) => Robots::parse(&r.text()),
        // RFC 9309: an unavailable file (4xx) means no rules.
        Ok(r) if (400..500).contains(&r.status) => Robots::allow_all(),
        // An unreachable file (5xx, or no answer) means everything is off
        // limits for now.
        _ => Robots::disallow_all(),
    };
    let mut state = shared.lock();
    state.origins.entry(origin).or_default().robots = Gate::Ready(robots);
    state.gate_fetches -= 1;
    drop(state);
    shared.schedule.notify_one();
    shared.events.notify_waiters();
}

async fn check_tdm(shared: Arc<Shared>, origin: String) {
    let url = format!("{origin}/.well-known/tdmrep.json");
    let file = match shared.fetcher.get(&url, &[]).await {
        Ok(r) if r.status == 200 => TdmFile::parse(&r.text()),
        _ => None,
    };
    let mut state = shared.lock();
    state.origins.entry(origin).or_default().tdm = Gate::Ready(file);
    state.gate_fetches -= 1;
    drop(state);
    shared.schedule.notify_one();
    shared.events.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_moves_halfway_to_the_target_and_only_success_lowers_it() {
        let s = CrawlSettings {
            target_concurrency: 2.0,
            ..CrawlSettings::default()
        };
        let ms = Duration::from_millis;
        assert_eq!(adjust_delay(&s, ms(1000), ms(400), Some(200)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(1000), ms(400), Some(500)),
            ms(1000),
            "a failure can't lower it"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(4000), Some(404)),
            ms(1050),
            "but can raise it"
        );
        assert_eq!(adjust_delay(&s, ms(300), ms(10), Some(429)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(50_000), ms(200_000), Some(200)),
            ms(60_000),
            "clamped to the max"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(10), None),
            ms(100),
            "a network error can't lower it"
        );
    }

    #[test]
    fn queue_order_is_priority_then_arrival() {
        let q = |priority, seq| Queued {
            priority,
            seq,
            request: CrawlRequest {
                id: seq,
                url: String::new(),
                priority,
                headers: vec![],
                dont_filter: false,
            },
            origin: String::new(),
            path: String::new(),
        };
        let mut heap: BinaryHeap<Queued> =
            [q(0, 1), q(5, 2), q(0, 3), q(5, 4)].into_iter().collect();
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|q| q.seq)).collect();
        assert_eq!(order, [2, 4, 1, 3]);
    }
}
