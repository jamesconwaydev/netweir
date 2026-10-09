//! Declarative items and link rules: compiled from Python once, run in Rust
//! on every page.

use std::sync::Arc;

use netweir_dom::extract::{self, Convert, Field, Selector, Value};
use netweir_dom::{Query, XPath};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::node::Node;
use crate::{SelectorError, XPathError};

/// A compiled CSS or XPath query, with the error type Python expects.
pub(crate) fn selector(kind: &str, query: &str) -> PyResult<Selector> {
    match kind {
        "css" => Query::new(query)
            .map(Selector::Css)
            .map_err(|e| SelectorError::new_err(e.to_string())),
        "xpath" => XPath::new(query)
            .map(Selector::XPath)
            .map_err(|e| XPathError::new_err(e.to_string())),
        other => Err(PyValueError::new_err(format!(
            "a query is css or xpath, not {other:?}"
        ))),
    }
}

/// What went wrong with one field on one page: ("module.Class.field",
/// what was found, "convert" or "query").
pub(crate) type Problem = (String, String, &'static str);

/// An extracted item, and its problems.
pub(crate) type Extracted<'py> = (Bound<'py, PyDict>, Vec<Problem>);

pub(crate) struct ItemInner {
    spec: extract::ItemSpec,
    /// Python's class name, for messages.
    name: String,
    fields: Vec<String>,
    defaults: Vec<Py<PyAny>>,
}

impl ItemInner {
    pub(crate) fn extract(&self, root: netweir_dom::Node<'_>) -> Vec<Value> {
        self.spec.extract(root)
    }

    /// The item as a dict in field order, a missing value replaced by its
    /// default, and one that would not convert or whose query failed by
    /// None, with a Problem for each of those.
    pub(crate) fn to_python<'py>(
        &self,
        py: Python<'py>,
        values: Vec<Value>,
    ) -> PyResult<Extracted<'py>> {
        let dict = PyDict::new(py);
        let mut invalid = Vec::new();
        for ((name, default), value) in self.fields.iter().zip(&self.defaults).zip(values) {
            let v = match value {
                Value::Missing => default.clone_ref(py).into_bound(py),
                other => value_py(py, other, &mut |text, kind| {
                    invalid.push((format!("{}.{name}", self.name), text, kind))
                })?,
            };
            dict.set_item(name, v)?;
        }
        Ok((dict, invalid))
    }
}

fn value_py<'py>(
    py: Python<'py>,
    value: Value,
    invalid: &mut dyn FnMut(String, &'static str),
) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Missing => py.None().into_bound(py),
        Value::Text(s) => s.into_pyobject(py)?.into_any(),
        Value::Int(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
        Value::List(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_py(py, item, invalid)?)?;
            }
            list.into_any()
        }
        Value::Invalid(text) => {
            invalid(text, "convert");
            py.None().into_bound(py)
        }
        Value::Error(reason) => {
            invalid(reason, "query");
            py.None().into_bound(py)
        }
    })
}

/// An Item class's fields, compiled. Built by `netweir.Item`; not meant to
/// be made by hand.
#[pyclass(frozen, module = "netweir")]
pub struct ItemSpec {
    pub(crate) inner: Arc<ItemInner>,
}

#[pymethods]
impl ItemSpec {
    /// `fields` holds (name, "css" | "xpath", query, pattern, all, strip,
    /// "text" | "int" | "float" | "bool", default) for each field.
    #[new]
    #[allow(clippy::type_complexity)]
    fn new(
        name: String,
        fields: Vec<(
            String,
            String,
            String,
            Option<String>,
            bool,
            bool,
            String,
            Py<PyAny>,
        )>,
    ) -> PyResult<ItemSpec> {
        let mut compiled = Vec::new();
        let mut names = Vec::new();
        let mut defaults = Vec::new();
        for (field, kind, query, pattern, all, strip, convert, default) in fields {
            let convert = match convert.as_str() {
                "text" => Convert::Text,
                "int" => Convert::Int,
                "float" => Convert::Float,
                "bool" => Convert::Bool,
                other => {
                    return Err(PyValueError::new_err(format!(
                        "a field converts to text, int, float or bool, not {other:?}"
                    )));
                }
            };
            let mut f = Field::new(&field, selector(&kind, &query)?)
                .all(all)
                .strip(strip)
                .convert(convert);
            if let Some(p) = pattern {
                f = f.re(&p).map_err(PyValueError::new_err)?;
            }
            compiled.push(f);
            names.push(field);
            defaults.push(default);
        }
        Ok(ItemSpec {
            inner: Arc::new(ItemInner {
                spec: extract::ItemSpec::new(compiled),
                name,
                fields: names,
                defaults,
            }),
        })
    }

    /// The item found below `node`, and its problems.
    fn extract<'py>(&self, py: Python<'py>, node: &Node) -> PyResult<Extracted<'py>> {
        let (doc, id, inner) = (node.document().clone(), node.node_id(), self.inner.clone());
        // SAFETY: the id came from a Node of this document, which `doc` keeps
        // alive.
        let values = py.detach(move || inner.extract(unsafe { doc.node(id) }));
        self.inner.to_python(py, values)
    }
}

/// A link rule: which links to take from a page, and what to do with the
/// pages they lead to.
pub(crate) struct Rule {
    pub selector: Selector,
    pub item: Option<Arc<ItemInner>>,
    /// Apply the rules to the pages this rule leads to.
    pub follow: bool,
    /// Hand those pages to a Python callback.
    pub to_python: bool,
    pub priority: i32,
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ItemSpec>()
}
