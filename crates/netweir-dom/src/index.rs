//! A flat index of a document's nodes in document order. A node's
//! descendants are the contiguous range after it, so scanning them is a
//! pass over small arrays rather than a walk through lexbor's linked nodes.

use crate::document::{Document, Node};
use crate::ffi::{self, RawNode};

pub(crate) struct Index {
    raw: Vec<*mut RawNode>,
    /// Each node's parent's position; u32::MAX for the root.
    parent: Vec<u32>,
    /// One past the last descendant of each node.
    end: Vec<u32>,
    kind: Vec<u8>,
    tag: Vec<usize>,
}

// SAFETY: the pointers are into a document that is never modified after
// parsing and outlives the index (the Document owns it).
unsafe impl Send for Index {}
unsafe impl Sync for Index {}

impl Index {
    pub(crate) fn build(root: *mut RawNode) -> Index {
        let n = unsafe { ffi::nw_number(root) };
        let mut raw = vec![std::ptr::null_mut(); n];
        let mut parent = vec![0u32; n];
        let mut kind = vec![0u8; n];
        let mut tag = vec![0usize; n];
        unsafe {
            ffi::nw_index_fill(
                root,
                raw.as_mut_ptr(),
                parent.as_mut_ptr(),
                kind.as_mut_ptr(),
                tag.as_mut_ptr(),
            )
        };
        // Children come after their parent, so one backward pass carries
        // each subtree's end up to its parent.
        let mut end: Vec<u32> = (1..=n as u32).collect();
        for i in (1..n).rev() {
            let p = parent[i] as usize;
            if p < n && end[i] > end[p] {
                end[p] = end[i];
            }
        }
        Index {
            raw,
            parent,
            end,
            kind,
            tag,
        }
    }

    /// Appends to `out` the nodes below `at` (from `at` itself if
    /// `include_self`) that pass `keep(kind, tag)`, in document order.
    pub(crate) fn descendants<'a>(
        &self,
        doc: &'a Document,
        at: u32,
        include_self: bool,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<Node<'a>>,
    ) {
        let start = if include_self { at } else { at + 1 } as usize;
        for j in start..self.end[at as usize] as usize {
            if keep(self.kind[j], self.tag[j]) {
                out.push(Node::from_raw(doc, self.raw[j]));
            }
        }
    }

    /// The position of the parent of the node at `at`.
    pub(crate) fn parent_of(&self, at: u32) -> u32 {
        self.parent[at as usize]
    }

    /// Appends to `out` the children of `at` that pass `keep(kind, tag)`.
    pub(crate) fn children<'a>(
        &self,
        doc: &'a Document,
        at: u32,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<Node<'a>>,
    ) {
        let stop = self.end[at as usize] as usize;
        let mut c = at as usize + 1;
        while c < stop {
            if keep(self.kind[c], self.tag[c]) {
                out.push(Node::from_raw(doc, self.raw[c]));
            }
            c = self.end[c] as usize;
        }
    }
}
