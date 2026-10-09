use netweir_dom::{Document, Output, Query};

const PAGE: &str = r#"
<html><body>
  <div id="main">
    <h1>All products</h1>
    <article class="product"><h3><a href="/a" title="Book A">A</a></h3><p class="price">£1.00</p></article>
    <article class="product"><h3><a href="/b" title="Book B">B</a></h3><p class="price">£2.50</p></article>
    <article class="product sold"><h3><a href="/c">C</a></h3><p class="price"><span>£</span>3.75</p></article>
  </div>
  <ul><li class="next"><a href="page-2.html">next</a></li></ul>
</body></html>"#;

fn strings(css: &str) -> Vec<String> {
    let doc = Document::parse(PAGE);
    Query::new(css).unwrap().strings(doc.root())
}

#[test]
fn text_of_direct_children() {
    assert_eq!(strings("h1::text"), ["All products"]);
    assert_eq!(strings(".price::text"), ["£1.00", "£2.50", "3.75"]);
}

#[test]
fn text_of_all_descendants_with_a_space() {
    assert_eq!(strings(".sold .price ::text"), ["£", "3.75"]);
}

#[test]
fn attributes() {
    assert_eq!(strings("article h3 a::attr(href)"), ["/a", "/b", "/c"]);
    assert_eq!(strings("article a::attr(title)"), ["Book A", "Book B"]);
    assert_eq!(strings("li.next a::attr( href )"), ["page-2.html"]);
}

#[test]
fn deep_text_of_nested_matches_is_returned_once() {
    let doc = Document::parse("<div><div><p>hi</p></div><p>x</p></div><div>y</div>");
    let q = Query::new("div ::text").unwrap();
    assert_eq!(q.strings(doc.root()), ["hi", "x", "y"]);
}

#[test]
fn deep_text_of_a_deep_tree_is_linear() {
    let html = "<div>".repeat(20_000) + "x";
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    assert_eq!(Query::new("div ::text").unwrap().strings(doc.root()), ["x"]);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        start.elapsed()
    );
}

#[test]
fn valueless_attributes_come_back_empty() {
    let doc = Document::parse("<input disabled><input>");
    assert_eq!(
        Query::new("input::attr(disabled)")
            .unwrap()
            .strings(doc.root()),
        [""]
    );
}

#[test]
fn plain_selector_gives_text_content() {
    assert_eq!(strings(".sold .price"), ["£3.75"]);
}

#[test]
fn modern_selectors() {
    assert_eq!(strings("article:nth-child(3) a::text"), ["B"]);
    assert_eq!(strings("article:not(.sold) a::text"), ["A", "B"]);
    assert_eq!(strings("a[href^='/'][title$='B']::text"), ["B"]);
    assert_eq!(strings("#main > h1::text"), ["All products"]);
    assert_eq!(strings("article:has(.price span) a::text"), ["C"]);
    assert_eq!(strings(":is(h1, li) ::text"), ["All products", "next"]);
}

#[test]
fn a_match_for_two_selectors_is_returned_once() {
    assert_eq!(strings("h1, #main > h1, h1:not(p)").len(), 1);
}

#[test]
fn results_are_in_document_order() {
    assert_eq!(strings("li a, h1"), ["All products", "next"]);
}

#[test]
fn scoped_queries_see_only_below_the_scope() {
    let doc = Document::parse(PAGE);
    let articles = Query::new("article").unwrap().select(doc.root());
    assert_eq!(articles.len(), 3);
    let price = Query::new(".price::text").unwrap();
    assert_eq!(price.strings(articles[1]), ["£2.50"]);
    // The scope itself is not a candidate.
    assert!(
        Query::new("article")
            .unwrap()
            .select(articles[0])
            .is_empty()
    );
}

#[test]
fn bare_text_query_returns_text_of_the_scope() {
    let doc = Document::parse("<p>a<b>b</b>c</p>");
    assert_eq!(
        Query::new("::text").unwrap().strings(doc.root()),
        ["a", "b", "c"]
    );
}

#[test]
fn outputs_are_parsed() {
    assert_eq!(Query::new("a").unwrap().output(), &Output::Nodes);
    assert_eq!(
        Query::new("a::text").unwrap().output(),
        &Output::Text { deep: false }
    );
    assert_eq!(
        Query::new("a ::text").unwrap().output(),
        &Output::Text { deep: true }
    );
    assert_eq!(
        Query::new("a::attr(href)").unwrap().output(),
        &Output::Attr("href".into())
    );
}

#[test]
fn bad_queries_are_errors_not_panics() {
    for bad in [
        "",
        "   ",
        "div[",
        "a::attr(",
        "a::attr()",
        "a::attr(href",
        "a::attr(href)::text",
        "a::text::text",
        "a::attr(href))",
        "a::attr(a b)",
        "a::attr(::text)",
        "p >",
        "##x",
        "!!",
    ] {
        assert!(Query::new(bad).is_err(), "{bad:?} should be rejected");
    }
}

#[test]
fn queries_run_on_many_documents() {
    let q = Query::new("b::text").unwrap();
    for i in 0..100 {
        let doc = Document::parse(&format!("<b>{i}</b>"));
        assert_eq!(q.strings(doc.root()), [i.to_string()]);
    }
}
