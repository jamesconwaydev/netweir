//! A running Chrome and its browser contexts.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::conn::Connection;
use crate::page::{Cookie, Page, WaitUntil, cookies_from, cookies_to};
use crate::{Error, Result, launch};

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Chrome's executable; found with [`crate::find_chrome`] when None.
    pub executable: Option<PathBuf>,
    pub headless: bool,
    /// More command-line switches for Chrome.
    pub args: Vec<String>,
    /// The default limit for navigations and actions.
    pub timeout: Duration,
}

impl Default for LaunchOptions {
    fn default() -> LaunchOptions {
        LaunchOptions {
            executable: None,
            headless: true,
            args: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// What every page says it is: the user agent without the headless
/// marker, and the client hints Chrome reports for itself.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub user_agent: String,
    pub metadata: Value,
}

/// A running Chrome. Cheap to clone; closes when [`Browser::close`] is
/// called or the last clone is dropped.
#[derive(Clone)]
pub struct Browser {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub conn: Connection,
    child: Mutex<Option<Child>>,
    profile: PathBuf,
    version: String,
    pub identity: Option<Identity>,
    pub timeout: Duration,
}

impl Browser {
    pub async fn launch(options: LaunchOptions) -> Result<Browser> {
        let executable = match options.executable {
            Some(path) => path,
            None => launch::find_chrome()?,
        };
        let profile = profile_dir()
            .map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
        let mut args = vec![
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            // Leaves navigator.webdriver false.
            "--disable-blink-features=AutomationControlled".into(),
        ];
        if options.headless {
            args.push("--headless".into());
        }
        args.extend(options.args);
        args.push("about:blank".into());
        let started = match launch::start(&executable, &args) {
            Ok(started) => started,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(e);
            }
        };
        let conn = Connection::new(started.replies, started.commands);
        let mut inner = Inner {
            conn,
            child: Mutex::new(Some(started.child)),
            profile,
            version: String::new(),
            identity: None,
            timeout: options.timeout,
        };
        let version = match tokio::time::timeout(
            Duration::from_secs(30),
            inner.conn.call("", "Browser.getVersion", json!({})),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                return Err(Error::Launch(format!(
                    "{} didn't answer: {e}",
                    executable.display()
                )));
            }
            Err(_) => {
                return Err(Error::Launch(format!(
                    "{} didn't answer in 30 seconds",
                    executable.display()
                )));
            }
        };
        inner.version = str_of(&version, "product");
        let user_agent = str_of(&version, "userAgent");
        let mut browser = Browser {
            inner: Arc::new(inner),
        };
        if user_agent.contains("HeadlessChrome/") {
            let metadata = browser.own_metadata().await?;
            let identity = Identity {
                user_agent: user_agent.replace("HeadlessChrome/", "Chrome/"),
                metadata,
            };
            // Nothing else holds the Arc yet.
            Arc::get_mut(&mut browser.inner)
                .expect("the browser isn't shared before launch returns")
                .identity = Some(identity);
        }
        Ok(browser)
    }

    /// The client hints Chrome reports for itself, read where no override
    /// is in force and the hints are visible: chrome://version.
    async fn own_metadata(&self) -> Result<Value> {
        let page = Page::open(self.inner.clone(), None, false).await?;
        let read = async {
            page.goto("chrome://version", WaitUntil::Load, None).await?;
            page.evaluate(
                "navigator.userAgentData.getHighEntropyValues(['architecture', 'bitness', \
                 'formFactors', 'fullVersionList', 'model', 'platformVersion', 'wow64'])",
            )
            .await
        }
        .await;
        page.close().await?;
        let values = read?;
        Ok(json!({
            "brands": values["brands"],
            "fullVersionList": values["fullVersionList"],
            "platform": values["platform"],
            "platformVersion": values["platformVersion"],
            "architecture": values["architecture"],
            "model": values["model"],
            "mobile": values["mobile"],
            "bitness": values["bitness"],
            "wow64": values["wow64"],
            "formFactors": values["formFactors"],
        }))
    }

    /// Chrome's product string, such as `Chrome/154.0.8037.98`.
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    pub fn profile_dir(&self) -> &Path {
        &self.inner.profile
    }

    pub fn is_closed(&self) -> bool {
        self.inner.conn.is_closed()
            && self
                .inner
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
    }

    /// A page in a context of its own: no cookies or storage shared with
    /// any other page. Closing it closes the context.
    pub async fn new_page(&self) -> Result<Page> {
        let context = self.create_context().await?;
        Page::open(self.inner.clone(), Some(context), true).await
    }

    /// A context whose pages share cookies and storage with each other.
    pub async fn new_context(&self) -> Result<Context> {
        Ok(Context {
            browser: self.inner.clone(),
            id: self.create_context().await?,
        })
    }

    async fn create_context(&self) -> Result<String> {
        let r = self
            .inner
            .conn
            .call("", "Target.createBrowserContext", json!({}))
            .await?;
        Ok(str_of(&r, "browserContextId"))
    }

    /// Every CDP method sent so far, for tests that check nothing
    /// detectable was.
    pub fn methods_sent(&self) -> Vec<String> {
        self.inner.conn.methods_sent()
    }

    /// Asks Chrome to quit, waits for it (killing it after 10 seconds),
    /// and removes its profile directory.
    pub async fn close(&self) -> Result<()> {
        let inner = self.inner.clone();
        if inner
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Ok(());
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            inner.conn.call("", "Browser.close", json!({})),
        )
        .await;
        tokio::task::spawn_blocking(move || inner.shut_down(Duration::from_secs(10)))
            .await
            .map_err(|e| Error::Launch(format!("closing the browser failed: {e}")))?
    }
}

impl Inner {
    fn shut_down(&self, grace: Duration) -> Result<()> {
        let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        self.conn.shut();
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        remove_profile(&self.profile)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.shut_down(Duration::ZERO);
    }
}

/// Chrome's helper processes can hold files in the profile for a moment
/// after the browser exits, and Windows won't delete an open file.
fn remove_profile(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Err(Error::Launch(format!(
                    "can't remove the profile directory {}: {e}",
                    dir.display()
                )));
            }
        }
    }
}

fn profile_dir() -> std::io::Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "netweir-chrome-{}-{}-{nanos}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub(crate) fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Pages that share cookies and storage.
#[derive(Clone)]
pub struct Context {
    browser: Arc<Inner>,
    id: String,
}

impl Context {
    pub async fn new_page(&self) -> Result<Page> {
        Page::open(self.browser.clone(), Some(self.id.clone()), false).await
    }

    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        context_cookies(&self.browser, Some(&self.id)).await
    }

    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        set_context_cookies(&self.browser, Some(&self.id), cookies).await
    }

    /// Closes the context and every page in it.
    pub async fn close(&self) -> Result<()> {
        match self
            .browser
            .conn
            .call(
                "",
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.id}),
            )
            .await
        {
            Ok(_) | Err(Error::Closed) => Ok(()),
            // Already disposed.
            Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

pub(crate) async fn context_cookies(browser: &Inner, context: Option<&str>) -> Result<Vec<Cookie>> {
    let mut params = json!({});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    let r = browser.conn.call("", "Storage.getCookies", params).await?;
    Ok(cookies_from(&r["cookies"]))
}

pub(crate) async fn set_context_cookies(
    browser: &Inner,
    context: Option<&str>,
    cookies: &[Cookie],
) -> Result<()> {
    let mut params = json!({"cookies": cookies_to(cookies)});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    browser.conn.call("", "Storage.setCookies", params).await?;
    Ok(())
}
