//! A page: one target, one session, its navigation state and its actions.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::browser::{Inner, context_cookies, set_context_cookies, str_of};
use crate::conn::{GONE, Handler};
use crate::{Error, Result};

/// netweir's helpers, installed in each document's isolated world. The
/// page can't see them: the world shares the DOM, not the globals.
const HELPERS: &str = include_str!("helpers.js");

/// The lifecycle event a navigation waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitUntil {
    DomContentLoaded,
    Load,
    /// No network requests for 500 ms.
    NetworkIdle,
}

impl WaitUntil {
    fn event(self) -> &'static str {
        match self {
            WaitUntil::DomContentLoaded => "DOMContentLoaded",
            WaitUntil::Load => "load",
            WaitUntil::NetworkIdle => "networkIdle",
        }
    }
}

impl FromStr for WaitUntil {
    type Err = Error;

    fn from_str(s: &str) -> Result<WaitUntil> {
        match s {
            "domcontentloaded" => Ok(WaitUntil::DomContentLoaded),
            "load" => Ok(WaitUntil::Load),
            "networkidle" => Ok(WaitUntil::NetworkIdle),
            _ => Err(Error::Invalid(format!(
                "wait must be \"domcontentloaded\", \"load\" or \"networkidle\", not {s:?}"
            ))),
        }
    }
}

/// The main document's response to a navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub url: String,
    /// 0 when there was no HTTP response (about:, data:, chrome:).
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Chrome's id for the document's load, which
    /// [`Page::wait_for_navigation`] waits past.
    pub loader: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Seconds since the epoch; -1 for a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    /// "Strict", "Lax" or "None", when set.
    pub same_site: Option<String>,
}

pub(crate) fn cookies_from(list: &Value) -> Vec<Cookie> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|c| Cookie {
            name: str_of(c, "name"),
            value: str_of(c, "value"),
            domain: str_of(c, "domain"),
            path: str_of(c, "path"),
            expires: c["expires"].as_f64().unwrap_or(-1.0),
            http_only: c["httpOnly"].as_bool().unwrap_or(false),
            secure: c["secure"].as_bool().unwrap_or(false),
            same_site: c["sameSite"].as_str().map(str::to_string),
        })
        .collect()
}

pub(crate) fn cookies_to(cookies: &[Cookie]) -> Value {
    cookies
        .iter()
        .map(|c| {
            let mut v = json!({
                "name": c.name,
                "value": c.value,
                "domain": c.domain,
                "path": if c.path.is_empty() { "/" } else { &c.path },
                "httpOnly": c.http_only,
                "secure": c.secure,
            });
            if c.expires >= 0.0 {
                v["expires"] = Value::from(c.expires);
            }
            if let Some(s) = &c.same_site {
                v["sameSite"] = Value::from(s.as_str());
            }
            v
        })
        .collect()
}

/// A page. Cheap to clone; the page stays open until [`Page::close`].
#[derive(Clone)]
pub struct Page {
    inner: Arc<PageInner>,
}

struct PageInner {
    browser: Arc<Inner>,
    target: String,
    session: String,
    context: Option<String>,
    /// Whether closing the page disposes of its context.
    owns_context: bool,
    shared: Arc<Shared>,
    /// The isolated world of the current document: (loader id, context id).
    world: tokio::sync::Mutex<Option<(String, i64)>>,
    closed: AtomicBool,
}

/// What the reader thread updates and the waiters watch.
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    frame: String,
    url: String,
    /// The current document's loader.
    loader: String,
    /// Loaders in the order their documents were committed.
    committed: Vec<String>,
    /// Lifecycle events seen, by loader.
    events: HashMap<String, Vec<String>>,
    responses: HashMap<String, Response>,
    gone: bool,
}

/// How many documents' events and responses are kept.
const KEPT: usize = 16;

impl State {
    fn on(&mut self, method: &str, p: &Value) {
        match method {
            "Page.lifecycleEvent" if p["frameId"] == self.frame.as_str() => {
                let loader = str_of(p, "loaderId");
                self.events
                    .entry(loader)
                    .or_default()
                    .push(str_of(p, "name"));
            }
            "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
                let frame = &p["frame"];
                self.frame = str_of(frame, "id");
                self.url = str_of(frame, "url");
                self.loader = str_of(frame, "loaderId");
                self.committed.push(self.loader.clone());
                if self.committed.len() > KEPT {
                    let old = self.committed.remove(0);
                    self.events.remove(&old);
                    self.responses.remove(&old);
                }
            }
            "Page.navigatedWithinDocument" if p["frameId"] == self.frame.as_str() => {
                self.url = str_of(p, "url");
            }
            "Network.responseReceived"
                if p["type"] == "Document" && p["frameId"] == self.frame.as_str() =>
            {
                let r = &p["response"];
                let headers = r["headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect();
                let response = Response {
                    url: str_of(r, "url"),
                    status: r["status"].as_u64().unwrap_or(0) as u16,
                    headers,
                    loader: str_of(p, "loaderId"),
                };
                // Navigations that never commit (a 204, a download) leave
                // responses behind; only committed ones are kept.
                let committed = &self.committed;
                self.responses.retain(|l, _| committed.contains(l));
                self.responses.insert(str_of(p, "loaderId"), response);
            }
            "Inspector.targetCrashed" | GONE => self.gone = true,
            _ => {}
        }
    }

    /// Whether a document committed after `loader`'s has reached `event`.
    fn reached_after(&self, loader: &str, event: &str) -> bool {
        let Some(at) = self.committed.iter().position(|l| l == loader) else {
            return false;
        };
        self.committed[at + 1..].iter().any(|l| {
            self.events
                .get(l)
                .is_some_and(|e| e.iter().any(|n| n == event))
        })
    }

    /// Whether `loader`'s navigation, or one that replaced it, has reached
    /// `event`.
    fn reached(&self, loader: &str, event: &str) -> bool {
        let from = self.committed.iter().position(|l| l == loader);
        let Some(from) = from else { return false };
        self.committed[from..].iter().any(|l| {
            self.events
                .get(l)
                .is_some_and(|e| e.iter().any(|n| n == event))
        })
    }
}

impl Page {
    pub(crate) async fn open(
        browser: Arc<Inner>,
        context: Option<String>,
        owns_context: bool,
        guard: Option<crate::browser::Guard>,
    ) -> Result<Page> {
        let conn = &browser.conn;
        let mut params = json!({"url": "about:blank"});
        if let Some(id) = &context {
            params["browserContextId"] = Value::from(id.as_str());
        }
        let target = str_of(
            &conn.call("", "Target.createTarget", params).await?,
            "targetId",
        );
        let session = str_of(
            &conn
                .call(
                    "",
                    "Target.attachToTarget",
                    json!({"targetId": target, "flatten": true}),
                )
                .await?,
            "sessionId",
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        });
        let watched = shared.clone();
        // Documents the guard must see first, answered on a task of their
        // own, since the guard is async and the reader thread can't wait.
        let guarded = guard.map(|guard| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
            let (conn, session) = (conn.clone(), session.clone());
            tokio::spawn(async move {
                while let Some((id, url)) = rx.recv().await {
                    let (method, params) = if guard(url).await {
                        ("Fetch.continueRequest", json!({"requestId": id}))
                    } else {
                        (
                            "Fetch.failRequest",
                            json!({"requestId": id, "errorReason": "BlockedByClient"}),
                        )
                    };
                    conn.send(&session, method, params);
                }
            });
            tx
        });
        let fetching = browser.proxy_login.is_some() || guarded.is_some();
        let login = Login {
            conn: conn.clone(),
            session: session.clone(),
            credentials: browser.proxy_login.clone(),
            guarded,
            frame: target.clone(),
        };
        let handler: Handler = Arc::new(move |method, params| {
            match method {
                "Fetch.authRequired" => return login.answer(params),
                "Fetch.requestPaused" => return login.release(params),
                _ => {}
            }
            watched
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .on(method, params);
            watched.changed.notify_waiters();
        });
        conn.on(&session, handler);
        let page = Page {
            inner: Arc::new(PageInner {
                browser: browser.clone(),
                target,
                session,
                context,
                owns_context,
                shared,
                world: tokio::sync::Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        };
        let setup = async {
            page.call("Page.enable", json!({})).await?;
            page.call("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
                .await?;
            page.call("Network.enable", json!({})).await?;
            if fetching {
                // To answer the proxy's challenges, Chrome has every
                // request paused, each released as it comes: a round trip
                // per request, only for a proxy with a login. A guard needs
                // only documents paused. Page script can't see the Fetch
                // domain.
                let pattern = if browser.proxy_login.is_some() {
                    json!({"urlPattern": "*"})
                } else {
                    json!({"urlPattern": "*", "resourceType": "Document"})
                };
                page.call(
                    "Fetch.enable",
                    json!({
                        "handleAuthRequests": browser.proxy_login.is_some(),
                        "patterns": [pattern],
                    }),
                )
                .await?;
            }
            if let Some(identity) = &browser.identity {
                page.call(
                    "Network.setUserAgentOverride",
                    json!({"userAgent": identity.user_agent, "userAgentMetadata": identity.metadata}),
                )
                .await?;
            }
            let tree = page.call("Page.getFrameTree", json!({})).await?;
            let frame = &tree["frameTree"]["frame"];
            let mut state = page.state();
            if state.frame.is_empty() {
                state.frame = str_of(frame, "id");
                state.url = str_of(frame, "url");
                state.loader = str_of(frame, "loaderId");
                let loader = state.loader.clone();
                state.committed.push(loader);
            }
            Ok(())
        }
        .await;
        if let Err(e) = setup {
            let _ = page.close().await;
            return Err(e);
        }
        Ok(page)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(Error::PageClosed);
        }
        self.inner
            .browser
            .conn
            .call(&self.inner.session, method, params)
            .await
    }

    /// Waits until `ready` holds for the page's state, the page goes away
    /// (Closed), or `timeout` runs out (Timeout, saying `what`).
    async fn until(
        &self,
        what: impl Fn() -> String,
        timeout: Duration,
        ready: impl Fn(&State) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let changed = self.inner.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state();
                if ready(&state) {
                    return Ok(());
                }
                if state.gone {
                    return Err(if self.inner.browser.conn.is_closed() {
                        Error::Closed
                    } else {
                        Error::PageClosed
                    });
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(Error::Timeout(what()));
            }
        }
    }

    /// The limit for one wait, at most a year: a longer one would overflow
    /// the clock.
    fn timeout(&self, timeout: Option<Duration>) -> Duration {
        timeout
            .unwrap_or(self.inner.browser.timeout)
            .min(Duration::from_secs(365 * 24 * 3600))
    }

    /// Navigates and waits for `wait`. An HTTP error status is returned,
    /// not raised; a network error is [`Error::Navigation`].
    pub async fn goto(
        &self,
        url: &str,
        wait: WaitUntil,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let deadline = Instant::now() + self.timeout(timeout);
        let navigate = self.call("Page.navigate", json!({"url": url}));
        let r = tokio::time::timeout_at(deadline, navigate)
            .await
            .map_err(|_| Error::Timeout(format!("navigating to {url}")))??;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(Error::Navigation(format!("{url}: {e}")));
        }
        let Some(loader) = r["loaderId"].as_str().map(str::to_string) else {
            // Within the same document (a #fragment): nothing to wait for.
            return Ok(self.response_for(None));
        };
        let event = wait.event();
        self.until(
            || format!("{event} of {url}"),
            deadline.saturating_duration_since(Instant::now()),
            |s| s.reached(&loader, event),
        )
        .await?;
        Ok(self.response_for(Some(&loader)))
    }

    /// The response of the document the page shows now.
    pub fn response(&self) -> Response {
        self.response_for(None)
    }

    /// Waits until a document that replaced `after` (by a script's
    /// redirect, a form, a challenge passing) reaches `wait`, and returns
    /// the response of the document showing then. Returns at once if that
    /// has already happened. Take `after` from [`Page::goto`] or
    /// [`Page::response`] before whatever is expected to navigate.
    pub async fn wait_for_navigation(
        &self,
        after: &Response,
        wait: WaitUntil,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let event = wait.event();
        self.until(
            || format!("{event} of a navigation away from {}", after.url),
            self.timeout(timeout),
            |s| s.reached_after(&after.loader, event),
        )
        .await?;
        Ok(self.response_for(None))
    }

    fn response_for(&self, loader: Option<&str>) -> Response {
        let state = self.state();
        let loader = loader.unwrap_or(&state.loader);
        state
            .responses
            .get(loader)
            .cloned()
            .unwrap_or_else(|| Response {
                url: state.url.clone(),
                status: 0,
                headers: Vec::new(),
                loader: loader.to_string(),
            })
    }

    /// The current URL.
    pub fn url(&self) -> String {
        self.state().url.clone()
    }

    /// The current document's isolated world, made (with the helpers in
    /// it) the first time it's needed.
    async fn world(&self, fresh: bool) -> Result<i64> {
        let mut world = self.inner.world.lock().await;
        let (frame, loader) = {
            let state = self.state();
            (state.frame.clone(), state.loader.clone())
        };
        if let Some((l, id)) = &*world
            && *l == loader
            && !fresh
        {
            return Ok(*id);
        }
        let r = self
            .call("Page.createIsolatedWorld", json!({"frameId": frame}))
            .await?;
        let id = r["executionContextId"].as_i64().unwrap_or_default();
        self.run(Some(id), HELPERS).await?;
        *world = Some((loader, id));
        Ok(id)
    }

    /// Runs `expression` in netweir's isolated world, awaiting a promise.
    async fn helper(&self, expression: &str) -> Result<Value> {
        let id = self.world(false).await?;
        match self.run(Some(id), expression).await {
            // The document changed under it; once more in the new one.
            Err(Error::Protocol { message, .. }) if message.contains("context") => {
                let id = self.world(true).await?;
                self.run(Some(id), expression).await
            }
            other => other,
        }
    }

    /// `Runtime.evaluate`, which works without `Runtime.enable`. With no
    /// context, it runs in the main world.
    async fn run(&self, context: Option<i64>, expression: &str) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
        });
        if let Some(id) = context {
            params["contextId"] = Value::from(id);
        }
        let r = self.call("Runtime.evaluate", params).await?;
        if let Some(details) = r.get("exceptionDetails") {
            let message = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("script error");
            return Err(Error::Script(message.to_string()));
        }
        let result = &r["result"];
        // NaN, Infinity, -0 and BigInts aren't JSON; they come as text.
        if let Some(text) = result["unserializableValue"].as_str() {
            return Ok(Value::from(text.trim_end_matches('n')));
        }
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
    }

    /// Runs `js` in the page's main world: an expression, or a function,
    /// which is called. A promise is awaited. The result comes back as
    /// JSON.
    pub async fn evaluate(&self, js: &str) -> Result<Value> {
        let expression = format!(
            "(async () => {{ const v = ({js}\n); return typeof v === 'function' ? await v() : await v; }})()"
        );
        self.run(None, &expression).await
    }

    /// The document as Chrome now has it, doctype included.
    pub async fn content(&self) -> Result<String> {
        let html = self.helper("__netweir.content()").await?;
        Ok(html.as_str().unwrap_or_default().to_string())
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self
            .helper("document.title")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Waits until the element `selector` finds passes `checks`, scrolling
    /// it into view first if `scroll`. Returns its centre.
    async fn actionable(
        &self,
        action: &str,
        selector: &str,
        checks: &[&str],
        scroll: bool,
        timeout: Option<Duration>,
    ) -> Result<(f64, f64)> {
        let deadline = Instant::now() + self.timeout(timeout);
        let call = format!(
            "__netweir.check({}, {}, {scroll})",
            Value::from(selector),
            json!(checks)
        );
        let mut pause = Duration::from_millis(20);
        loop {
            let r = self.helper(&call).await?;
            if let Some(failed) = r["failed"].as_str() {
                if Instant::now() + pause >= deadline {
                    return Err(Error::Timeout(format!(
                        "{action} {selector}: {}",
                        never(failed)
                    )));
                }
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_millis(200));
                continue;
            }
            return Ok((
                r["x"].as_f64().unwrap_or(0.0),
                r["y"].as_f64().unwrap_or(0.0),
            ));
        }
    }

    /// Clicks the element's centre with the mouse, once it's visible,
    /// still, enabled and not covered.
    pub async fn click(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        let checks = [
            "attached",
            "visible",
            "stable",
            "enabled",
            "receives events",
        ];
        let (x, y) = self
            .actionable("click", selector, &checks, true, timeout)
            .await?;
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )
        .await?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": if kind == "mousePressed" { 1 } else { 0 }, "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Replaces the element's contents with `text`, as typing would.
    pub async fn fill(&self, selector: &str, text: &str, timeout: Option<Duration>) -> Result<()> {
        let checks = ["attached", "visible", "enabled", "editable"];
        self.actionable("fill", selector, &checks, true, timeout)
            .await?;
        let focused = self
            .helper(&format!("__netweir.focus({})", Value::from(selector)))
            .await?;
        if focused != Value::Bool(true) {
            return Err(Error::Script(format!(
                "fill {selector}: it couldn't be focused"
            )));
        }
        if text.is_empty() {
            self.press("Delete").await
        } else {
            self.call("Input.insertText", json!({"text": text})).await?;
            Ok(())
        }
    }

    /// Presses and releases a key in whatever has focus: a character, or
    /// a name such as "Enter", "Tab", "Escape", "Backspace" or "ArrowDown".
    pub async fn press(&self, key: &str) -> Result<()> {
        let k = Key::of(key)?;
        let mut down = json!({
            "type": if k.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": k.key,
            "code": k.code,
            "windowsVirtualKeyCode": k.key_code,
        });
        if let Some(text) = &k.text {
            down["text"] = Value::from(text.as_str());
            down["unmodifiedText"] = Value::from(text.as_str());
        }
        self.call("Input.dispatchKeyEvent", down).await?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.key_code}),
        )
        .await?;
        Ok(())
    }

    /// Waits until `selector` is "attached", "visible", "hidden" (or gone)
    /// or "detached".
    pub async fn wait_for(
        &self,
        selector: &str,
        state: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        if !["attached", "visible", "hidden", "detached"].contains(&state) {
            return Err(Error::Invalid(format!(
                "state must be \"attached\", \"visible\", \"hidden\" or \"detached\", not {state:?}"
            )));
        }
        let deadline = Instant::now() + self.timeout(timeout);
        let call = format!("__netweir.state({})", Value::from(selector));
        let mut pause = Duration::from_millis(20);
        loop {
            let now = self.helper(&call).await?;
            let now = now.as_str().unwrap_or("detached");
            let done = match state {
                "attached" => now != "detached",
                "visible" => now == "visible",
                "hidden" => now != "visible",
                _ => now == "detached",
            };
            if done {
                return Ok(());
            }
            if Instant::now() + pause >= deadline {
                return Err(Error::Timeout(format!(
                    "waiting for {selector} to be {state}"
                )));
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(200));
        }
    }

    /// A PNG of the viewport, or of the whole page.
    pub async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        let mut params = json!({"format": "png"});
        if full_page {
            let metrics = self.call("Page.getLayoutMetrics", json!({})).await?;
            let size = &metrics["cssContentSize"];
            params["captureBeyondViewport"] = Value::Bool(true);
            params["clip"] = json!({
                "x": 0, "y": 0, "scale": 1,
                "width": size["width"].as_f64().unwrap_or(0.0).ceil(),
                "height": size["height"].as_f64().unwrap_or(0.0).ceil(),
            });
        }
        let r = self.call("Page.captureScreenshot", params).await?;
        base64::engine::general_purpose::STANDARD
            .decode(r["data"].as_str().unwrap_or_default())
            .map_err(|e| Error::Protocol {
                method: "Page.captureScreenshot".into(),
                message: e.to_string(),
            })
    }

    /// The cookies of the page's context.
    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        context_cookies(&self.inner.browser, self.inner.context.as_deref()).await
    }

    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        set_context_cookies(&self.inner.browser, self.inner.context.as_deref(), cookies).await
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Relaxed) || self.state().gone
    }

    /// Closes the page, and its context if the page has one of its own.
    pub async fn close(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let conn = &self.inner.browser.conn;
        let closed = conn
            .call(
                "",
                "Target.closeTarget",
                json!({"targetId": self.inner.target}),
            )
            .await;
        conn.off(&self.inner.session);
        if self.inner.owns_context
            && let Some(id) = &self.inner.context
        {
            let _ = conn
                .call(
                    "",
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": id}),
                )
                .await;
        }
        match closed {
            Ok(_) | Err(Error::Closed) | Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Answers a page's authentication challenges: a proxy's with the
/// proxy's login, a site's by leaving it to Chrome. A wrong login isn't
/// given again: Chrome stops asking after it's refused.
struct Login {
    conn: crate::conn::Connection,
    session: String,
    credentials: Option<(String, String)>,
    /// Where the main frame's documents go to be guarded.
    guarded: Option<tokio::sync::mpsc::UnboundedSender<(String, String)>>,
    /// The main frame, whose id is the target's.
    frame: String,
}

impl Login {
    fn answer(&self, params: &Value) {
        let response = match &self.credentials {
            Some((username, password)) if params["authChallenge"]["source"] == "Proxy" => {
                json!({"response": "ProvideCredentials", "username": username, "password": password})
            }
            _ => json!({"response": "Default"}),
        };
        self.conn.send(
            &self.session,
            "Fetch.continueWithAuth",
            json!({"requestId": str_of(params, "requestId"), "authChallengeResponse": response}),
        );
    }
}

impl Login {
    /// Lets a request Fetch paused go on unchanged, unless it's a document
    /// for the main frame and there's a guard to ask first.
    fn release(&self, params: &Value) {
        if let Some(guarded) = &self.guarded
            && params["resourceType"] == "Document"
            && params["frameId"] == self.frame.as_str()
        {
            let url = str_of(&params["request"], "url");
            let id = str_of(params, "requestId");
            if guarded.send((id.clone(), url)).is_err() {
                // The guard has gone (it panicked): nothing it would have
                // been asked about goes through unasked.
                self.conn.send(
                    &self.session,
                    "Fetch.failRequest",
                    json!({"requestId": id, "errorReason": "BlockedByClient"}),
                );
            }
            return;
        }
        self.conn.send(
            &self.session,
            "Fetch.continueRequest",
            json!({"requestId": str_of(params, "requestId")}),
        );
    }
}

/// How a failed check reads in a timeout.
fn never(check: &str) -> &'static str {
    match check {
        "attached" => "nothing matched it",
        "visible" => "it never became visible",
        "stable" => "it never stopped moving",
        "enabled" => "it never became enabled",
        "editable" => "it never became editable",
        "receives events" => "something else was always at its centre, covering it",
        _ => "it never became actionable",
    }
}

struct Key {
    key: String,
    code: String,
    key_code: u32,
    text: Option<String>,
}

impl Key {
    fn of(name: &str) -> Result<Key> {
        let named = |code: &str, key_code: u32, text: Option<&str>| Key {
            key: name.to_string(),
            code: code.to_string(),
            key_code,
            text: text.map(str::to_string),
        };
        Ok(match name {
            "Enter" => named("Enter", 13, Some("\r")),
            "Tab" => named("Tab", 9, None),
            "Escape" => named("Escape", 27, None),
            "Backspace" => named("Backspace", 8, None),
            "Delete" => named("Delete", 46, None),
            "ArrowLeft" => named("ArrowLeft", 37, None),
            "ArrowUp" => named("ArrowUp", 38, None),
            "ArrowRight" => named("ArrowRight", 39, None),
            "ArrowDown" => named("ArrowDown", 40, None),
            "Home" => named("Home", 36, None),
            "End" => named("End", 35, None),
            "PageUp" => named("PageUp", 33, None),
            "PageDown" => named("PageDown", 34, None),
            " " | "Space" => Key {
                key: " ".into(),
                code: "Space".into(),
                key_code: 32,
                text: Some(" ".into()),
            },
            _ => {
                let mut chars = name.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(Error::Invalid(format!("unknown key {name:?}")));
                };
                let (code, key_code) = if c.is_ascii_alphabetic() {
                    (
                        format!("Key{}", c.to_ascii_uppercase()),
                        c.to_ascii_uppercase() as u32,
                    )
                } else if c.is_ascii_digit() {
                    (format!("Digit{c}"), c as u32)
                } else {
                    (String::new(), 0)
                };
                Key {
                    key: c.to_string(),
                    code,
                    key_code,
                    text: Some(c.to_string()),
                }
            }
        })
    }
}
