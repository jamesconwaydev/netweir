//! HTML parsing and CSS queries for netweir.
//!
//! Parsing is done by lexbor, a C implementation of the HTML5 parsing spec.
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

mod document;
mod ffi;
mod index;
mod query;
mod xpath;

pub use document::{Document, Node, NodeId, NodeKind, ParseTimeout};
pub use query::{Hit, Output, Query, QueryError};
pub use xpath::{XPath, XPathError, XValue};
