//! Fetching from Python: `Fetcher.get` is awaitable, `Fetcher.get_blocking`
//! is for scripts. Both run on one shared tokio runtime with the GIL
//! released.

use std::sync::Arc;
use std::time::Duration;

use netweir_core::{
    FetchError as CoreError, FetchErrorKind, FetchOptions, Fetcher as CoreFetcher, Profile,
};
use netweir_dom::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::Node;

/// A `netweir.FetchError(message, kind)`, defined in Python so it can carry
/// a `kind` attribute.
pub(crate) fn fetch_error_value(py: Python<'_>, e: &CoreError) -> PyResult<Py<PyAny>> {
    let kind = match e.kind {
        FetchErrorKind::Invalid => "invalid",
        FetchErrorKind::Timeout => "timeout",
        FetchErrorKind::Connect => "connect",
        FetchErrorKind::Tls => "tls",
        FetchErrorKind::TooManyRedirects => "too_many_redirects",
        FetchErrorKind::Body => "body",
        FetchErrorKind::Other => "other",
    };
    let class = py.import("netweir._errors")?.getattr("FetchError")?;
    Ok(class.call1((e.message.clone(), kind))?.unbind())
}

/// Raises `netweir.FetchError(message, kind)`.
fn fetch_error(e: CoreError) -> PyErr {
    Python::attach(|py| match fetch_error_value(py, &e) {
        Ok(err) => PyErr::from_value(err.into_bound(py)),
        Err(err) => err,
    })
}

/// Fetch options from the arguments Fetcher and Crawler share.
pub(crate) fn fetch_options(
    profile: &str,
    proxy: Option<String>,
    timeout: f64,
    verify: bool,
) -> PyResult<FetchOptions> {
    let profile = Profile::named(profile).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let timeout = Duration::try_from_secs_f64(timeout)
        .ok()
        .filter(|t| !t.is_zero())
        .ok_or_else(|| PyValueError::new_err("timeout must be a positive number of seconds"))?;
    let mut options = FetchOptions::new(profile);
    options.proxy = proxy;
    options.timeout = timeout;
    options.verify_certificates = verify;
    Ok(options)
}

/// A connection pool and cookie jar that sends every request as one
/// browser profile.
#[pyclass(frozen, module = "netweir")]
pub struct Fetcher {
    inner: CoreFetcher,
}

#[pymethods]
impl Fetcher {
    #[new]
    #[pyo3(signature = (profile="chrome", proxy=None, timeout=30.0, verify=true))]
    fn new(profile: &str, proxy: Option<String>, timeout: f64, verify: bool) -> PyResult<Fetcher> {
        let options = fetch_options(profile, proxy, timeout, verify)?;
        // Everything that can fail here is a bad argument (profile, proxy).
        let inner = CoreFetcher::new(options).map_err(|e| PyValueError::new_err(e.message))?;
        Ok(Fetcher { inner })
    }

    /// Awaitable GET. Resolves to a Response.
    #[pyo3(signature = (url, headers=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        url: String,
        headers: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fetcher = self.inner.clone();
        let headers = headers.unwrap_or_default();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = fetcher.get(&url, &headers).await.map_err(fetch_error)?;
            Ok(Response::from(response))
        })
    }

    /// GET that blocks the calling thread (with the GIL released).
    #[pyo3(signature = (url, headers=None))]
    fn get_blocking(
        &self,
        py: Python<'_>,
        url: String,
        headers: Option<Vec<(String, String)>>,
    ) -> PyResult<Response> {
        let headers = headers.unwrap_or_default();
        let runtime = pyo3_async_runtimes::tokio::get_runtime();
        py.detach(|| runtime.block_on(self.inner.get(&url, &headers)))
            .map(Response::from)
            .map_err(fetch_error)
    }
}

#[pyclass(frozen, module = "netweir")]
pub struct Response {
    inner: Arc<netweir_core::Response>,
}

impl From<netweir_core::Response> for Response {
    fn from(r: netweir_core::Response) -> Response {
        Response { inner: Arc::new(r) }
    }
}

#[pymethods]
impl Response {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn version(&self) -> &'static str {
        self.inner.version
    }

    /// (name, value) pairs in the order received; names are lowercase.
    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.body)
    }

    /// The body decoded the way a browser would pick the encoding.
    fn text(&self, py: Python<'_>) -> String {
        let r = self.inner.clone();
        py.detach(move || r.text())
    }

    /// Decodes and parses the body; returns the document's root node.
    #[pyo3(signature = (timeout=None))]
    fn parse(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Node> {
        let r = self.inner.clone();
        let budget = crate::budget(timeout)?;
        let doc = py
            .detach(move || Document::parse_within(&r.text(), budget))
            .map_err(|e| crate::ParseTimeout::new_err(e.to_string()))?;
        Ok(Node::root_of(Arc::new(doc)))
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Fetcher>()?;
    m.add_class::<Response>()?;
    Ok(())
}
