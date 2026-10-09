use std::sync::Arc;
use std::thread;

use netweir_dom::{Document, Query};

#[test]
fn documents_parse_and_query_on_many_threads() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            thread::spawn(move || {
                let q = Query::new("li::text").unwrap();
                for i in 0..200 {
                    let doc = Document::parse(&format!("<ul><li>{t}-{i}</li></ul>"));
                    assert_eq!(q.strings(doc.root()), [format!("{t}-{i}")]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn one_document_is_shared_by_many_threads() {
    let html = "<ul>".to_string() + &"<li>x</li>".repeat(1000) + "</ul>";
    let doc = Arc::new(Document::parse(&html));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let doc = doc.clone();
            thread::spawn(move || {
                let q = Query::new("li::text").unwrap();
                for _ in 0..50 {
                    assert_eq!(q.strings(doc.root()).len(), 1000);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
