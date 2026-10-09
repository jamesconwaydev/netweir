//! HTML parsing, CSS and XPath queries, and declarative extraction for
//! netweir.
//!
//! Parsing is done by lexbor, a C implementation of the HTML5 parsing spec.
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

mod document;
pub mod extract;
mod ffi;
mod index;
mod query;
mod xpath;

pub use document::{Document, Node, NodeId, NodeKind, ParseTimeout};
pub use query::{Hit, Output, Query, QueryError};
pub use xpath::{XPath, XPathError, XValue};
