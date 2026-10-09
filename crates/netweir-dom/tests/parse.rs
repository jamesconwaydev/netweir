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
fn boolean_attributes_read_as_empty_not_missing() {
    let doc = Document::parse("<input disabled>");
    let input = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("input"))
        .unwrap();
    assert_eq!(input.attr("disabled"), Some(""));
    assert_eq!(input.attr("checked"), None);
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

#[test]
fn siblings_both_ways() {
    let doc = Document::parse("<ul><li>a</li>text<li>b</li></ul>");
    let ul = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("ul"))
        .unwrap();
    let kids: Vec<_> = ul.children().collect();
    assert_eq!(kids.len(), 3);
    assert_eq!(kids[1].prev_sibling(), Some(kids[0]));
    assert_eq!(kids[1].next_sibling(), Some(kids[2]));
    assert_eq!(kids[0].prev_sibling(), None);
    assert_eq!(kids[2].next_sibling(), None);
}

#[test]
fn outer_html_serialises_like_a_browser() {
    let doc =
        Document::parse(r#"<div id=x class="a b"><p>one &amp; <b>two</b></p><br><!--c--></div>"#);
    let div = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("div"))
        .unwrap();
    assert_eq!(
        div.html(),
        r#"<div id="x" class="a b"><p>one &amp; <b>two</b></p><br><!--c--></div>"#
    );
    let text = div
        .descendants()
        .find(|n| n.kind() == NodeKind::Text)
        .unwrap();
    assert_eq!(text.html(), "one &amp; ");
    let comment = div
        .descendants()
        .find(|n| n.kind() == NodeKind::Comment)
        .unwrap();
    assert_eq!(comment.html(), "<!--c-->");
}

#[test]
fn document_order_ranks_every_node() {
    let doc = Document::parse("<p>a<b>b</b>c</p><i>d</i>");
    let all: Vec<_> = doc.root().descendants().collect();
    let ranks: Vec<u32> = all.iter().map(|n| n.order()).collect();
    let mut sorted = ranks.clone();
    sorted.sort();
    assert_eq!(ranks, sorted, "descendants() is document order");
    assert!(doc.root().order() < ranks[0]);
    let mut unique = ranks.clone();
    unique.dedup();
    assert_eq!(unique.len(), ranks.len());
}

#[test]
fn stepping_through_document_order_both_ways() {
    let doc = Document::parse("<div><p>a<b>b</b></p>c</div><i>d</i>");
    let forward: Vec<String> = std::iter::successors(Some(doc.root()), |n| n.next_in_order())
        .map(|n| format!("{n:?}"))
        .collect();
    let all: Vec<String> = std::iter::once(doc.root())
        .chain(doc.root().descendants())
        .map(|n| format!("{n:?}"))
        .collect();
    assert_eq!(forward, all);
    let last = doc.root().descendants().last().unwrap();
    let mut backward: Vec<String> = std::iter::successors(Some(last), |n| n.prev_in_order())
        .map(|n| format!("{n:?}"))
        .collect();
    backward.reverse();
    assert_eq!(backward, all);
    let div = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("div"))
        .unwrap();
    assert_eq!(div.last_child().unwrap().data(), Some("c"));
}

#[test]
fn readable_text_leaves_out_code() {
    let doc = Document::parse(
        "<div>a<script>var x</script><style>p{}</style>b<template>t</template><noscript>n</noscript></div>",
    );
    let div = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("div"))
        .unwrap();
    assert_eq!(div.readable_text_pieces(), ["a", "b", "n"]);
    // Asked directly, a script or style element gives its own text.
    let style = div
        .descendants()
        .find(|n| n.tag() == Some("style"))
        .unwrap();
    assert_eq!(style.readable_text_pieces(), ["p{}"]);
    // XPath's string value still includes everything.
    assert_eq!(div.text(), "avar xp{}btn");
}

#[test]
fn templates_are_found_whatever_their_case_and_wherever_a_chunk_ends() {
    // Long enough to be parsed in several chunks, with the tag across a
    // chunk boundary.
    let pad = "x".repeat(16 * 1024 - 6);
    let html = format!("<div>{pad}<TeMpLaTe><p>in</p></TEMPLATE></div>");
    let doc = Document::parse(&html);
    let p = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("p"))
        .unwrap();
    assert_eq!(p.parent().and_then(|t| t.tag()), Some("template"));
}

#[test]
fn template_contents_are_the_templates_children() {
    let html =
        "<div>a<template><p class=x>in</p>tx<template><i>deep</i></template></template>b</div>";
    let doc = Document::parse(html);
    let template = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("template"))
        .unwrap();
    let tags: Vec<_> = template
        .children()
        .map(|c| c.tag().unwrap_or("#text"))
        .collect();
    assert_eq!(tags, ["p", "#text", "template"]);
    assert_eq!(
        doc.root()
            .descendants()
            .filter(|n| n.tag() == Some("i"))
            .count(),
        1
    );
    // Serialised once, not twice.
    assert_eq!(
        template.html(),
        "<template><p class=\"x\">in</p>tx<template><i>deep</i></template></template>"
    );
}
