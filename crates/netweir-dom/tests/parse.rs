use std::time::{Duration, Instant};

use netweir_dom::{Document, NodeKind};

fn tags(doc: &Document) -> Vec<&str> {
    doc.root().descendants().filter_map(|n| n.tag()).collect()
}

#[test]
fn adds_the_structure_a_browser_would() {
    let doc = Document::parse("<title>t</title><p>one<p>two");
    assert_eq!(tags(&doc), ["html", "head", "title", "body", "p", "p"]);
}

#[test]
fn inserts_tbody_like_a_browser() {
    let doc = Document::parse("<table><tr><td>x</td></tr></table>");
    assert!(tags(&doc).contains(&"tbody"));
}

#[test]
fn decodes_entities_into_one_text_node() {
    let doc = Document::parse("<p>a&amp;b</p>");
    let texts: Vec<&str> = doc
        .root()
        .descendants()
        .filter(|n| n.kind() == NodeKind::Text)
        .filter_map(|n| n.data())
        .collect();
    assert_eq!(texts, ["a&b"]);
}

#[test]
fn moves_stray_table_text_before_the_table() {
    let doc = Document::parse("<table>oops<tr><td>cell</td></tr></table>");
    assert_eq!(doc.root().text(), "oopscell");
}

#[test]
fn reads_attributes() {
    let doc = Document::parse(r#"<a href="/x" class="btn primary" data-id=7>go</a>"#);
    let a = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("a"))
        .unwrap();
    assert_eq!(a.attr("href"), Some("/x"));
    assert_eq!(a.attr("missing"), None);
    assert_eq!(
        a.attrs(),
        [("href", "/x"), ("class", "btn primary"), ("data-id", "7")]
    );
}

#[test]
fn walks_parents_and_children() {
    let doc = Document::parse("<ul><li>a</li><li>b</li></ul>");
    let ul = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("ul"))
        .unwrap();
    let items: Vec<_> = ul.children().collect();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].text(), "b");
    assert_eq!(items[0].parent(), Some(ul));
}

#[test]
fn text_skips_comments() {
    let doc = Document::parse("<p>a<!-- hidden -->b</p>");
    assert_eq!(doc.root().text(), "ab");
}

#[test]
fn node_ids_round_trip() {
    let doc = Document::parse("<p>x</p>");
    let p = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("p"))
        .unwrap();
    let again = unsafe { doc.node(p.id()) };
    assert_eq!(again, p);
}

#[test]
fn survives_hostile_input() {
    let cases = [
        String::new(),
        "\0\0\0".to_string(),
        "<".repeat(10_000),
        "<div>".repeat(5_000),
        "<svg><foreignObject><math><mi><table><select><template><p>".repeat(500),
        String::from_utf8_lossy(&(0..=255u8).cycle().take(50_000).collect::<Vec<_>>()).into_owned(),
    ];
    for html in &cases {
        let doc = Document::parse(html);
        let _ = doc.root().text();
        let _ = doc.root().descendants().count();
    }
}

#[test]
fn chunked_parse_matches_whole_parse() {
    // Long enough to span many chunks, with multi-byte characters at every
    // possible offset against the chunk boundary.
    let html = "<p class=x>é€😀</p>".repeat(20_000);
    let whole = Document::parse(&html);
    let chunked = Document::parse_within(&html, Duration::from_secs(60)).unwrap();
    assert_eq!(whole.root().text(), chunked.root().text());
    assert_eq!(
        whole.root().descendants().count(),
        chunked.root().descendants().count()
    );
}

#[test]
fn deep_nesting_is_stopped_by_the_budget() {
    // Takes many seconds unbounded.
    let html = "<div>".repeat(200_000);
    let start = Instant::now();
    let err = Document::parse_within(&html, Duration::from_millis(200)).unwrap_err();
    assert!(err.parsed < html.len());
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "took {:?}",
        start.elapsed()
    );
}
