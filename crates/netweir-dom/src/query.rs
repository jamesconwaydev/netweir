use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;

use crate::document::{Node, NodeId, NodeKind};
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

/// One result of a query.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit<'a> {
    /// An element, or with `::text` a text node.
    Node(Node<'a>),
    /// An attribute's value, from `::attr()`.
    Value(String),
}

impl Hit<'_> {
    /// What `get()` returns for this hit: an element or comment as HTML, a
    /// text node's text, an attribute's value.
    pub fn to_string_value(&self) -> String {
        match self {
            Hit::Node(n) if n.kind() == NodeKind::Text => n.data().unwrap_or_default().to_string(),
            Hit::Node(n) => n.html(),
            Hit::Value(v) => v.clone(),
        }
    }
}

/// A compiled CSS query, with Scrapy-style `::text` and `::attr()` endings.
/// A comma-separated list can give each selector its own ending; results
/// come back in document order.
///
/// Compile once, run on many documents, from any number of threads.
pub struct Query {
    parts: Vec<Part>,
}

struct Part {
    /// Null for an ending with no selector (`::text`), which matches the scope.
    list: *mut RawSelectorList,
    output: Output,
}

// SAFETY: each selector list is a self-contained allocation with no
// thread-local state, so moving it to another thread is fine. Sharing is
// fine too: matching creates its own lxb_selectors_t per call and takes the
// list as `const lxb_css_selector_list_t *`; lexbor's matcher
// (selectors/selectors.c, 3.0.0) writes only to that per-call state and never
// to the list. The list is freed only in Drop, which needs ownership.
unsafe impl Send for Query {}
unsafe impl Sync for Query {}

impl Query {
    pub fn new(css: &str) -> Result<Query, QueryError> {
        let error = |reason| QueryError {
            query: css.to_string(),
            reason,
        };
        // Owned by a Query from the start, so a later part failing to
        // compile frees the parts compiled before it.
        let mut query = Query { parts: Vec::new() };
        for piece in split_top_level(css) {
            let (selector, output) = split_output(piece).ok_or_else(|| error("bad ::attr()"))?;
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
            query.parts.push(Part { list, output });
        }
        Ok(query)
    }

    /// The ending of each selector in the list, in order.
    pub fn outputs(&self) -> Vec<Output> {
        self.parts.iter().map(|p| p.output.clone()).collect()
    }

    /// Elements below `scope` (not `scope` itself) matched by any selector
    /// in the list, before endings are applied, in document order. A
    /// selector that is only an ending, such as `::text`, matches `scope`.
    pub fn select<'a>(&self, scope: Node<'a>) -> Vec<Node<'a>> {
        let mut all: Vec<Node<'a>> = Vec::new();
        for part in &self.parts {
            all.extend(part.select(scope));
        }
        if self.parts.len() > 1 {
            sort_and_dedup(&mut all, |n| n.order());
        }
        all
    }

    /// The query's results, in document order.
    pub fn run<'a>(&self, scope: Node<'a>) -> Vec<Hit<'a>> {
        if let [part] = self.parts.as_slice() {
            return part.run(scope);
        }
        let mut keyed: Vec<((u32, u32), Hit<'a>)> = Vec::new();
        for part in &self.parts {
            keyed.extend(part.hits(scope, true));
        }
        // Equal keys are the same node or the same attribute.
        keyed.sort_by_key(|(k, _)| *k);
        keyed.dedup_by_key(|(k, _)| *k);
        keyed.into_iter().map(|(_, h)| h).collect()
    }

    /// `run`, as the strings `get()`/`getall()` give.
    pub fn strings(&self, scope: Node<'_>) -> Vec<String> {
        self.run(scope).iter().map(Hit::to_string_value).collect()
    }
}

fn sort_and_dedup<T: PartialEq>(items: &mut Vec<T>, key: impl Fn(&T) -> u32) {
    items.sort_by_key(|t| key(t));
    items.dedup();
}

impl Part {
    fn select<'a>(&self, scope: Node<'a>) -> Vec<Node<'a>> {
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

    fn run<'a>(&self, scope: Node<'a>) -> Vec<Hit<'a>> {
        self.hits(scope, false)
            .into_iter()
            .map(|(_, h)| h)
            .collect()
    }

    /// The hits, with their positions in document order if `keyed` (a key
    /// costs a call into lexbor and, on first use, the document index). An
    /// attribute value sorts just after its element and before the
    /// element's children (whose order is higher), in source order, as XPath
    /// orders attributes.
    fn hits<'a>(&self, scope: Node<'a>, keyed: bool) -> Vec<((u32, u32), Hit<'a>)> {
        let key = |n: Node<'_>, at: u32| if keyed { (n.order(), at) } else { (0, 0) };
        let mut matches = self.select(scope);
        if self.output == (Output::Text { deep: true }) {
            matches = outermost(matches);
        }
        let mut out = Vec::new();
        for node in matches {
            match &self.output {
                Output::Nodes => out.push((key(node, 0), Hit::Node(node))),
                // Unkeyed, one lookup is enough.
                Output::Attr(name) if !keyed => {
                    if let Some(v) = node.attr(name) {
                        out.push(((0, 0), Hit::Value(v.to_string())));
                    }
                }
                Output::Attr(name) => {
                    let attrs = node.attrs();
                    if let Some(at) = attrs.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) {
                        out.push((
                            key(node, at as u32 + 1),
                            Hit::Value(attrs[at].1.to_string()),
                        ));
                    }
                }
                Output::Text { deep } => {
                    let texts: Box<dyn Iterator<Item = Node<'_>>> = if *deep {
                        Box::new(node.descendants())
                    } else {
                        Box::new(node.children())
                    };
                    out.extend(
                        texts
                            .filter(|n| n.kind() == NodeKind::Text)
                            .map(|t| (key(t, 0), Hit::Node(t))),
                    );
                }
            }
        }
        out
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        for part in &self.parts {
            if !part.list.is_null() {
                unsafe { ffi::nw_css_free(part.list) }
            }
        }
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("outputs", &self.outputs())
            .finish()
    }
}

/// Splits a selector list at commas that are not inside parentheses,
/// brackets or quotes. An empty piece stays, so `a,` is rejected later.
fn split_top_level(css: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let (mut depth, mut quote, mut start, mut escaped) = (0i32, None, 0, false);
    for (i, c) in css.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                pieces.push(&css[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    pieces.push(&css[start..]);
    pieces
}

/// Drops matches that sit inside an earlier match, so `div ::text` on nested
/// `<div>`s returns each text node once, as Scrapy does. Each match walks up
/// only until it meets another match or the root.
fn outermost(matches: Vec<Node<'_>>) -> Vec<Node<'_>> {
    let matched: HashSet<NodeId> = matches.iter().map(Node::id).collect();
    matches
        .into_iter()
        .filter(|m| {
            let mut up = m.parent();
            while let Some(p) = up {
                if matched.contains(&p.id()) {
                    return false;
                }
                up = p.parent();
            }
            true
        })
        .collect()
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
        let bad = |c: char| c.is_whitespace() || "()\"'>/=:,[]".contains(c);
        if name.is_empty() || name.contains(bad) {
            return None;
        }
        return Some((&css[..start], Output::Attr(name.to_string())));
    }
    Some((css, Output::Nodes))
}
