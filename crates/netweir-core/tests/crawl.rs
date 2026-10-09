//! The crawl engine against a scripted local site.

mod site;

use std::time::{Duration, Instant};

use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, DropReason, Event, FetchOptions, Profile, Submitted,
};
use site::{Page, Site};

fn settings() -> CrawlSettings {
    CrawlSettings {
        // Fast by default here; throttling has its own tests.
        throttle: false,
        start_delay: Duration::ZERO,
        ..CrawlSettings::default()
    }
}

fn crawler(settings: CrawlSettings) -> Crawler {
    Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings,
    )
    .unwrap()
}

fn request(id: u64, url: String) -> CrawlRequest {
    CrawlRequest {
        id,
        url,
        priority: 0,
        headers: Vec::new(),
        dont_filter: false,
    }
}

/// Every event until the crawl is done.
async fn drain(c: &Crawler) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let batch = tokio::time::timeout(Duration::from_secs(20), c.next(64))
            .await
            .expect("crawl stalled");
        if batch.is_empty() {
            return all;
        }
        all.extend(batch);
    }
}

fn fetched(events: &[Event]) -> Vec<u64> {
    let mut ids: Vec<u64> = events
        .iter()
        .filter_map(|e| {
            if let Event::Fetched { id, .. } = e {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    ids.sort();
    ids
}

fn dropped(events: &[Event]) -> Vec<(u64, DropReason)> {
    let mut out: Vec<(u64, DropReason)> = events
        .iter()
        .filter_map(|e| {
            if let Event::Dropped { id, reason } = e {
                Some((*id, *reason))
            } else {
                None
            }
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

#[tokio::test]
async fn crawls_until_there_is_nothing_left() {
    let site = Site::start(vec![
        ("/a", Page::html("a")),
        ("/b", Page::html("b")),
        ("/c", Page::html("c")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/a", "/b", "/c"].iter().enumerate() {
        assert_eq!(c.submit(request(i as u64, site.url(p))), Submitted::Queued);
    }
    let events = drain(&c).await;
    assert_eq!(fetched(&events), [0, 1, 2]);
    let body = events.iter().find_map(|e| match e {
        Event::Fetched { id: 1, response } => Some(response.text()),
        _ => None,
    });
    assert_eq!(body.as_deref(), Some("b"));
    let stats = c.stats();
    assert_eq!(stats.fetched, 3);
    assert_eq!(stats.queued, 0);
}

#[tokio::test]
async fn the_same_page_is_fetched_once() {
    let site = Site::start(vec![("/p?a=1&b=2", Page::html("p"))]).await;
    let c = crawler(settings());
    assert_eq!(
        c.submit(request(1, site.url("/p?a=1&b=2"))),
        Submitted::Queued
    );
    assert_eq!(
        c.submit(request(2, site.url("/p?b=2&a=1#x"))),
        Submitted::Duplicate
    );
    let mut again = request(3, site.url("/p?a=1&b=2"));
    again.dont_filter = true;
    assert_eq!(
        c.submit(again),
        Submitted::Queued,
        "dont_filter skips the check"
    );
    assert_eq!(c.submit(request(4, "not a url".into())), Submitted::Invalid);
    assert_eq!(fetched(&drain(&c).await), [1, 3]);
    assert_eq!(c.stats().duplicates, 1);
}

#[tokio::test]
async fn robots_txt_is_read_first_and_obeyed() {
    let robots = "User-agent: *\nDisallow: /private\n\nUser-agent: netweir\nDisallow: /secret\n";
    let site = Site::start(vec![
        ("/robots.txt", Page::status(200, robots)),
        ("/open", Page::html("o")),
        ("/private/x", Page::html("p")),
        ("/secret", Page::html("s")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/open", "/private/x", "/secret"].iter().enumerate() {
        c.submit(request(i as u64, site.url(p)));
    }
    let events = drain(&c).await;
    // netweir's own group applies, not "*": /private is allowed, /secret isn't.
    assert_eq!(fetched(&events), [0, 1]);
    assert_eq!(dropped(&events), [(2, DropReason::Robots)]);
    let paths = site.paths();
    assert_eq!(paths.first().map(String::as_str), Some("/robots.txt"));
    assert_eq!(
        paths.iter().filter(|p| *p == "/robots.txt").count(),
        1,
        "read once per site"
    );
    assert!(!paths.contains(&"/secret".to_string()));
}

#[tokio::test]
async fn robots_txt_errors_follow_rfc_9309() {
    // 4xx: no rules, everything allowed.
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/a")));
    assert_eq!(fetched(&drain(&c).await), [1]);

    // 5xx: the site can't say, so nothing is allowed.
    let site = Site::start(vec![
        ("/robots.txt", Page::status(503, "")),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/a")));
    assert_eq!(dropped(&drain(&c).await), [(1, DropReason::Robots)]);
}

#[tokio::test]
async fn obey_robots_can_be_turned_off() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /\n"),
        ),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        ..settings()
    });
    c.submit(request(1, site.url("/a")));
    assert_eq!(fetched(&drain(&c).await), [1]);
    assert!(!site.paths().contains(&"/robots.txt".to_string()));
}

#[tokio::test]
async fn tdm_reservations_are_honoured() {
    let tdm = r#"[{"location": "/reserved/*", "tdm-reservation": 1}]"#;
    let site = Site::start(vec![
        ("/.well-known/tdmrep.json", Page::status(200, tdm)),
        ("/reserved/a", Page::html("r")),
        (
            "/header",
            Page::html("h").with_header("tdm-reservation", "1"),
        ),
        (
            "/meta",
            Page::html(r#"<head><meta name="tdm-reservation" content="1"></head>"#),
        ),
        ("/free", Page::html("f")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/reserved/a", "/header", "/meta", "/free"]
        .iter()
        .enumerate()
    {
        c.submit(request(i as u64, site.url(p)));
    }
    let events = drain(&c).await;
    assert_eq!(fetched(&events), [3]);
    assert_eq!(
        dropped(&events),
        [
            (0, DropReason::TdmReserved),
            (1, DropReason::TdmReserved),
            (2, DropReason::TdmReserved)
        ]
    );
    assert!(
        !site.paths().contains(&"/reserved/a".to_string()),
        "reserved by the file: never fetched"
    );

    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(9, site.url("/header")));
    assert_eq!(fetched(&drain(&c).await), [9]);
}

#[tokio::test]
async fn per_domain_concurrency_is_capped() {
    let slow = Duration::from_millis(200);
    let pages: Vec<(&str, Page)> = ["/1", "/2", "/3", "/4", "/5", "/6"]
        .iter()
        .map(|p| (*p, Page::html("x").slow(slow)))
        .collect();
    let site = Site::start(pages).await;
    let c = crawler(CrawlSettings {
        per_domain: 2,
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    let start = Instant::now();
    for i in 1..=6 {
        c.submit(request(i, site.url(&format!("/{i}"))));
    }
    assert_eq!(fetched(&drain(&c).await).len(), 6);
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(580),
        "6 x 200 ms, 2 at a time, took {took:?}"
    );
    assert!(took < Duration::from_millis(1500), "took {took:?}");
}

#[tokio::test]
async fn throttling_spaces_requests_to_one_site() {
    let site = Site::start(vec![
        ("/1", Page::html("x")),
        ("/2", Page::html("x")),
        ("/3", Page::html("x")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        throttle: true,
        start_delay: Duration::from_millis(300),
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    for i in 1..=3 {
        c.submit(request(i, site.url(&format!("/{i}"))));
    }
    drain(&c).await;
    let t = site.times("/");
    assert_eq!(t.len(), 3);
    // The delay starts at 300 ms and halves towards the (tiny) latency.
    assert!(
        t[1] - t[0] >= Duration::from_millis(280),
        "{:?}",
        t[1] - t[0]
    );
    assert!(
        t[2] - t[1] >= Duration::from_millis(130),
        "{:?}",
        t[2] - t[1]
    );
}

#[tokio::test]
async fn crawl_delay_in_robots_txt_is_a_floor() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nCrawl-delay: 0.3\n"),
        ),
        ("/1", Page::html("x")),
        ("/2", Page::html("x")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/1")));
    c.submit(request(2, site.url("/2")));
    drain(&c).await;
    let (one, two) = (site.times("/1")[0], site.times("/2")[0]);
    assert!(two - one >= Duration::from_millis(280), "{:?}", two - one);
}

#[tokio::test]
async fn higher_priority_goes_first() {
    let site = Site::start(vec![
        ("/low", Page::html("l")),
        ("/high", Page::html("h")),
        ("/first", Page::html("f")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        per_domain: 1,
        throttle: true,
        start_delay: Duration::from_millis(100),
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/first")));
    // Once /first is under way, the host's delay holds the next two back
    // long enough to be queued together.
    assert_eq!(fetched(&c.next(1).await), [1]);
    c.submit(request(2, site.url("/low")));
    let mut high = request(3, site.url("/high"));
    high.priority = 10;
    c.submit(high);
    drain(&c).await;
    assert_eq!(site.paths(), ["/first", "/high", "/low"]);
}

#[tokio::test]
async fn failures_are_events() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(7, format!("http://127.0.0.1:{port}/gone")));
    let events = drain(&c).await;
    assert!(
        matches!(&events[..], [Event::Failed { id: 7, error }] if error.kind == netweir_core::FetchErrorKind::Connect)
    );
    assert_eq!(c.stats().failed, 1);
}

#[tokio::test]
async fn requests_added_while_crawling_are_picked_up() {
    let site = Site::start(vec![
        ("/1", Page::html("x").slow(Duration::from_millis(100))),
        ("/2", Page::html("y")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/1")));
    let first = c.next(10).await;
    assert_eq!(fetched(&first), [1]);
    c.submit(request(2, site.url("/2")));
    assert_eq!(fetched(&drain(&c).await), [2]);
}
