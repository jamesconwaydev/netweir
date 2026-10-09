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

/// For a Chrome major version (such as "154"), the `Sec-CH-UA` value its
/// pages should present, if not Chrome's own.
pub type Brands = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct LaunchOptions {
    /// Chrome's executable; found with [`crate::find_chrome`] when None.
    pub executable: Option<PathBuf>,
    pub headless: bool,
    /// More command-line switches for Chrome.
    pub args: Vec<String>,
    /// The default limit for navigations and actions.
    pub timeout: Duration,
    /// An `http://`, `https://` or `socks5://` proxy for everything Chrome
    /// fetches. A username and password in it are answered when the proxy
    /// asks for them.
    pub proxy: Option<String>,
    /// The brands pages present. Chrome for Testing calls itself Chromium
    /// alone, where Google Chrome of the same version lists itself too.
    pub brands: Option<Brands>,
}

impl std::fmt::Debug for LaunchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOptions")
            .field("executable", &self.executable)
            .field("headless", &self.headless)
            .field("args", &self.args)
            .field("timeout", &self.timeout)
            .field("proxy", &self.proxy.as_ref().map(|_| "…"))
            .field("brands", &self.brands.as_ref().map(|_| "…"))
            .finish()
    }
}

impl Default for LaunchOptions {
    fn default() -> LaunchOptions {
        LaunchOptions {
            executable: None,
            headless: true,
            args: Vec::new(),
            timeout: Duration::from_secs(30),
            proxy: None,
            brands: None,
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
    /// The proxy's username and password, for its challenges.
    pub proxy_login: Option<(String, String)>,
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
        let mut proxy_login = None;
        if let Some(proxy) = &options.proxy {
            let (server, login) = split_proxy(proxy)?;
            args.push(format!("--proxy-server={server}"));
            proxy_login = login;
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
            proxy_login,
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
        let full = browser
            .inner
            .version
            .trim_start_matches("Chrome/")
            .to_string();
        let major = full.split('.').next().unwrap_or_default().to_string();
        let brands = options.brands.as_ref().and_then(|b| b(&major));
        if user_agent.contains("HeadlessChrome/") || brands.is_some() {
            let mut metadata = browser.own_metadata().await?;
            if let Some(header) = brands {
                let list = parse_brands(&header);
                metadata["brands"] = list
                    .iter()
                    .map(|(brand, version)| json!({"brand": brand, "version": version}))
                    .collect();
                // A brand at Chrome's own major version carries its full
                // version; the made-up one, as in Chrome, is N.0.0.0.
                metadata["fullVersionList"] = list
                    .iter()
                    .map(|(brand, version)| {
                        let full = if *version == major {
                            full.clone()
                        } else {
                            format!("{version}.0.0.0")
                        };
                        json!({"brand": brand, "version": full})
                    })
                    .collect();
            }
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

    /// True once closed, and also once Chrome has exited or crashed.
    pub fn is_closed(&self) -> bool {
        self.inner.conn.is_closed() || self.child().is_none()
    }

    /// Chrome's process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child().as_ref().map(|c| c.id())
    }

    fn child(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.inner.child.lock().unwrap_or_else(|e| e.into_inner())
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

/// `"Chromium";v="154", "Google Chrome";v="154"` as (brand, version)
/// pairs.
fn parse_brands(header: &str) -> Vec<(String, String)> {
    header
        .split(',')
        .filter_map(|part| {
            let (brand, version) = part.trim().split_once(";v=")?;
            Some((
                brand.trim_matches('"').to_string(),
                version.trim_matches('"').to_string(),
            ))
        })
        .collect()
}

/// A proxy URL as Chrome takes it (`scheme://host:port`, which has no
/// room for a login), and the username and password that were in it.
fn split_proxy(proxy: &str) -> Result<(String, Option<(String, String)>)> {
    let bad = || Error::Invalid(format!("not a proxy URL: {proxy}"));
    let (scheme, rest) = proxy.split_once("://").ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    let Some((login, host)) = rest.rsplit_once('@') else {
        return Ok((format!("{scheme}://{rest}"), None));
    };
    if host.is_empty() {
        return Err(bad());
    }
    let (user, password) = login.split_once(':').unwrap_or((login, ""));
    Ok((
        format!("{scheme}://{host}"),
        Some((unescape(user), unescape(password))),
    ))
}

/// Undoes URL percent-escapes, so a password can hold `@` or `:`.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_login_comes_off_the_url_chrome_gets() {
        assert_eq!(
            split_proxy("http://proxy:8080").unwrap(),
            ("http://proxy:8080".into(), None)
        );
        assert_eq!(
            split_proxy("socks5://ann:p%40ss%3Aword@10.0.0.1:1080/").unwrap(),
            (
                "socks5://10.0.0.1:1080".into(),
                Some(("ann".into(), "p@ss:word".into()))
            )
        );
        assert!(split_proxy("proxy:8080").is_err());
        assert!(split_proxy("http://ann@").is_err());
    }
}
