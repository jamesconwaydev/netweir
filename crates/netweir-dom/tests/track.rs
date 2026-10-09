//! Tracked selectors against hand-made redesigns: the element found before
//! a redesign is found again after it, or, when it's gone, nothing is.

use netweir_dom::track::{Fingerprint, THRESHOLD, relocate};
use netweir_dom::{Document, Query};

#[derive(serde::Deserialize)]
struct Case {
    case: String,
    query: String,
    after: Option<String>,
}

fn page(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/redesigns/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn every_redesign_case() {
    let cases: Vec<Case> = serde_json::from_str(&page("cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let before = Document::parse(&page(&format!("{}.before.html", case.case)));
        let after = Document::parse(&page(&format!("{}.after.html", case.case)));
        let query = Query::new(&case.query).unwrap();
        let found = query.select(before.root());
        let element = found
            .first()
            .unwrap_or_else(|| panic!("{}: no match before", case.case));
        let fp = Fingerprint::of(*element);
        assert!(
            query.select(after.root()).is_empty(),
            "{}: the selector still works",
            case.case
        );
        // The fingerprint survives being saved and read back.
        let fp = Fingerprint::from_json(&fp.to_json()).unwrap();
        let got = relocate(after.root(), &fp, THRESHOLD);
        let got_text = got.as_ref().map(|(n, _)| n.text().trim().to_string());
        let score = got.as_ref().map(|(_, s)| *s);
        let best = netweir_dom::track::best(after.root(), &fp).map(|(n, s)| (n.text(), s));
        println!("{:16} best {:?}", case.case, best);
        if got_text != case.after {
            failures.push(format!(
                "{}: got {got_text:?} ({score:?}), want {:?}",
                case.case, case.after
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_unchanged_page_scores_one() {
    let html = page("class-renamed.before.html");
    let doc = Document::parse(&html);
    let el = Query::new("p.price_color").unwrap().select(doc.root())[0];
    let (found, score) = relocate(doc.root(), &Fingerprint::of(el), THRESHOLD).unwrap();
    assert_eq!(found, el);
    assert!((score - 1.0).abs() < 1e-9, "{score}");
}
