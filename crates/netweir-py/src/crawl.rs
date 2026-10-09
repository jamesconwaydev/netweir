//! The crawl engine from Python: submit requests, await batches of events.

use std::time::Duration;

use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler as CoreCrawler, DropReason, Event, Submitted,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use crate::fetch::{Response, fetch_error_value, fetch_options};

fn seconds(name: &str, value: f64) -> PyResult<Duration> {
    Duration::try_from_secs_f64(value).map_err(|_| {
        PyValueError::new_err(format!("{name} must be a non-negative number of seconds"))
    })
}

/// One crawl: per-host queues, robots.txt and TDMRep checks, throttling and
/// deduplication, all in Rust. Python submits requests by id and awaits
/// `next()` for what happened to them.
#[pyclass(frozen, module = "netweir")]
pub struct Crawler {
    inner: CoreCrawler,
}

#[pymethods]
impl Crawler {
    #[new]
    #[pyo3(signature = (
        profile="chrome", proxy=None, timeout=30.0, verify=true,
        concurrency=64, per_domain=8, obey_robots=true, robots_agent="netweir", obey_tdmrep=true,
        throttle=true, start_delay=1.0, min_delay=0.0, max_delay=60.0, target_concurrency=1.0,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        profile: &str,
        proxy: Option<String>,
        timeout: f64,
        verify: bool,
        concurrency: usize,
        per_domain: usize,
        obey_robots: bool,
        robots_agent: &str,
        obey_tdmrep: bool,
        throttle: bool,
        start_delay: f64,
        min_delay: f64,
        max_delay: f64,
        target_concurrency: f64,
    ) -> PyResult<Crawler> {
        if concurrency == 0 || per_domain == 0 {
            return Err(PyValueError::new_err(
                "concurrency and per_domain must be at least 1",
            ));
        }
        if !(target_concurrency > 0.0 && target_concurrency.is_finite()) {
            return Err(PyValueError::new_err(
                "target_concurrency must be a positive number",
            ));
        }
        let (min_delay, max_delay) = (
            seconds("min_delay", min_delay)?,
            seconds("max_delay", max_delay)?,
        );
        if min_delay > max_delay {
            return Err(PyValueError::new_err("min_delay must not exceed max_delay"));
        }
        let settings = CrawlSettings {
            concurrency,
            per_domain,
            obey_robots,
            robots_agent: robots_agent.to_string(),
            obey_tdmrep,
            throttle,
            start_delay: seconds("start_delay", start_delay)?,
            min_delay,
            max_delay,
            target_concurrency,
        };
        let options = fetch_options(profile, proxy, timeout, verify)?;
        // The scheduler runs on the shared runtime.
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        let inner =
            CoreCrawler::new(options, settings).map_err(|e| PyValueError::new_err(e.message))?;
        Ok(Crawler { inner })
    }

    /// Queues a request. Returns "queued", "duplicate" or "invalid".
    #[pyo3(signature = (id, url, priority=0, headers=None, dont_filter=false))]
    fn submit(
        &self,
        id: u64,
        url: String,
        priority: i32,
        headers: Option<Vec<(String, String)>>,
        dont_filter: bool,
    ) -> &'static str {
        let request = CrawlRequest {
            id,
            url,
            priority,
            headers: headers.unwrap_or_default(),
            dont_filter,
        };
        match self.inner.submit(request) {
            Submitted::Queued => "queued",
            Submitted::Duplicate => "duplicate",
            Submitted::Invalid => "invalid",
        }
    }

    /// Awaits up to `max` events as (kind, id, detail) tuples: ("fetched",
    /// id, Response), ("failed", id, FetchError) or ("dropped", id,
    /// "robots" | "tdm"). An empty list means the crawl is finished.
    #[pyo3(signature = (max=256))]
    fn next<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        // The future owns a handle to the engine, so it doesn't borrow self.
        let pending = self.inner.next_owned(max);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let events = pending.await;
            Python::attach(|py| -> PyResult<Py<PyAny>> {
                let mut out = Vec::with_capacity(events.len());
                for event in events {
                    let item: Bound<'_, PyTuple> = match event {
                        Event::Fetched { id, response } => PyTuple::new(
                            py,
                            [
                                ("fetched").into_pyobject(py)?.into_any(),
                                id.into_pyobject(py)?.into_any(),
                                Py::new(py, Response::from(response))?
                                    .into_bound(py)
                                    .into_any(),
                            ],
                        )?,
                        Event::Failed { id, error } => PyTuple::new(
                            py,
                            [
                                ("failed").into_pyobject(py)?.into_any(),
                                id.into_pyobject(py)?.into_any(),
                                fetch_error_value(py, &error)?.into_bound(py),
                            ],
                        )?,
                        Event::Dropped { id, reason } => {
                            let why = match reason {
                                DropReason::Robots => "robots",
                                DropReason::TdmReserved => "tdm",
                            };
                            PyTuple::new(
                                py,
                                [
                                    ("dropped").into_pyobject(py)?.into_any(),
                                    id.into_pyobject(py)?.into_any(),
                                    why.into_pyobject(py)?.into_any(),
                                ],
                            )?
                        }
                    };
                    out.push(item);
                }
                Ok(pyo3::types::PyList::new(py, out)?.into_any().unbind())
            })
        })
    }

    /// Counters so far.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = self.inner.stats();
        let d = PyDict::new(py);
        d.set_item("queued", s.queued)?;
        d.set_item("in_flight", s.in_flight)?;
        d.set_item("fetched", s.fetched)?;
        d.set_item("failed", s.failed)?;
        d.set_item("duplicates", s.duplicates)?;
        d.set_item("dropped_robots", s.dropped_robots)?;
        d.set_item("dropped_tdm", s.dropped_tdm)?;
        d.set_item("bytes", s.bytes)?;
        Ok(d)
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Crawler>()
}
