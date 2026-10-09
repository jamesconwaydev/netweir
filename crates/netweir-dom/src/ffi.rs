//! Declarations for `csrc/shim.c`. Everything here is unsafe to call; the
//! safe wrappers live in `document.rs` and `query.rs`.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct RawDocument {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RawNode {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RawAttr {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RawSelectorList {
    _private: [u8; 0],
}

pub type FoundFn = extern "C" fn(node: *mut RawNode, ctx: *mut c_void);

pub const ELEMENT: c_int = 0x01;
pub const TEXT: c_int = 0x03;
pub const CDATA: c_int = 0x04;
pub const COMMENT: c_int = 0x08;
pub const DOCUMENT: c_int = 0x09;
pub const DOCTYPE: c_int = 0x0A;

unsafe extern "C" {
    pub fn nw_chunk_begin() -> *mut RawDocument;
    pub fn nw_chunk(doc: *mut RawDocument, html: *const c_char, len: usize) -> c_int;
    pub fn nw_chunk_end(doc: *mut RawDocument) -> c_int;
    pub fn nw_destroy(doc: *mut RawDocument);
    pub fn nw_root(doc: *mut RawDocument) -> *mut RawNode;

    pub fn nw_parent(n: *mut RawNode) -> *mut RawNode;
    pub fn nw_first_child(n: *mut RawNode) -> *mut RawNode;
    pub fn nw_next(n: *mut RawNode) -> *mut RawNode;
    pub fn nw_type(n: *mut RawNode) -> c_int;
    pub fn nw_tag(n: *mut RawNode, len: *mut usize) -> *const c_char;
    pub fn nw_char_data(n: *mut RawNode, len: *mut usize) -> *const c_char;
    pub fn nw_get_attr(
        n: *mut RawNode,
        name: *const c_char,
        name_len: usize,
        len: *mut usize,
    ) -> *const c_char;
    pub fn nw_first_attr(n: *mut RawNode) -> *mut RawAttr;
    pub fn nw_next_attr(a: *mut RawAttr) -> *mut RawAttr;
    pub fn nw_attr_name(a: *mut RawAttr, len: *mut usize) -> *const c_char;
    pub fn nw_attr_value(a: *mut RawAttr, len: *mut usize) -> *const c_char;

    pub fn nw_css_compile(css: *const c_char, len: usize) -> *mut RawSelectorList;
    pub fn nw_css_free(list: *mut RawSelectorList);
    pub fn nw_select(
        root: *mut RawNode,
        list: *mut RawSelectorList,
        found: FoundFn,
        ctx: *mut c_void,
    ) -> c_int;
}

/// Borrows `len` bytes at `ptr` as a string.
///
/// # Safety
/// `ptr` must be null or point to `len` readable bytes that live at least as
/// long as `'a`. lexbor stores all names and text as UTF-8 (invalid input
/// becomes U+FFFD while parsing), so a failed check here means memory
/// corruption, not bad input, and we return "" rather than read on.
pub unsafe fn str_at<'a>(ptr: *const c_char, len: usize) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(bytes).unwrap_or("")
}
