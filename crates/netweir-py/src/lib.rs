//! Python bindings. Kept thin: anything that could live in netweir-dom does.

mod fetch;

use std::sync::Arc;
use std::time::Duration;

use netweir_dom::{Document, NodeId, Output, Query};
use pyo3::create_exception;
use pyo3::exceptions::{PyIndexError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

create_exception!(
    netweir,
    SelectorError,
    PyValueError,
    "The CSS selector could not be parsed."
);
create_exception!(
    netweir,
    ParseTimeout,
    PyTimeoutError,
    "Parsing took longer than the timeout."
);

fn compile(query: &str) -> PyResult<Query> {
    Query::new(query).map_err(|e| SelectorError::new_err(e.to_string()))
}

/// A node in a parsed document.
#[pyclass(frozen, module = "netweir")]
pub(crate) struct Node {
    doc: Arc<Document>,
    id: NodeId,
}

impl Node {
    pub(crate) fn root_of(doc: Arc<Document>) -> Node {
        let id = doc.root().id();
        Node { doc, id }
    }

    fn get(&self) -> netweir_dom::Node<'_> {
        // SAFETY: `id` was taken from a node of `doc`, which this Node keeps alive.
        unsafe { self.doc.node(self.id) }
    }
}

/// Query results: nodes, or strings when the query ended in `::text` or
/// `::attr()`.
#[pyclass(frozen, sequence, module = "netweir")]
struct Selection {
    doc: Arc<Document>,
    nodes: Vec<NodeId>,
    strings: Option<Vec<String>>,
}

impl Selection {
    fn run(
        py: Python<'_>,
        doc: &Arc<Document>,
        scopes: Vec<NodeId>,
        query: &str,
    ) -> PyResult<Selection> {
        let query = compile(query)?;
        let shared = doc.clone();
        let (nodes, strings) = py.detach(move || {
            let mut nodes = Vec::new();
            let mut strings = Vec::new();
            for id in scopes {
                // SAFETY: every id in `scopes` came from `shared`.
                let scope = unsafe { shared.node(id) };
                match query.output() {
                    Output::Nodes => nodes.extend(query.select(scope).iter().map(|n| n.id())),
                    _ => strings.extend(query.strings(scope)),
                }
            }
            let wants_nodes = *query.output() == Output::Nodes;
            (nodes, (!wants_nodes).then_some(strings))
        });
        Ok(Selection {
            doc: doc.clone(),
            nodes,
            strings,
        })
    }

    fn text_of(&self, id: NodeId) -> String {
        // SAFETY: ids in a Selection all come from its `doc`.
        unsafe { self.doc.node(id) }.text()
    }
}

#[pymethods]
impl Node {
    fn css(&self, py: Python<'_>, query: &str) -> PyResult<Selection> {
        Selection::run(py, &self.doc, vec![self.id], query)
    }

    /// All text below this node.
    #[getter]
    fn text(&self) -> String {
        self.get().text()
    }

    /// Lowercase tag name, or None for the document node.
    #[getter]
    fn tag(&self) -> Option<&str> {
        self.get().tag()
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.get().attr(name)
    }

    #[getter]
    fn attrs<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (k, v) in self.get().attrs() {
            dict.set_item(k, v)?;
        }
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        match self.get().tag() {
            Some(tag) => format!("<Node {tag}>"),
            None => "<Node document>".to_string(),
        }
    }
}

#[pymethods]
impl Selection {
    /// The first result as a string, or `default` if there are none.
    #[pyo3(signature = (default=None))]
    fn get(&self, default: Option<String>) -> Option<String> {
        match &self.strings {
            Some(s) => s.first().cloned().or(default),
            None => self.nodes.first().map(|&id| self.text_of(id)).or(default),
        }
    }

    /// Every result as a string.
    fn getall(&self) -> Vec<String> {
        match &self.strings {
            Some(s) => s.clone(),
            None => self.nodes.iter().map(|&id| self.text_of(id)).collect(),
        }
    }

    /// Runs `query` below every node in this selection.
    fn css(&self, py: Python<'_>, query: &str) -> PyResult<Selection> {
        Selection::run(py, &self.doc, self.nodes.clone(), query)
    }

    fn __len__(&self) -> usize {
        self.strings.as_ref().map_or(self.nodes.len(), Vec::len)
    }

    fn __getitem__(&self, py: Python<'_>, index: isize) -> PyResult<Py<PyAny>> {
        let len = self.__len__() as isize;
        let i = if index < 0 { index + len } else { index };
        if i < 0 || i >= len {
            return Err(PyIndexError::new_err("selection index out of range"));
        }
        let i = i as usize;
        Ok(match &self.strings {
            Some(s) => s[i].clone().into_pyobject(py)?.into_any().unbind(),
            None => Py::new(
                py,
                Node {
                    doc: self.doc.clone(),
                    id: self.nodes[i],
                },
            )?
            .into_any(),
        })
    }

    fn __repr__(&self) -> String {
        match &self.strings {
            Some(s) => format!("<Selection {} strings>", s.len()),
            None => format!("<Selection {} nodes>", self.nodes.len()),
        }
    }
}

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

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_class::<Node>()?;
    m.add_class::<Selection>()?;
    m.add("SelectorError", m.py().get_type::<SelectorError>())?;
    m.add("ParseTimeout", m.py().get_type::<ParseTimeout>())?;
    fetch::register(m)?;
    Ok(())
}
