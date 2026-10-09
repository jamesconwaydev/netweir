//! Bindings for netweir-browser. Every method that talks to Chrome returns
//! an awaitable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use netweir_browser::{
    Browser as CoreBrowser, Context as CoreContext, Cookie, Error, LaunchOptions, Page as CorePage,
    Response as CoreResponse, WaitUntil,
};
use netweir_dom::Document;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use crate::Node;

/// Raises the Python exception that matches `e`.
fn raise(e: Error) -> PyErr {
    Python::attach(|py| {
        let made = (|| -> PyResult<PyErr> {
            let errors = py.import("netweir._errors")?;
            let value = match &e {
                Error::Invalid(m) => return Ok(PyValueError::new_err(m.clone())),
                Error::Timeout(_) => errors.getattr("BrowserTimeout")?.call1((e.to_string(),))?,
                Error::Navigation(m) => {
                    // Chrome's net error names: the connection never
                    // happened, ran out of time, or failed TLS. Anything
                    // else, such as ERR_ABORTED for a 204 or a download,
                    // reached the server.
                    let kind = if m.contains("TIMED_OUT") {
                        "timeout"
                    } else if m.contains("CERT") || m.contains("SSL") {
                        "tls"
                    } else if [
                        "CONNECTION",
                        "NAME_NOT_RESOLVED",
                        "ADDRESS",
                        "INTERNET_DISCONNECTED",
                        "PROXY",
                    ]
                    .iter()
                    .any(|n| m.contains(n))
                    {
                        "connect"
                    } else {
                        "other"
                    };
                    errors.getattr("FetchError")?.call1((m.clone(), kind))?
                }
                _ => errors.getattr("BrowserError")?.call1((e.to_string(),))?,
            };
            Ok(PyErr::from_value(value))
        })();
        made.unwrap_or_else(|err| err)
    })
}

fn seconds(name: &str, value: Option<f64>) -> PyResult<Option<Duration>> {
    match value {
        None => Ok(None),
        Some(t) if t > 0.0 => Duration::try_from_secs_f64(t)
            .map(Some)
            .map_err(|_| PyValueError::new_err(format!("{name} is too long: {t} seconds"))),
        Some(_) => Err(PyValueError::new_err(format!(
            "{name} must be a positive number of seconds"
        ))),
    }
}

/// Starts Chrome. Resolves to a Browser.
#[pyfunction]
#[pyo3(signature = (executable=None, headless=true, args=Vec::new(), timeout=30.0, proxy=None))]
pub(crate) fn launch(
    py: Python<'_>,
    executable: Option<PathBuf>,
    headless: bool,
    args: Vec<String>,
    timeout: f64,
    proxy: Option<String>,
) -> PyResult<Bound<'_, PyAny>> {
    let timeout = seconds("timeout", Some(timeout))?.expect("checked above");
    let options = LaunchOptions {
        executable,
        headless,
        args,
        timeout,
        proxy,
    };
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let inner = CoreBrowser::launch(options).await.map_err(raise)?;
        Ok(Browser { inner })
    })
}

/// A running Chrome. Closing it closes every page.
#[pyclass(frozen, module = "netweir")]
pub struct Browser {
    inner: CoreBrowser,
}

#[pymethods]
impl Browser {
    /// Chrome's version, such as "Chrome/154.0.8037.98".
    #[getter]
    fn version(&self) -> String {
        self.inner.version().to_string()
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// A page in a context of its own: it shares no cookies or storage.
    fn new_page<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = browser.new_page().await.map_err(raise)?;
            Ok(BrowserPage { inner, live: None })
        })
    }

    /// A context whose pages share cookies and storage.
    fn new_context<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = browser.new_context().await.map_err(raise)?;
            Ok(BrowserContext { inner })
        })
    }

    /// Every DevTools method sent so far.
    fn methods_sent(&self) -> Vec<String> {
        self.inner.methods_sent()
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            browser.close().await.map_err(raise)
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<Browser {}>", self.inner.version())
    }
}

/// Pages that share cookies and storage.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserContext {
    inner: CoreContext,
}

#[pymethods]
impl BrowserContext {
    fn new_page<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = context.new_page().await.map_err(raise)?;
            Ok(BrowserPage { inner, live: None })
        })
    }

    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let cookies = context.cookies().await.map_err(raise)?;
            Python::attach(|py| cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(cookie_from)
            .collect::<PyResult<Vec<_>>>()?;
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            context.set_cookies(&cookies).await.map_err(raise)
        })
    }

    /// Closes the context and every page in it.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            context.close().await.map_err(raise)
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }
}

/// A page in Chrome.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserPage {
    inner: CorePage,
    /// For a page a crawl fetched: closing it frees its place in the
    /// crawl's pool.
    live: Option<netweir_core::LivePage>,
}

impl BrowserPage {
    pub(crate) fn from_crawl(live: netweir_core::LivePage) -> BrowserPage {
        BrowserPage {
            inner: live.page().clone(),
            live: Some(live),
        }
    }
}

#[pymethods]
impl BrowserPage {
    /// The current URL.
    #[getter]
    fn url(&self) -> String {
        self.inner.url()
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Navigates and waits for "domcontentloaded", "load" or
    /// "networkidle". Resolves to a BrowserResponse.
    #[pyo3(signature = (url, wait="load", timeout=None))]
    fn goto<'py>(
        &self,
        py: Python<'py>,
        url: String,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = page.goto(&url, wait, timeout).await.map_err(raise)?;
            Ok(BrowserResponse { inner })
        })
    }

    /// Waits for a navigation away from the document showing now to reach
    /// `wait`. Resolves to the BrowserResponse of the new document.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn wait_for_navigation<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        // The document showing now, read before anything is awaited, so a
        // navigation that starts while the future waits to run still
        // counts.
        let after = page.response();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = page
                .wait_for_navigation(&after, wait, timeout)
                .await
                .map_err(raise)?;
            Ok(BrowserResponse { inner })
        })
    }

    /// The HTML as Chrome now has it.
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.content().await.map_err(raise)
        })
    }

    /// The page as it is now, parsed into a netweir Node.
    fn parse<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let html = page.content().await.map_err(raise)?;
            let doc = tokio::task::spawn_blocking(move || Document::parse(&html))
                .await
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Node::root_of(Arc::new(doc)))
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(
            py,
            async move { page.title().await.map_err(raise) },
        )
    }

    /// Runs `js` in the page: an expression, or a function, which is
    /// called. Resolves to the result as Python values.
    fn evaluate<'py>(&self, py: Python<'py>, js: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let value = page.evaluate(&js).await.map_err(raise)?;
            Python::attach(|py| to_python(py, &value))
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.click(&selector, timeout).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, text, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.fill(&selector, &text, timeout).await.map_err(raise)
        })
    }

    fn press<'py>(&self, py: Python<'py>, key: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.press(&key).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, state="visible".to_string(), timeout=None))]
    fn wait_for<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        state: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.wait_for(&selector, &state, timeout)
                .await
                .map_err(raise)
        })
    }

    /// PNG bytes, also written to `path` if given.
    #[pyo3(signature = (path=None, full_page=false))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        full_page: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let png = page.screenshot(full_page).await.map_err(raise)?;
            if let Some(path) = path {
                std::fs::write(&path, &png)?;
            }
            Python::attach(|py| Ok(PyBytes::new(py, &png).unbind()))
        })
    }

    /// The cookies of the page's context, as dicts.
    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let cookies = page.cookies().await.map_err(raise)?;
            Python::attach(|py| cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(cookie_from)
            .collect::<PyResult<Vec<_>>>()?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.set_cookies(&cookies).await.map_err(raise)
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (page, live) = (self.inner.clone(), self.live.clone());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            match live {
                Some(live) => {
                    live.close().await;
                    Ok(())
                }
                None => page.close().await.map_err(raise),
            }
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<BrowserPage {}>", self.inner.url())
    }
}

/// The main document's response to a navigation.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserResponse {
    inner: CoreResponse,
}

#[pymethods]
impl BrowserResponse {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    /// 0 when there was no HTTP response (about:, data:).
    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    fn __repr__(&self) -> String {
        format!("<BrowserResponse {} {}>", self.inner.status, self.inner.url)
    }
}

fn cookie_list(py: Python<'_>, cookies: &[Cookie]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for c in cookies {
        let d = PyDict::new(py);
        d.set_item("name", &c.name)?;
        d.set_item("value", &c.value)?;
        d.set_item("domain", &c.domain)?;
        d.set_item("path", &c.path)?;
        d.set_item("expires", c.expires)?;
        d.set_item("http_only", c.http_only)?;
        d.set_item("secure", c.secure)?;
        d.set_item("same_site", &c.same_site)?;
        list.append(d)?;
    }
    Ok(list.into_any().unbind())
}

fn cookie_from(d: &Bound<'_, PyAny>) -> PyResult<Cookie> {
    let d = d
        .cast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("each cookie must be a dict"))?;
    let text = |key: &str| -> PyResult<Option<String>> {
        d.get_item(key)?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()
    };
    let flag = |key: &str| -> PyResult<bool> {
        Ok(d.get_item(key)?
            .map(|v| v.is_truthy())
            .transpose()?
            .unwrap_or(false))
    };
    let required = |key: &str| -> PyResult<String> {
        text(key)?.ok_or_else(|| PyValueError::new_err(format!("a cookie needs a {key:?}")))
    };
    Ok(Cookie {
        name: required("name")?,
        value: required("value")?,
        domain: required("domain")?,
        path: text("path")?.unwrap_or_else(|| "/".into()),
        expires: d
            .get_item("expires")?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or(-1.0),
        http_only: flag("http_only")?,
        secure: flag("secure")?,
        same_site: text("same_site")?,
    })
}

/// JSON from the page as Python values.
fn to_python(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value;
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into_pyobject(py)?.into_any().unbind(),
            None => n
                .as_f64()
                .unwrap_or(f64::NAN)
                .into_pyobject(py)?
                .into_any()
                .unbind(),
        },
        Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(to_python(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(map) => {
            let d = PyDict::new(py);
            for (k, v) in map {
                d.set_item(k, to_python(py, v)?)?;
            }
            d.into_any().unbind()
        }
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(launch, m)?)?;
    m.add_class::<Browser>()?;
    m.add_class::<BrowserContext>()?;
    m.add_class::<BrowserPage>()?;
    m.add_class::<BrowserResponse>()?;
    Ok(())
}
