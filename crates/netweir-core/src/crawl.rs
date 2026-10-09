//! The crawl engine: a queue per host, robots.txt and TDMRep checks before
//! a site's first request, per-host concurrency and adaptive delays, and
//! deduplication. The caller submits requests and takes events in batches.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netweir_browser::{Browser, Cookie, LaunchOptions, WaitUntil};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::canonical::{Fingerprint, fingerprint};
use crate::checkpoint::{Checkpoint, Pending, Saved};
use crate::classify::{BlockKind, Outcome, classify};
use crate::fetch::{FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Response};
use crate::profile::Profile;
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
    /// Further tries after a server error, a network error, throttling or
    /// a block.
    pub retries: u32,
    /// Retry n waits a random time up to backoff_base * 2^n, capped at
    /// backoff_max ("full jitter").
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Proxies a host moves through when it blocks a session. Empty: the
    /// fetch options' proxy, with a fresh cookie jar each time.
    pub proxies: Vec<String>,
    /// The circuit breaker looks at a host's last `breaker_window`
    /// responses; if more than `breaker_ratio` of them were blocks, the
    /// host pauses for `breaker_pause`.
    pub breaker_window: usize,
    pub breaker_ratio: f64,
    pub breaker_pause: Duration,
    /// A file to keep the crawl's state in, so it resumes after a crash.
    pub checkpoint: Option<std::path::PathBuf>,
    /// Which requests go through Chrome rather than the HTTP client.
    pub browser: BrowserMode,
    /// Chrome pages open at once.
    pub browser_pages: usize,
    /// How Chrome is started, the first time a request needs it.
    pub browser_launch: LaunchOptions,
}

/// Which requests a crawl fetches in Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserMode {
    /// Only those that ask for it.
    #[default]
    Off,
    /// Those that ask, and any still blocked after its retries, once.
    OnBlock,
    /// Every request.
    Always,
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
            retries: 3,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            proxies: Vec::new(),
            breaker_window: 50,
            breaker_ratio: 0.3,
            breaker_pause: Duration::from_secs(300),
            checkpoint: None,
            browser: BrowserMode::Off,
            browser_pages: 4,
            browser_launch: LaunchOptions::default(),
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
    /// Fetch it in Chrome.
    pub browser: bool,
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
    Fetched {
        id: u64,
        response: Response,
        /// For a request fetched in Chrome, the page, still open.
        page: Option<LivePage>,
    },
    Failed {
        id: u64,
        error: FetchError,
    },
    Dropped {
        id: u64,
        reason: DropReason,
    },
    /// Still blocked after every retry and a new session for each.
    Blocked {
        id: u64,
        vendor: String,
        kind: BlockKind,
        response: Response,
    },
    /// The circuit breaker paused a host that kept blocking.
    Paused {
        host: String,
        pause: Duration,
    },
    /// Something the caller should hear about, once.
    Warning {
        message: String,
    },
}

/// A page still open in Chrome after a browser fetch. Closing it, or
/// dropping the last clone, closes the page and frees its place among
/// `browser_pages`.
#[derive(Clone)]
pub struct LivePage {
    inner: Arc<Live>,
}

struct Live {
    page: netweir_browser::Page,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    runtime: tokio::runtime::Handle,
    /// Told when the page's slot frees, so a browser request waiting for
    /// one can start.
    shared: std::sync::Weak<Shared>,
}

impl Live {
    fn free(&self) {
        let freed = self
            .permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if freed && let Some(shared) = self.shared.upgrade() {
            shared.schedule.notify_one();
        }
    }
}

impl LivePage {
    pub fn page(&self) -> &netweir_browser::Page {
        &self.inner.page
    }

    pub async fn close(&self) {
        let _ = self.inner.page.close().await;
        self.inner.free();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.free();
        if !self.page.is_closed() {
            let page = self.page.clone();
            self.runtime.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

impl std::fmt::Debug for LivePage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LivePage({})", self.inner.page.url())
    }
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
    pub retries: u64,
    /// Responses that were blocks, retried or not.
    pub blocked: u64,
    /// 429s, and 503s with Retry-After.
    pub throttled: u64,
    pub sessions_replaced: u64,
    pub breaker_trips: u64,
    /// Pages fetched in Chrome.
    pub browser_fetches: u64,
    /// Requests blocked over HTTP that Chrome got through.
    pub browser_unblocked: u64,
}

/// A crawl in progress. Dropping it stops the scheduler; fetches already
/// under way finish on their own.
pub struct Crawler {
    shared: Arc<Shared>,
}

struct Shared {
    fetcher: Fetcher,
    checkpoint: Option<Checkpoint>,
    /// What the checkpoint held at the start, until the caller takes it.
    saved: Mutex<Option<Saved>>,
    /// What new sessions are made from.
    options: FetchOptions,
    /// The next entry of `settings.proxies` a new session takes.
    proxy_turn: std::sync::atomic::AtomicUsize,
    settings: CrawlSettings,
    state: Mutex<State>,
    /// Woken when there may be something to schedule.
    schedule: Notify,
    /// Woken when an event arrives or the crawl may have finished.
    events: Notify,
    /// Chrome, started the first time a request needs it.
    /// A Chrome that dies is started again for the next request that
    /// needs one; one that can't start isn't tried again.
    browser: tokio::sync::Mutex<Option<Result<Browser, String>>>,
    /// Places for open Chrome pages.
    pages: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}

#[derive(Default)]
struct State {
    seq: u64,
    /// Each saved request's row in the checkpoint, by id.
    rows: HashMap<u64, i64>,
    next_row: i64,
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
    /// Whether the crawl was told no HTTP profile matches its Chrome.
    warned_profile: bool,
    /// Whether the crawl was told a blocked request couldn't go to Chrome.
    warned_browser: bool,
    /// Hosts whose next request waits for a free Chrome page.
    page_waiters: Vec<String>,
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
    /// This host's own session after a block; until then, the crawl's.
    session: Option<Fetcher>,
    /// Whether each of the last responses was a block, newest last.
    recent: VecDeque<bool>,
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
    /// Tries already made.
    attempts: u32,
    /// Fetch it in Chrome.
    in_browser: bool,
    /// Blocked over HTTP and sent to Chrome: what the block was, for
    /// on_block if Chrome doesn't get through either.
    blocked: Option<Box<(String, BlockKind, Response)>>,
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
        let mut state = State::default();
        let (checkpoint, saved) = match &settings.checkpoint {
            Some(path) => {
                let (cp, mut saved) =
                    Checkpoint::open(path).map_err(|e| FetchError::invalid(e.to_string()))?;
                state.seen = std::mem::take(&mut saved.seen);
                state.next_row = saved.next_row;
                (Some(cp), Some(saved))
            }
            None => (None, None),
        };
        let settings_pages = settings.browser_pages.max(1);
        let shared = Arc::new(Shared {
            fetcher: Fetcher::new(fetch.clone())?,
            checkpoint,
            saved: Mutex::new(saved),
            options: fetch,
            proxy_turn: std::sync::atomic::AtomicUsize::new(0),
            settings,
            state: Mutex::new(state),
            schedule: Notify::new(),
            events: Notify::new(),
            browser: tokio::sync::Mutex::new(None),
            pages: Arc::new(Semaphore::new(settings_pages)),
            runtime: tokio::runtime::Handle::current(),
        });
        tokio::spawn(schedule_loop(shared.clone()));
        Ok(Crawler { shared })
    }

    pub fn submit(&self, request: CrawlRequest) -> Submitted {
        self.submit_with(request, "{}")
    }

    /// `submit`, saving `payload` (the caller's own part of the request, as
    /// JSON) with it in the checkpoint, if there is one.
    pub fn submit_with(&self, request: CrawlRequest, payload: &str) -> Submitted {
        self.admit(request, Persist::New(payload))
    }

    /// Queues a request read back from the checkpoint (`saved().pending`).
    /// It was accepted before, so it isn't checked again.
    pub fn resubmit(&self, request: CrawlRequest, row: i64) -> Submitted {
        self.admit(request, Persist::Restored(row))
    }

    /// What the checkpoint held when the crawl started; once.
    pub fn saved(&self) -> Option<Saved> {
        self.shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The caller has finished with these requests' events: they are done
    /// for good, and a resumed crawl won't fetch them again.
    pub fn done(&self, ids: &[u64]) {
        let Some(cp) = &self.shared.checkpoint else {
            return;
        };
        let mut state = self.shared.lock();
        for id in ids {
            if let Some(row) = state.rows.remove(id) {
                cp.done(row);
            }
        }
    }

    /// Records an item as delivered, by its stable id.
    pub fn item_done(&self, id: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.item(id);
        }
    }

    /// Records items as delivered and saves the caller's counters (JSON)
    /// in one commit.
    pub fn settle(&self, items: Vec<String>, counters: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.settle(items, counters);
        }
    }

    /// Saves the caller's counters (JSON) alongside the crawl.
    pub fn save_counters(&self, json: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.counters(json);
        }
    }

    /// Waits until every checkpoint write so far is on disk; an error if
    /// one of them failed.
    pub fn flush(&self) -> Result<(), String> {
        match &self.shared.checkpoint {
            Some(cp) => cp.flush(),
            None => Ok(()),
        }
    }

    /// Commits the checkpoint and closes its file, for when the crawl is
    /// over. Checkpoint writes after this are dropped.
    pub fn close(&self) -> Result<(), String> {
        let flushed = self.flush();
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        flushed
    }

    /// The Chrome the crawl is using, if it has started one.
    pub async fn browser(&self) -> Option<Browser> {
        match &*self.shared.browser.lock().await {
            Some(Ok(browser)) => Some(browser.clone()),
            _ => None,
        }
    }

    /// Closes Chrome, if the crawl started it.
    pub async fn close_browser(&self) {
        if let Some(browser) = self.browser().await {
            let _ = browser.close().await;
        }
    }

    fn admit(&self, request: CrawlRequest, persist: Persist<'_>) -> Submitted {
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
        let restored = matches!(persist, Persist::Restored(_));
        if !state.seen.insert(fp) && !request.dont_filter && !restored {
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
        let saving = self.shared.checkpoint.as_ref().map(|cp| {
            let row = match persist {
                Persist::Restored(row) => row,
                Persist::New(payload) => {
                    let row = state.next_row;
                    state.next_row += 1;
                    cp.seen(fp);
                    cp.request(Pending {
                        row,
                        url: request.url.clone(),
                        priority: request.priority,
                        headers: request.headers.clone(),
                        dont_filter: request.dont_filter,
                        depth: request.depth,
                        payload: payload.to_string(),
                    });
                    row
                }
            };
            (request.id, row)
        });
        if !enqueue(settings, &mut state, request, &url, 0) {
            return Submitted::Invalid;
        }
        if let Some((id, row)) = saving {
            state.rows.insert(id, row);
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
        // The scheduler keeps the shared state alive a while longer; the
        // checkpoint file shouldn't stay open with it.
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        let shared = self.shared.clone();
        self.shared.runtime.spawn(async move {
            if let Some(Ok(browser)) = &*shared.browser.lock().await {
                let _ = browser.close().await;
            }
        });
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
            Event::Blocked { .. } | Event::Paused { .. } | Event::Warning { .. } => {}
        }
        state.events.push_back(event);
        self.events.notify_waiters();
    }
}

/// Whether a submitted request is new to the checkpoint or read back from it.
enum Persist<'a> {
    New(&'a str),
    Restored(i64),
}

/// What the scheduler decided to do with a host's next request.
enum Action {
    /// With a Chrome page slot for a browser request.
    Fetch(Box<Queued>, Fetcher, Option<OwnedSemaphorePermit>),
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
                Action::Fetch(q, _, Some(permit)) => {
                    tokio::spawn(browser_fetch(s.clone(), *q, permit));
                }
                Action::Fetch(q, session, None) => {
                    tokio::spawn(fetch(s.clone(), *q, session));
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
    let in_browser = request.browser || settings.browser == BrowserMode::Always;
    let queued = Queued {
        priority: request.priority,
        seq: state.seq,
        request,
        host: host.clone(),
        origin,
        path,
        hops,
        attempts: 0,
        in_browser,
        blocked: None,
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
            session: None,
            recent: VecDeque::new(),
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
    // A Chrome page has freed: the hosts that waited for one go first.
    if !state.page_waiters.is_empty() && shared.pages.available_permits() > 0 {
        for name in std::mem::take(&mut state.page_waiters) {
            state.make_ready(&name);
        }
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
            // A browser request starts only with a Chrome page to use, so
            // it never holds a slot other requests could have had while it
            // waits for one.
            let permit = if host.queue.peek().is_some_and(|q| q.in_browser) {
                match shared.pages.clone().try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        if !state.page_waiters.contains(&name) {
                            state.page_waiters.push(name.clone());
                        }
                        break;
                    }
                }
            } else {
                None
            };
            let q = host.queue.pop().expect("peeked above");
            host.in_flight += 1;
            host.next_at = now + host.delay.max(host.floor);
            state.stats.queued -= 1;
            state.stats.in_flight += 1;
            let session = host
                .session
                .clone()
                .unwrap_or_else(|| shared.fetcher.clone());
            actions.push(Action::Fetch(Box::new(q), session, permit));
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

async fn fetch(shared: Arc<Shared>, mut q: Queued, session: Fetcher) {
    let started = Instant::now();
    let (url, headers, hops) = (q.request.url.clone(), q.request.headers.clone(), q.hops);
    let result = guarded(
        async move { session.hop(&url, &headers, hops).await },
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
    let outcome = match &result {
        Ok(Hop::Done(r)) => Some(classify(r)),
        _ => None,
    };
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
        } else if matches!(outcome, Some(Outcome::Ok)) {
            // No throttle to bring a delay a block added back down: each
            // good response halves it.
            host.delay = (host.delay / 2).max(settings.min_delay);
        }
    }
    if let Some(outcome) = &outcome {
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    let can_retry = q.attempts < settings.retries;
    let event = match (result, outcome) {
        (Ok(Hop::Done(response)), Some(Outcome::Blocked { vendor, kind })) => {
            state.stats.blocked += 1;
            if can_retry {
                new_session(&shared, &mut state, &q.host);
            }
            slow_down(settings, &mut state, &q.host, None, true);
            if can_retry {
                retry(settings, &mut state, q);
                None
            } else if settings.browser == BrowserMode::OnBlock {
                // One more go, in Chrome, once the host's delay is up. Not a
                // retry: the retries are spent.
                q.in_browser = true;
                q.blocked = Some(Box::new((vendor, kind, response)));
                requeue(&mut state, q, Duration::ZERO);
                None
            } else {
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::Throttled { retry_after })) => {
            state.stats.throttled += 1;
            // With the throttle on, it has already doubled the delay for
            // this 429.
            slow_down(
                settings,
                &mut state,
                &q.host,
                retry_after,
                !settings.throttle,
            );
            if can_retry {
                retry(settings, &mut state, q);
                None
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::HttpError(status)))
            if can_retry && RETRY_STATUSES.contains(&status) =>
        {
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        (Ok(Hop::Done(response)), _) => {
            if settings.obey_tdmrep && page_reserved(&state, &q, &response) {
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        // The next hop is queued like a new request, on its own host, so it
        // goes through that host's robots.txt, TDMRep and limits. Its URL
        // counts as seen, but is fetched even if seen: this request has to
        // end somewhere.
        (Ok(Hop::Redirect(next)), _) => {
            if let Some(fp) = fingerprint("GET", &next) {
                state.seen.insert(fp);
                if let Some(cp) = &shared.checkpoint {
                    cp.seen(fp);
                }
            }
            let id = q.request.id;
            q.attempts = 0;
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
        (Err(error), _) if can_retry && transient(&error) => {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => Some(Event::Failed {
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

/// How long a challenge page gets to pass and move on in Chrome.
const CHALLENGE_WAIT: Duration = Duration::from_secs(20);

/// What `render` brings back: the response, the open page, its cookies
/// when asked for, how long the page took to load, and Chrome's version.
type Rendered = (Response, LivePage, Vec<Cookie>, Duration, String);

/// Fetches `q` in Chrome, in the page slot `permit` holds, and deals with
/// the outcome as `fetch` does for the HTTP client. A page that's still
/// blocked isn't retried in Chrome.
async fn browser_fetch(shared: Arc<Shared>, mut q: Queued, permit: OwnedSemaphorePermit) {
    let timeout = shared.options.timeout;
    // A Chrome that stops answering mid-page mustn't hold the request, and
    // its place in the crawl, for good.
    let limit = timeout.saturating_mul(2) + CHALLENGE_WAIT;
    let work = render(
        shared.clone(),
        q.request.url.clone(),
        q.blocked.is_some(),
        permit,
    );
    let rendered = guarded(
        async move {
            tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
                Err(FetchError {
                    kind: FetchErrorKind::Timeout,
                    message: format!("Chrome took more than {}s", limit.as_secs()),
                })
            })
        },
        |why| {
            Err(FetchError {
                kind: FetchErrorKind::Other,
                message: format!("the browser fetch failed inside netweir: {why}"),
            })
        },
    )
    .await;
    let settings = &shared.settings;
    let outcome = rendered.as_ref().ok().map(|(r, ..)| classify(r));
    let mut state = shared.lock();
    state.stats.in_flight -= 1;
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle
            && let Ok((r, _, _, latency, _)) = &rendered
        {
            host.delay = adjust_delay(settings, host.delay, *latency, Some(r.status));
        }
    }
    if let Some(outcome) = &outcome {
        state.stats.browser_fetches += 1;
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    // Pages to close once the lock is released.
    let mut discard = None;
    let event = match (rendered, outcome) {
        (Ok((response, page, ..)), Some(Outcome::Blocked { vendor, kind })) => {
            discard = Some(page);
            state.stats.blocked += 1;
            slow_down(settings, &mut state, &q.host, None, true);
            Some(Event::Blocked {
                id: q.request.id,
                vendor,
                kind,
                response,
            })
        }
        // A server error is tried again, as the HTTP client would, unless
        // this is a blocked request's one go in Chrome.
        (Ok((response, page, ..)), Some(Outcome::HttpError(status)))
            if q.blocked.is_none()
                && q.attempts < settings.retries
                && RETRY_STATUSES.contains(&status) =>
        {
            discard = Some(page);
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        (Ok((response, page, cookies, _, version)), outcome) => {
            // Chrome follows redirects itself: where it ended up counts as
            // seen, as a redirect the HTTP client followed would.
            if let Some(fp) = fingerprint("GET", &response.url)
                && state.seen.insert(fp)
                && let Some(cp) = &shared.checkpoint
            {
                cp.seen(fp);
            }
            if q.blocked.is_some() && matches!(outcome, Some(Outcome::Ok)) {
                state.stats.browser_unblocked += 1;
                if let Some(message) = hand_back(&shared, &mut state, &q.host, &cookies, &version) {
                    shared.push_event(&mut state, Event::Warning { message });
                }
            }
            if settings.obey_tdmrep && page_reserved(&state, &q, &response) {
                discard = Some(page);
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: Some(page),
                })
            }
        }
        (Err(error), _) if q.attempts < settings.retries && transient(&error) => {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => match q.blocked.take() {
            // Chrome didn't get anywhere: it's still the block it was. Say
            // why once, or escalation would fail without a word.
            Some(blocked) => {
                if !state.warned_browser {
                    state.warned_browser = true;
                    let message = format!(
                        "a blocked request couldn't be tried in Chrome, so on_block gets it: {}",
                        error.message
                    );
                    shared.push_event(&mut state, Event::Warning { message });
                }
                let (vendor, kind, response) = *blocked;
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
            None => Some(Event::Failed {
                id: q.request.id,
                error,
            }),
        },
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
    drop(state);
    drop(discard);
    shared.schedule.notify_one();
}

/// Loads `url` in a new Chrome page and reads it back. A challenge page is
/// given time to pass and move on. With `cookies`, also returns the
/// page's cookies.
async fn render(
    shared: Arc<Shared>,
    url: String,
    cookies: bool,
    permit: OwnedSemaphorePermit,
) -> Result<Rendered, FetchError> {
    let browser = {
        let mut slot = shared.browser.lock().await;
        let dead = matches!(&*slot, Some(Ok(b)) if b.is_closed());
        if slot.is_none() || dead {
            let launched = Browser::launch(browser_launch(&shared)).await;
            *slot = Some(launched.map_err(|e| e.to_string()));
        }
        slot.clone().expect("set above")
    }
    .map_err(|e| FetchError {
        kind: FetchErrorKind::Other,
        message: format!("can't start Chrome: {e}"),
    })?;
    let page = browser.new_page().await.map_err(browser_error)?;
    let live = LivePage {
        inner: Arc::new(Live {
            page: page.clone(),
            permit: Mutex::new(Some(permit)),
            runtime: shared.runtime.clone(),
            shared: Arc::downgrade(&shared),
        }),
    };
    let timeout = shared.options.timeout;
    let started = Instant::now();
    let mut document = page
        .goto(&url, WaitUntil::Load, Some(timeout))
        .await
        .map_err(browser_error)?;
    let latency = started.elapsed();
    let deadline = Instant::now() + CHALLENGE_WAIT.min(timeout);
    let response = loop {
        // A script may already have moved the page on (a challenge that
        // passes at once does): read the newest document, once loaded.
        if page.response().loader != document.loader {
            let left = deadline.saturating_duration_since(Instant::now());
            match page
                .wait_for_navigation(&document, WaitUntil::Load, Some(left))
                .await
            {
                Ok(next) => document = next,
                Err(netweir_browser::Error::Timeout(_)) => {}
                Err(e) => return Err(browser_error(e)),
            }
        }
        let response = rendered(&document, page.content().await.map_err(browser_error)?);
        let challenged = matches!(
            classify(&response),
            Outcome::Blocked {
                kind: BlockKind::Challenge,
                ..
            }
        );
        let left = deadline.saturating_duration_since(Instant::now());
        if !challenged || left.is_zero() {
            break response;
        }
        match page
            .wait_for_navigation(&document, WaitUntil::Load, Some(left))
            .await
        {
            Ok(next) => document = next,
            // Out of time. One last look, for a challenge that swaps the
            // page's content without navigating.
            Err(netweir_browser::Error::Timeout(_)) => {
                break rendered(
                    &page.response(),
                    page.content().await.map_err(browser_error)?,
                );
            }
            Err(e) => return Err(browser_error(e)),
        }
    };
    let cookies = if cookies {
        page.cookies().await.map_err(browser_error)?
    } else {
        Vec::new()
    };
    Ok((
        response,
        live,
        cookies,
        latency,
        browser.version().to_string(),
    ))
}

/// How to start Chrome for this crawl: through the crawl's proxy, login
/// and all, so the cookies Chrome earns come from the address the HTTP
/// client uses.
fn browser_launch(shared: &Shared) -> LaunchOptions {
    let mut launch = shared.settings.browser_launch.clone();
    if launch.proxy.is_none() {
        launch.proxy = shared.options.proxy.clone();
    }
    if launch.brands.is_none() {
        launch.brands = Some(Arc::new(Profile::chrome_brands));
    }
    launch
}

/// A Response for a page Chrome rendered: the document's status and
/// headers, and the HTML as it is now. The body is UTF-8 whatever the
/// server sent, so the content type says so, and nothing about the body's
/// original encoding or length still applies.
fn rendered(document: &netweir_browser::Response, html: String) -> Response {
    let mut headers: Vec<(String, String)> = document
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "content-encoding" | "content-length" | "transfer-encoding"
            )
        })
        .collect();
    let mime = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .and_then(|(_, v)| v.split(';').next())
        .map_or("text/html", str::trim)
        .to_string();
    headers.retain(|(k, _)| k != "content-type");
    headers.push(("content-type".into(), format!("{mime}; charset=utf-8")));
    Response {
        url: document.url.clone(),
        status: document.status,
        version: "browser",
        headers,
        body: html.into(),
    }
}

fn browser_error(e: netweir_browser::Error) -> FetchError {
    use netweir_browser::Error;
    let kind = match &e {
        Error::Timeout(_) => FetchErrorKind::Timeout,
        Error::Navigation(m) if m.contains("TIMED_OUT") => FetchErrorKind::Timeout,
        Error::Navigation(m) if m.contains("CERT") || m.contains("SSL") => FetchErrorKind::Tls,
        Error::Navigation(m)
            if [
                "CONNECTION",
                "NAME_NOT_RESOLVED",
                "ADDRESS",
                "INTERNET_DISCONNECTED",
                "PROXY",
            ]
            .iter()
            .any(|n| m.contains(n)) =>
        {
            FetchErrorKind::Connect
        }
        _ => FetchErrorKind::Other,
    };
    FetchError {
        kind,
        message: format!("in Chrome: {e}"),
    }
}

/// Gives the host an HTTP session holding the cookies Chrome earned, so
/// its next requests don't need Chrome. A site may tie those cookies to
/// the browser they were issued to, so the session looks like the same
/// Chrome when netweir has its profile. When it doesn't, the session keeps
/// the crawl's profile and a warning (returned, once) says so.
fn hand_back(
    shared: &Shared,
    state: &mut State,
    host: &str,
    cookies: &[Cookie],
    version: &str,
) -> Option<String> {
    let major = version
        .trim_start_matches("Chrome/")
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut options = shared.options.clone();
    let mut warning = None;
    if !options
        .profile
        .name
        .starts_with(&format!("chrome-{major}-"))
    {
        match Profile::for_chrome(&major) {
            Some(profile) => options.profile = profile,
            None if !state.warned_profile => {
                state.warned_profile = true;
                warning = Some(format!(
                    "the installed {version} has no matching netweir profile, so cookies it earned \
                     go to requests that look like {}; a site may not accept them",
                    options.profile.name
                ));
            }
            None => {}
        }
    }
    if let (Ok(fetcher), Some(h)) = (Fetcher::new(options), state.hosts.get_mut(host)) {
        fetcher.add_cookies(cookies);
        h.session = Some(fetcher);
        state.stats.sessions_replaced += 1;
    }
    warning
}

/// Server errors worth another try: timeouts and overloaded or unreachable
/// upstreams (522 and 524 are Cloudflare's).
const RETRY_STATUSES: [u16; 7] = [408, 500, 502, 503, 504, 522, 524];

/// Network failures that may pass on another try.
fn transient(error: &FetchError) -> bool {
    matches!(
        error.kind,
        FetchErrorKind::Timeout
            | FetchErrorKind::Connect
            | FetchErrorKind::Body
            | FetchErrorKind::Other
    )
}

/// Puts `q` back on its host's queue, after a backoff.
fn retry(settings: &CrawlSettings, state: &mut State, mut q: Queued) {
    q.attempts += 1;
    state.stats.retries += 1;
    let cap = settings
        .backoff_base
        .saturating_mul(1u32 << q.attempts.min(20))
        .min(settings.backoff_max);
    let wait = cap.mul_f64(random_fraction(q.seq));
    requeue(state, q, wait);
}

/// Puts `q` back on its host's queue, to start no sooner than `wait`.
fn requeue(state: &mut State, mut q: Queued, wait: Duration) {
    let host = q.host.clone();
    state.seq += 1;
    q.seq = state.seq;
    if let Some(h) = state.hosts.get_mut(&host) {
        h.next_at = h.next_at.max(Instant::now() + wait);
        h.queue.push(q);
        state.stats.queued += 1;
    }
    state.make_ready(&host);
}

/// A number in [0, 1) that differs per call; for jitter, not secrets.
fn random_fraction(salt: u64) -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Blocks and throttling count against a host's pace: its delay doubles
/// (to at least half a second), and a Retry-After pushes its next request
/// back.
fn slow_down(
    settings: &CrawlSettings,
    state: &mut State,
    host: &str,
    wait: Option<Duration>,
    double: bool,
) {
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    if double {
        h.delay = h
            .delay
            .saturating_mul(2)
            .max(Duration::from_millis(500))
            .min(settings.max_delay);
    }
    // From now, not from the host's next turn, which was set before.
    let wait = wait
        .unwrap_or_default()
        .min(settings.max_delay)
        .max(h.delay);
    h.next_at = h.next_at.max(Instant::now() + wait);
}

/// Replaces the host's session: a fresh cookie jar, and the next proxy if
/// there are several.
fn new_session(shared: &Shared, state: &mut State, host: &str) {
    let mut options = shared.options.clone();
    let proxies = &shared.settings.proxies;
    if !proxies.is_empty() {
        let turn = shared
            .proxy_turn
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        options.proxy = Some(proxies[turn % proxies.len()].clone());
    }
    if let (Ok(fetcher), Some(h)) = (Fetcher::new(options), state.hosts.get_mut(host)) {
        h.session = Some(fetcher);
        state.stats.sessions_replaced += 1;
    }
}

/// Responses the breaker needs to have seen from a host before it judges.
const BREAKER_MIN_SEEN: usize = 10;

/// Records whether the host's latest response was a block, and pauses the
/// host when too many of its recent ones were.
fn breaker(shared: &Shared, state: &mut State, host: &str, blocked: bool) {
    let settings = &shared.settings;
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    h.recent.push_back(blocked);
    while h.recent.len() > settings.breaker_window {
        h.recent.pop_front();
    }
    let blocks = h.recent.iter().filter(|b| **b).count();
    // It judges once it has seen a few responses, not only once the window
    // is full: a site that blocks from its first answer pauses quickly.
    let enough = settings.breaker_window.min(BREAKER_MIN_SEEN);
    if settings.breaker_window == 0
        || h.recent.len() < enough
        || blocks as f64 <= settings.breaker_ratio * h.recent.len() as f64
    {
        return;
    }
    h.recent.clear();
    h.next_at = h.next_at.max(Instant::now() + settings.breaker_pause);
    state.stats.breaker_trips += 1;
    shared.push_event(
        state,
        Event::Paused {
            host: host.to_string(),
            pause: settings.breaker_pause,
        },
    );
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
                browser: false,
            },
            host: String::new(),
            origin: String::new(),
            path: String::new(),
            hops: 0,
            attempts: 0,
            in_browser: false,
            blocked: None,
        };
        let mut heap: BinaryHeap<Queued> =
            [q(0, 1), q(5, 2), q(0, 3), q(5, 4)].into_iter().collect();
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|q| q.seq)).collect();
        assert_eq!(order, [2, 4, 1, 3]);
    }
}
