use std::ffi::c_void;
use std::fmt;

use crate::document::{Node, NodeKind};
use crate::ffi::{self, RawNode, RawSelectorList};

/// What a query returns for each matched element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The elements themselves.
    Nodes,
    /// `::text`: the text nodes directly inside each match. Written with a
    /// space before it (`div ::text`), every text node below each match.
    Text { deep: bool },
    /// `::attr(name)`: the attribute's value. Matches without it are skipped.
    Attr(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub query: String,
    pub reason: &'static str,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid selector {:?}: {}", self.query, self.reason)
    }
}

impl std::error::Error for QueryError {}

/// A compiled CSS query, with Scrapy-style `::text` and `::attr()` endings.
///
/// Compile once, run on many documents. A `Query` can move between threads
/// but not be shared by them at once: lexbor does not say its compiled
/// selectors are safe to match from two threads, so each thread compiles its
/// own.
pub struct Query {
    list: *mut RawSelectorList,
    output: Output,
}

// SAFETY: the selector list is a self-contained allocation with no
// thread-local state, so moving it to another thread is fine. Not `Sync`.
unsafe impl Send for Query {}

impl Query {
    pub fn new(css: &str) -> Result<Query, QueryError> {
        let error = |reason| QueryError {
            query: css.to_string(),
            reason,
        };
        let (selector, output) = split_output(css).ok_or_else(|| error("bad ::attr()"))?;
        let list = if selector.trim().is_empty() {
            if output == Output::Nodes {
                return Err(error("empty selector"));
            }
            std::ptr::null_mut()
        } else {
            let list = unsafe { ffi::nw_css_compile(selector.as_ptr().cast(), selector.len()) };
            if list.is_null() {
                return Err(error("not valid CSS"));
            }
            list
        };
        Ok(Query { list, output })
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    /// Elements below `scope` (not `scope` itself) that match, in document
    /// order. A query that is only an ending, such as `::text`, returns
    /// `scope` itself.
    pub fn select<'a>(&self, scope: Node<'a>) -> Vec<Node<'a>> {
        if self.list.is_null() {
            return vec![scope];
        }
        extern "C" fn found(node: *mut RawNode, ctx: *mut c_void) {
            unsafe { (*ctx.cast::<Vec<*mut RawNode>>()).push(node) }
        }
        let mut raw: Vec<*mut RawNode> = Vec::new();
        {
            let _guard = scope
                .doc
                .select_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Only fails when lexbor cannot allocate; return what was found.
            unsafe {
                ffi::nw_select(
                    scope.raw,
                    self.list,
                    found,
                    (&mut raw as *mut Vec<_>).cast(),
                )
            };
        }
        raw.into_iter()
            .filter_map(|r| Node::wrap(scope.doc, r))
            .collect()
    }

    /// The query's results as strings: text, attribute values, or for a plain
    /// selector, the text content of each match.
    pub fn strings(&self, scope: Node<'_>) -> Vec<String> {
        let mut out = Vec::new();
        for node in self.select(scope) {
            match &self.output {
                Output::Nodes => out.push(node.text()),
                Output::Attr(name) => out.extend(node.attr(name).map(str::to_string)),
                Output::Text { deep } => {
                    let texts: Box<dyn Iterator<Item = Node<'_>>> = if *deep {
                        Box::new(node.descendants())
                    } else {
                        Box::new(node.children())
                    };
                    out.extend(
                        texts
                            .filter(|n| n.kind() == NodeKind::Text)
                            .filter_map(|n| n.data())
                            .map(str::to_string),
                    );
                }
            }
        }
        out
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        if !self.list.is_null() {
            unsafe { ffi::nw_css_free(self.list) }
        }
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("output", &self.output)
            .finish()
    }
}

/// Splits `a::attr(href)` into `("a", Attr("href"))`. `None` means a
/// malformed `::attr(...)`.
fn split_output(css: &str) -> Option<(&str, Output)> {
    let css = css.trim_end();
    if let Some(selector) = css.strip_suffix("::text") {
        let deep = selector.is_empty() || selector.ends_with(char::is_whitespace);
        return Some((selector, Output::Text { deep }));
    }
    if let Some(start) = css.rfind("::attr(") {
        let name = css[start + "::attr(".len()..].strip_suffix(')')?.trim();
        if name.is_empty() {
            return None;
        }
        return Some((&css[..start], Output::Attr(name.to_string())));
    }
    Some((css, Output::Nodes))
}
