//! Any bytes as HTML: parse within a budget, index, query and serialise.
#![no_main]

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, Query, XPath};

fuzz_target!(|data: &[u8]| {
    let html = String::from_utf8_lossy(data);
    let Ok(doc) = Document::parse_within(&html, Duration::from_secs(1)) else {
        return;
    };
    let root = doc.root();
    let _ = root.html();
    let _ = root.text();
    let _ = XPath::new("count(//*)").unwrap().run(root);
    let _ = XPath::new("//*[last()]/text()").unwrap().run(root);
    let _ = Query::new("* ::text").unwrap().strings(root);
});
