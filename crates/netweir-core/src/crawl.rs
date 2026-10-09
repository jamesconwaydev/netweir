//! The crawl engine: a queue per host, robots.txt and TDMRep checks before
//! a site's first request, per-host concurrency and adaptive delays, and
//! deduplication. The caller submits requests and takes events in batches.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use url::Url;

use crate::canonical::{Fingerprint, fingerprint};
use crate::fetch::{FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Response};
use crate::robots::Robots;
use crate::tdmrep::{self, Reservation, TdmFile};
use crate::traps;

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
    /// Links followed from a start page beyond which requests are refused.
    pub max_depth: Option<u32>,
    /// Requests accepted for one host beyond which more are refused.
    pub max_pages_per_domain: Option<u64>,
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
            max_depth: None,
            max_pages_per_domain: None,
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
    /// Links followed to get here from a start page (which is 0).
    pub depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// An equal request was already submitted.
    Duplicate,
    /// Not an http(s) URL.
    Invalid,
    /// Deeper than `max_depth`.
    TooDeep,
    /// The path repeats itself, as crawler traps do.
    Trap,
    /// The host already has `max_pages_per_domain` requests.
    DomainFull,
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
    pub skipped_depth: u64,
    pub skipped_traps: u64,
    pub skipped_domain_full: u64,
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
    /// Hosts that may be able to start a request. The scheduler visits only
    /// these, so its work follows what changed, not how many hosts exist.
    ready: VecDeque<String>,
    /// Hosts waiting out their delay, soonest first.
    timers: BinaryHeap<Reverse<(Instant, String)>>,
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
    /// In `State::ready`.
    listed: bool,
    /// When this host's timer is set for, so it is set once.
    timer: Option<Instant>,
    /// Requests submitted for this host, for `max_pages_per_domain`.
    accepted: u64,
}

#[derive(Default)]
struct Origin {
    robots: Gate<Robots>,
    tdm: Gate<Option<TdmFile>>,
    /// Hosts waiting for this origin's robots.txt or TDMRep file.
    waiting: Vec<String>,
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
    host: String,
    origin: String,
    path: String,
    /// Redirects followed so far to get to `request.url`.
    hops: usize,
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
        let Some(fp) = fingerprint("GET", &request.url) else {
            return Submitted::Invalid;
        };
        let Ok(parsed) = Url::parse(&request.url) else {
            return Submitted::Invalid;
        };
        let settings = &self.shared.settings;
        let mut state = self.shared.lock();
        if settings.max_depth.is_some_and(|max| request.depth > max) {
            state.stats.skipped_depth += 1;
            return Submitted::TooDeep;
        }
        if traps::repeats(parsed.path()) {
            state.stats.skipped_traps += 1;
            return Submitted::Trap;
        }
        if !state.seen.insert(fp) && !request.dont_filter {
            state.stats.duplicates += 1;
            return Submitted::Duplicate;
        }
        let host = parsed.host_str().unwrap_or_default();
        let accepted = state.hosts.get(host).map_or(0, |h| h.accepted);
        if settings
            .max_pages_per_domain
            .is_some_and(|max| accepted >= max)
        {
            // Not seen after all: the same URL is refused the same way.
            state.seen.remove(&fp);
            state.stats.skipped_domain_full += 1;
            return Submitted::DomainFull;
        }
        let url = request.url.clone();
        if !enqueue(settings, &mut state, request, &url, 0) {
            return Submitted::Invalid;
        }
        if let Some(h) = parsed.host_str().and_then(|h| state.hosts.get_mut(h)) {
            h.accepted += 1;
        }
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
                let batch = state.events.drain(..n).collect();
                drop(state);
                // Room in the backlog may let the scheduler start more.
                shared.schedule.notify_one();
                return batch;
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
    /// Another host's check of the same origin is under way.
    Wait,
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
                Action::Wait => {}
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

/// Queues `request` for `url` (the request's own URL, or where a redirect
/// led) on its host, and marks the host ready. False if the URL can't be
/// crawled.
fn enqueue(
    settings: &CrawlSettings,
    state: &mut State,
    mut request: CrawlRequest,
    url: &str,
    hops: usize,
) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let (Some(host), true) = (
        parsed.host_str(),
        matches!(parsed.scheme(), "http" | "https"),
    ) else {
        return false;
    };
    let host = host.to_string();
    let origin = parsed.origin().ascii_serialization();
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    request.url = url.to_string();
    state.seq += 1;
    let queued = Queued {
        priority: request.priority,
        seq: state.seq,
        request,
        host: host.clone(),
        origin,
        path,
        hops,
    };
    state
        .hosts
        .entry(host.clone())
        .or_insert_with(|| Host {
            queue: BinaryHeap::new(),
            in_flight: 0,
            next_at: Instant::now(),
            delay: if settings.throttle {
                settings.start_delay
            } else {
                settings.min_delay
            },
            floor: Duration::ZERO,
            listed: false,
            timer: None,
            accepted: 0,
        })
        .queue
        .push(queued);
    state.stats.queued += 1;
    state.make_ready(&host);
    true
}

impl State {
    fn make_ready(&mut self, name: &str) {
        if let Some(host) = self.hosts.get_mut(name)
            && !host.listed
        {
            host.listed = true;
            self.ready.push_back(name.to_string());
        }
    }
}

/// Events held for the reader before the scheduler stops starting fetches,
/// so a slow reader doesn't mean the whole frontier downloaded into memory.
fn backlog_limit(settings: &CrawlSettings) -> usize {
    settings.concurrency.saturating_mul(2)
}

/// Starts everything that may start now; returns what to do and when to
/// look again.
fn plan(
    settings: &CrawlSettings,
    state: &mut State,
    shared: &Shared,
) -> (Vec<Action>, Option<Instant>) {
    let now = Instant::now();
    while let Some(Reverse((at, _))) = state.timers.peek() {
        if *at > now {
            break;
        }
        let Some(Reverse((_, name))) = state.timers.pop() else {
            break;
        };
        if let Some(host) = state.hosts.get_mut(&name) {
            host.timer = None;
        }
        state.make_ready(&name);
    }
    let mut actions = Vec::new();
    'hosts: while let Some(name) = state.ready.front().cloned() {
        // Gate fetches count against the limit like any other request.
        if state.stats.in_flight + state.gate_fetches >= settings.concurrency
            || state.events.len() >= backlog_limit(settings)
        {
            break;
        }
        state.ready.pop_front();
        if let Some(host) = state.hosts.get_mut(&name) {
            host.listed = false;
        }
        loop {
            if state.stats.in_flight + state.gate_fetches >= settings.concurrency {
                // Still has work: back in line for when a slot frees.
                state.make_ready(&name);
                break 'hosts;
            }
            let Some(host) = state.hosts.get(&name) else {
                break;
            };
            let Some(next) = host.queue.peek() else { break };
            let origin = next.origin.clone();
            let path = &next.path;
            // Robots and TDMRep must be known for this origin first.
            let o = state.origins.entry(origin.clone()).or_default();
            let mut gate = None;
            if settings.obey_robots {
                match o.robots {
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        gate = Some(Action::CheckRobots(origin.clone()));
                    }
                    Gate::Fetching => gate = Some(Action::Wait),
                    Gate::Ready(_) => {}
                }
            }
            if gate.is_none() && settings.obey_tdmrep {
                match o.tdm {
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        gate = Some(Action::CheckTdm(origin.clone()));
                    }
                    Gate::Fetching => gate = Some(Action::Wait),
                    Gate::Ready(_) => {}
                }
            }
            if let Some(gate) = gate {
                if !o.waiting.contains(&name) {
                    o.waiting.push(name.clone());
                }
                if !matches!(gate, Action::Wait) {
                    state.gate_fetches += 1;
                    actions.push(gate);
                }
                break;
            }
            let blocked = match (&o.robots, &o.tdm) {
                (Gate::Ready(r), _)
                    if settings.obey_robots && !r.allowed(&settings.robots_agent, path) =>
                {
                    Some(DropReason::Robots)
                }
                (_, Gate::Ready(Some(f)))
                    if settings.obey_tdmrep && f.reservation(path).reserved == Some(true) =>
                {
                    Some(DropReason::TdmReserved)
                }
                _ => None,
            };
            let floor = match &o.robots {
                Gate::Ready(r) => r
                    .crawl_delay(&settings.robots_agent)
                    .unwrap_or_default()
                    .min(settings.max_delay),
                _ => Duration::ZERO,
            };
            let host = state.hosts.get_mut(&name).expect("looked up above");
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
                // Ready again when one of its fetches finishes.
                break;
            }
            if now < host.next_at {
                if host.timer != Some(host.next_at) {
                    host.timer = Some(host.next_at);
                    state.timers.push(Reverse((host.next_at, name.clone())));
                }
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
    let sleep_until = state.timers.peek().map(|Reverse((at, _))| *at);
    (actions, sleep_until)
}

/// Runs `work` as its own task, so a panic inside it (a parser bug on
/// hostile input, say) becomes `fallback` instead of killing the caller
/// before it has done its bookkeeping.
async fn guarded<T: Send + 'static>(
    work: impl std::future::Future<Output = T> + Send + 'static,
    fallback: impl FnOnce(String) -> T,
) -> T {
    match tokio::spawn(work).await {
        Ok(value) => value,
        Err(e) => fallback(e.to_string()),
    }
}

async fn fetch(shared: Arc<Shared>, q: Queued) {
    let started = Instant::now();
    let (fetcher, url, headers, hops) = (
        shared.clone(),
        q.request.url.clone(),
        q.request.headers.clone(),
        q.hops,
    );
    let result = guarded(
        async move { fetcher.fetcher.hop(&url, &headers, hops).await },
        |why| {
            Err(FetchError {
                kind: FetchErrorKind::Other,
                message: format!("the fetch failed inside netweir: {why}"),
            })
        },
    )
    .await;
    let latency = started.elapsed();
    let settings = &shared.settings;
    let mut state = shared.lock();
    state.stats.in_flight -= 1;
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle {
            let status = match &result {
                Ok(Hop::Done(r)) => Some(r.status),
                _ => None,
            };
            host.delay = adjust_delay(settings, host.delay, latency, status);
        }
    }
    state.make_ready(&q.host);
    let event = match result {
        Ok(Hop::Done(response)) => {
            if settings.obey_tdmrep && page_reserved(&state, &q, &response) {
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                })
            }
        }
        // The next hop is queued like a new request, on its own host, so it
        // goes through that host's robots.txt, TDMRep and limits. Its URL
        // counts as seen, but is fetched even if seen: this request has to
        // end somewhere.
        Ok(Hop::Redirect(next)) => {
            if let Some(fp) = fingerprint("GET", &next) {
                state.seen.insert(fp);
            }
            let id = q.request.id;
            if enqueue(settings, &mut state, q.request, &next, q.hops + 1) {
                None
            } else {
                Some(Event::Failed {
                    id,
                    error: FetchError::invalid(format!(
                        "redirect to a URL netweir can't fetch: {next}"
                    )),
                })
            }
        }
        Err(error) => Some(Event::Failed {
            id: q.request.id,
            error,
        }),
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
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
    let (fetcher, url) = (shared.clone(), format!("{origin}/robots.txt"));
    let robots = guarded(
        async move {
            match fetcher.fetcher.get(&url, &[]).await {
                // The parser reads at most robots::MAX_BYTES of it.
                Ok(r) if (200..300).contains(&r.status) => Robots::parse(&r.text()),
                // RFC 9309: an unavailable file (4xx) means no rules.
                Ok(r) if (400..500).contains(&r.status) => Robots::allow_all(),
                // An unreachable file (5xx, or no answer) means everything
                // is off limits for now.
                _ => Robots::disallow_all(),
            }
        },
        // A file netweir can't read is treated as unreachable.
        |_| Robots::disallow_all(),
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().robots = Gate::Ready(robots);
    gate_done(&shared, state, &origin);
}

async fn check_tdm(shared: Arc<Shared>, origin: String) {
    let (fetcher, url) = (shared.clone(), format!("{origin}/.well-known/tdmrep.json"));
    let file = guarded(
        async move {
            match fetcher.fetcher.get(&url, &[]).await {
                Ok(r) if r.status == 200 => TdmFile::parse(&r.text()),
                _ => None,
            }
        },
        |_| None,
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().tdm = Gate::Ready(file);
    gate_done(&shared, state, &origin);
}

/// A gate fetch finished: the hosts waiting on it may go on.
fn gate_done(shared: &Shared, mut state: std::sync::MutexGuard<'_, State>, origin: &str) {
    state.gate_fetches -= 1;
    let waiting = state
        .origins
        .get_mut(origin)
        .map(|o| std::mem::take(&mut o.waiting))
        .unwrap_or_default();
    for host in waiting {
        state.make_ready(&host);
    }
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

    #[tokio::test]
    async fn a_panic_in_guarded_work_becomes_the_fallback() {
        let value = guarded(async { panic!("parser bug") }, |why| {
            assert!(why.contains("panic"), "{why}");
            7
        })
        .await;
        assert_eq!(value, 7);
        assert_eq!(guarded(async { 1 }, |_| 2).await, 1);
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
                depth: 0,
            },
            host: String::new(),
            origin: String::new(),
            path: String::new(),
            hops: 0,
        };
        let mut heap: BinaryHeap<Queued> =
            [q(0, 1), q(5, 2), q(0, 3), q(5, 4)].into_iter().collect();
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|q| q.seq)).collect();
        assert_eq!(order, [2, 4, 1, 3]);
    }
}
