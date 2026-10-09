//! Python bindings. Kept thin: anything that could live in netweir-dom or
//! netweir-core does.

mod browser;
mod crawl;
mod export;
mod fetch;
mod filter;
mod node;
mod rules;

use std::sync::Arc;
use std::time::Duration;

use netweir_dom::Document;
use pyo3::create_exception;
use pyo3::exceptions::{PyTimeoutError, PyValueError};
use pyo3::prelude::*;

pub(crate) use node::{Node, Selection};

create_exception!(
    netweir,
    SelectorError,
    PyValueError,
    "The CSS selector could not be parsed."
);
create_exception!(
    netweir,
    XPathError,
    PyValueError,
    "The XPath expression could not be parsed or evaluated."
);
create_exception!(
    netweir,
    ParseTimeout,
    PyTimeoutError,
    "Parsing took longer than the timeout."
);

/// Seconds from Python to a parse budget; `None` means no limit.
pub(crate) fn budget(timeout: Option<f64>) -> PyResult<Duration> {
    match timeout {
        None => Ok(Duration::MAX),
        Some(t) if t >= 0.0 && t.is_finite() => {
            Ok(Duration::try_from_secs_f64(t).unwrap_or(Duration::MAX))
        }
        Some(_) => Err(PyValueError::new_err(
            "timeout must be a non-negative number of seconds",
        )),
    }
}

/// Parses an HTML document and returns its root node.
///
/// With `timeout` (seconds), gives up and raises ParseTimeout on pages that
/// take longer, such as deeply nested hostile markup.
#[pyfunction]
#[pyo3(signature = (html, timeout=None))]
fn parse(py: Python<'_>, html: &str, timeout: Option<f64>) -> PyResult<Node> {
    let budget = budget(timeout)?;
    let doc = py
        .detach(|| Document::parse_within(html, budget))
        .map_err(|e| ParseTimeout::new_err(e.to_string()))?;
    Ok(Node::root_of(Arc::new(doc)))
}

/// The crawl's identity for a GET of `url`, as hex: equal for the same
/// page however its URL is written. None for anything but http(s).
#[pyfunction]
fn fingerprint(url: &str) -> Option<String> {
    netweir_core::canonical::fingerprint("GET", url)
        .map(|fp| fp.iter().map(|b| format!("{b:02x}")).collect())
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_function(wrap_pyfunction!(fingerprint, m)?)?;
    m.add_class::<Node>()?;
    m.add_class::<Selection>()?;
    m.add("SelectorError", m.py().get_type::<SelectorError>())?;
    m.add("XPathError", m.py().get_type::<XPathError>())?;
    m.add("ParseTimeout", m.py().get_type::<ParseTimeout>())?;
    browser::register(m)?;
    fetch::register(m)?;
    crawl::register(m)?;
    export::register(m)?;
    rules::register(m)?;
    Ok(())
}
