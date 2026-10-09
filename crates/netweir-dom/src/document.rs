use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ffi::{self, RawDocument, RawNode, str_at};

/// A parsed HTML document.
///
/// Built by lexbor following the HTML5 spec, so a page parses the way a
/// browser would parse it: missing `<html>`, `<head>`, `<body>` and `<tbody>`
/// elements are added, and misnested tags are repaired.
pub struct Document {
    raw: *mut RawDocument,
    /// Selector runs take this lock. lexbor does not promise that its matcher
    /// is safe with concurrent readers of one document, so we don't assume it.
    pub(crate) select_lock: Mutex<()>,
}

// SAFETY: the tree is never modified after `parse` returns. Every other
// access is a read through `Node`, except selector matching, which is
// serialised by `select_lock`.
unsafe impl Send for Document {}
unsafe impl Sync for Document {}

impl Document {
    pub fn parse(html: &str) -> Document {
        Document::parse_within(html, Duration::MAX).expect("an unlimited budget cannot run out")
    }

    /// Like [`Document::parse`], but gives up once `budget` has passed.
    ///
    /// The HTML5 spec makes some inputs, such as thousands of unclosed
    /// `<div>`s, take time quadratic in their nesting depth. A 400 KB page of
    /// them takes seconds. Browsers cap the depth; lexbor does not. A crawler
    /// fetching pages it does not control should parse with a budget.
    pub fn parse_within(html: &str, budget: Duration) -> Result<Document, ParseTimeout> {
        // ponytail: the budget is checked between 16 KB chunks, so a parse can
        // overrun it by the time one chunk takes. It bounds time, not memory:
        // a crafted page of distinct formatting tags makes the HTML5 adoption
        // agency clone thousands of elements per paragraph (64 KB reached
        // 6.7 GB in a test). A node cap checked between chunks comes with the
        // crawler.
        parse_in_chunks(html, budget, 16 * 1024)
    }

    /// The document node. Its children are the doctype and `<html>`.
    pub fn root(&self) -> Node<'_> {
        Node {
            doc: self,
            raw: unsafe { ffi::nw_root(self.raw) },
        }
    }

    /// Rebuilds a node from an id returned by [`Node::id`].
    ///
    /// # Safety
    /// `id` must come from a node of this same document.
    pub unsafe fn node(&self, id: NodeId) -> Node<'_> {
        Node {
            doc: self,
            raw: id.0 as *mut RawNode,
        }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        unsafe { ffi::nw_destroy(self.raw) }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Document")
    }
}

fn parse_in_chunks(html: &str, budget: Duration, chunk: usize) -> Result<Document, ParseTimeout> {
    let start = Instant::now();
    let raw = unsafe { ffi::nw_chunk_begin() };
    // lexbor only fails when it cannot allocate. Rust aborts on allocation
    // failure, so netweir treats lexbor's the same way.
    assert!(!raw.is_null(), "lexbor failed to allocate a document");
    let doc = Document {
        raw,
        select_lock: Mutex::new(()),
    };
    let mut rest = html;
    while !rest.is_empty() {
        if start.elapsed() > budget {
            return Err(ParseTimeout {
                budget,
                parsed: html.len() - rest.len(),
            });
        }
        let mut cut = rest.len().min(chunk);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (head, tail) = rest.split_at(cut);
        let status = unsafe { ffi::nw_chunk(doc.raw, head.as_ptr().cast(), head.len()) };
        assert_eq!(status, 0, "lexbor failed to allocate while parsing");
        rest = tail;
    }
    assert_eq!(
        unsafe { ffi::nw_chunk_end(doc.raw) },
        0,
        "lexbor failed to allocate while parsing"
    );
    Ok(doc)
}

/// Returned by [`Document::parse_within`] when parsing ran out of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTimeout {
    pub budget: Duration,
    /// Bytes parsed before giving up.
    pub parsed: usize,
}

impl fmt::Display for ParseTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parsing took longer than {:?} (gave up after {} bytes)",
            self.budget, self.parsed
        )
    }
}

impl std::error::Error for ParseTimeout {}

/// A node's identity, stable for the life of its document. Lets code that
/// cannot hold a borrow (such as the Python bindings) refer to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Doctype,
    Element,
    Text,
    Comment,
    Other,
}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) raw: *mut RawNode,
}

impl<'a> Node<'a> {
    pub(crate) fn wrap(doc: &'a Document, raw: *mut RawNode) -> Option<Node<'a>> {
        (!raw.is_null()).then_some(Node { doc, raw })
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.raw as usize)
    }

    pub fn kind(&self) -> NodeKind {
        match unsafe { ffi::nw_type(self.raw) } {
            ffi::ELEMENT => NodeKind::Element,
            ffi::TEXT | ffi::CDATA => NodeKind::Text,
            ffi::COMMENT => NodeKind::Comment,
            ffi::DOCUMENT => NodeKind::Document,
            ffi::DOCTYPE => NodeKind::Doctype,
            _ => NodeKind::Other,
        }
    }

    /// The lowercase tag name, for elements.
    pub fn tag(&self) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        Some(unsafe { str_at(ffi::nw_tag(self.raw, &mut len), len) })
    }

    pub fn attr(&self, name: &str) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        let ptr = unsafe { ffi::nw_get_attr(self.raw, name.as_ptr().cast(), name.len(), &mut len) };
        (!ptr.is_null()).then(|| unsafe { str_at(ptr, len) })
    }

    /// Every attribute as `(name, value)`, in source order.
    pub fn attrs(&self) -> Vec<(&'a str, &'a str)> {
        let mut out = Vec::new();
        if self.kind() != NodeKind::Element {
            return out;
        }
        let mut a = unsafe { ffi::nw_first_attr(self.raw) };
        while !a.is_null() {
            let (mut nl, mut vl) = (0, 0);
            let name = unsafe { str_at(ffi::nw_attr_name(a, &mut nl), nl) };
            let value = unsafe { str_at(ffi::nw_attr_value(a, &mut vl), vl) };
            out.push((name, value));
            a = unsafe { ffi::nw_next_attr(a) };
        }
        out
    }

    /// The node's own text, for text and comment nodes.
    pub fn data(&self) -> Option<&'a str> {
        match self.kind() {
            NodeKind::Text | NodeKind::Comment => {
                let mut len = 0;
                Some(unsafe { str_at(ffi::nw_char_data(self.raw, &mut len), len) })
            }
            _ => None,
        }
    }

    pub fn parent(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_parent(self.raw) })
    }

    pub fn children(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        std::iter::successors(
            Node::wrap(doc, unsafe { ffi::nw_first_child(self.raw) }),
            move |n| Node::wrap(doc, unsafe { ffi::nw_next(n.raw) }),
        )
    }

    /// Every node below this one, in document order.
    pub fn descendants(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        let top = self.raw;
        let mut next = unsafe { ffi::nw_first_child(top) };
        std::iter::from_fn(move || {
            let current = Node::wrap(doc, next)?;
            next = unsafe { ffi::nw_first_child(current.raw) };
            let mut up = current.raw;
            while next.is_null() && up != top {
                next = unsafe { ffi::nw_next(up) };
                up = unsafe { ffi::nw_parent(up) };
            }
            Some(current)
        })
    }

    /// All text below this node, joined. Comments are not text.
    pub fn text(&self) -> String {
        if let (NodeKind::Text, Some(d)) = (self.kind(), self.data()) {
            return d.to_string();
        }
        self.descendants()
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            NodeKind::Element => write!(f, "<{}>", self.tag().unwrap_or("?")),
            NodeKind::Text => write!(f, "{:?}", self.data().unwrap_or("")),
            kind => write!(f, "{kind:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(doc: &Document) -> Vec<String> {
        doc.root()
            .descendants()
            .map(|n| format!("{:?}{:?}", n, n.attrs()))
            .collect()
    }

    #[test]
    fn tiny_chunks_build_the_same_tree_as_one_chunk() {
        // 7-byte chunks split tags, entities, attribute values and multi-byte
        // characters at every offset.
        let html = "<!doctype html><p class=\"a b\">é€😀 &amp; &eacute;</p>\
                    <script>if (a < b) {}</script><textarea>x<y</textarea>\
                    <table>t<tr><td>c</table><svg><path d=\"M0 0\"/></svg>"
            .repeat(50);
        let one = parse_in_chunks(&html, Duration::MAX, usize::MAX).unwrap();
        let small = parse_in_chunks(&html, Duration::MAX, 7).unwrap();
        assert_eq!(shape(&one), shape(&small));
    }
}
