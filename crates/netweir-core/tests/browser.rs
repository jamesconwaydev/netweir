//! Requests fetched in Chrome: asked for, always, or when a site keeps
//! blocking. Skipped without Chrome unless NETWEIR_REQUIRE_CHROME is set.

mod site;

use std::time::Duration;

use netweir_browser::LaunchOptions;
use netweir_core::{
    BrowserMode, CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile,
};
use site::{Page, Site};

fn have_chrome() -> bool {
    match netweir_browser::find_chrome() {
        Ok(_) => true,
        Err(e) if std::env::var_os("NETWEIR_REQUIRE_CHROME").is_some() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            false
        }
    }
}

fn settings(browser: BrowserMode) -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        obey_robots: false,
        obey_tdmrep: false,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        retries: 1,
        browser,
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

fn request(id: u64, url: String, browser: bool) -> CrawlRequest {
    CrawlRequest {
        id,
        url,
        priority: 0,
        headers: Vec::new(),
        dont_filter: false,
        depth: 0,
        browser,
    }
}

async fn drain(c: &Crawler) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let batch = tokio::time::timeout(Duration::from_secs(30), c.next(64))
            .await
            .expect("crawl stalled");
        if batch.is_empty() {
            return all;
        }
        all.extend(batch);
    }
}

const BUILT_BY_SCRIPT: &str = "<p id=static>static</p><script>document.body.insertAdjacentHTML('beforeend', '<p id=made>made by script</p>')</script>";

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_request_comes_back_rendered_with_its_page_open() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![("/", Page::html(BUILT_BY_SCRIPT))]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert_eq!(response.status, 200);
    assert!(
        response.text().contains(r#"<p id="made">"#),
        "{}",
        response.text()
    );
    let page = page.as_ref().expect("the live page comes with it");
    assert_eq!(
        page.page()
            .evaluate("document.querySelector('#made').textContent")
            .await
            .unwrap(),
        "made by script"
    );
    page.close().await;
    assert!(page.page().is_closed());
    let stats = c.stats();
    assert_eq!(stats.browser_fetches, 1);
    c.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn other_requests_still_use_the_http_client() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![("/", Page::html(BUILT_BY_SCRIPT))]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert!(page.is_none());
    assert!(!response.text().contains(r#"<p id="made">"#));
    assert_eq!(c.stats().browser_fetches, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_browser_always_every_request_goes_through_chrome() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![
        ("/a", Page::html(BUILT_BY_SCRIPT)),
        ("/b", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    let c = crawler(settings(BrowserMode::Always));
    c.submit(request(1, site.url("/a"), false));
    c.submit(request(2, site.url("/b"), false));
    let events = drain(&c).await;
    let rendered = events
        .iter()
        .filter(|e| matches!(e, Event::Fetched { response, .. } if response.text().contains(r#"<p id="made">"#)))
        .count();
    assert_eq!(rendered, 2, "{events:?}");
    assert_eq!(c.stats().browser_fetches, 2);
}

/// A challenge that a browser passes: its script sets a cookie and
/// reloads, and with the cookie the site serves the page.
fn challenge() -> Page {
    Page::status(
        403,
        "<title>Just a moment...</title><script>setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 200)</script>",
    )
    .with_header("content-type", "text/html")
    .with_header("cf-mitigated", "challenge")
    .unless_cookie("pass=1", Page::html("<p id=prize>the page</p>"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_request_gets_through_in_chrome_and_hands_its_cookies_back() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![("/guarded", challenge()), ("/also", challenge())]).await;
    let c = crawler(settings(BrowserMode::OnBlock));
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert_eq!(response.status, 200);
    assert!(response.text().contains("the page"));
    if let Some(page) = page {
        page.close().await;
    }
    let stats = c.stats();
    assert_eq!((stats.browser_fetches, stats.browser_unblocked), (1, 1));

    // The HTTP client now carries Chrome's cookie and isn't challenged.
    c.submit(request(2, site.url("/also"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert!(page.is_none(), "fetched over HTTP");
    assert_eq!(response.status, 200);
    assert_eq!(c.stats().browser_fetches, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_escalation_a_block_goes_to_on_block() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![("/guarded", challenge())]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    assert!(
        matches!(events.first(), Some(Event::Blocked { .. })),
        "{events:?}"
    );
    assert_eq!(c.stats().browser_fetches, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chrome_that_wont_start_fails_its_requests() {
    let site = Site::start(vec![("/", Page::html("x"))]).await;
    let c = crawler(CrawlSettings {
        browser_launch: LaunchOptions {
            executable: Some("/nonexistent/chrome".into()),
            ..LaunchOptions::default()
        },
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Failed { error, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert!(
        error.message.contains("/nonexistent/chrome"),
        "{}",
        error.message
    );
}
