//! HTML parsing and CSS queries for netweir.
//!
//! Parsing is done by lexbor, a C implementation of the HTML5 parsing spec.
//! The tree stays in lexbor's memory; [`Node`] is a borrowed handle into it.

mod document;
mod ffi;
mod query;

pub use document::{Document, Node, NodeId, NodeKind, ParseTimeout};
pub use query::{Output, Query, QueryError};
