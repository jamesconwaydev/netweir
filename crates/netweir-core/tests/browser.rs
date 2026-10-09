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

/// The first event about a request; warnings come and go with the
/// machine (a runner's Chrome may have no matching profile).
fn main(events: &[Event]) -> Option<&Event> {
    events.iter().find(|e| !matches!(e, Event::Warning { .. }))
}

/// Like `drain`, but closes each Chrome page as it arrives, as a spider's
/// callback returning would.
async fn drain_closing(c: &Crawler) -> Vec<(Event, std::time::Instant)> {
    let mut all = Vec::new();
    loop {
        let batch = tokio::time::timeout(Duration::from_secs(30), c.next(64))
            .await
            .expect("crawl stalled");
        if batch.is_empty() {
            return all;
        }
        for event in batch {
            if let Event::Fetched {
                page: Some(page), ..
            } = &event
            {
                page.close().await;
            }
            all.push((event, std::time::Instant::now()));
        }
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
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
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
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
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
    challenge_moving_on(
        "setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 200)",
    )
}

/// A challenge whose script `moves_on` once it has set the cookie.
fn challenge_moving_on(moves_on: &str) -> Page {
    Page::status(
        403,
        &format!("<title>Just a moment...</title><script>{moves_on}</script>"),
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
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
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
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
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
        matches!(main(&events), Some(Event::Blocked { .. })),
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
    let Some(Event::Failed { error, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert!(
        error.message.contains("/nonexistent/chrome"),
        "{}",
        error.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_that_moves_on_at_once_still_counts_as_passed() {
    if !have_chrome() {
        return;
    }
    // Real challenges navigate whenever they like, including before the
    // first page has been read.
    let site = Site::start(vec![
        (
            "/soon",
            challenge_moving_on(
                "setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 0)",
            ),
        ),
        (
            "/now",
            challenge_moving_on("document.cookie = 'pass=1; path=/'; location.reload()"),
        ),
        ("/warm", Page::html("warm")),
    ])
    .await;
    let c = crawler(settings(BrowserMode::OnBlock));
    // Chrome starts first, so what's timed is the challenges alone: slow
    // runners take seconds to start it.
    c.submit(request(0, site.url("/warm"), true));
    drain_closing(&c).await;
    c.submit(request(1, site.url("/soon"), false));
    c.submit(request(2, site.url("/now"), false));
    let started = std::time::Instant::now();
    let events = drain_closing(&c).await;
    let passed = events
        .iter()
        .filter(|(e, _)| matches!(e, Event::Fetched { response, .. } if response.status == 200))
        .count();
    assert_eq!(passed, 2, "{events:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(c.stats().browser_unblocked, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_request_that_cant_reach_chrome_says_why() {
    let site = Site::start(vec![("/guarded", challenge())]).await;
    let c = crawler(CrawlSettings {
        browser_launch: LaunchOptions {
            executable: Some("/nonexistent/chrome".into()),
            ..LaunchOptions::default()
        },
        ..settings(BrowserMode::OnBlock)
    });
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    assert!(
        matches!(main(&events), Some(Event::Blocked { .. })),
        "{events:?}"
    );
    let warned = events.iter().any(
        |e| matches!(e, Event::Warning { message } if message.contains("/nonexistent/chrome")),
    );
    assert!(warned, "{events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_requests_waiting_for_a_page_dont_hold_up_the_rest() {
    if !have_chrome() {
        return;
    }
    let slow = Page::html(BUILT_BY_SCRIPT).slow(Duration::from_millis(500));
    let site = Site::start(vec![
        ("/a", slow.clone()),
        ("/b", slow.clone()),
        ("/c", slow.clone()),
    ])
    .await;
    let other = Site::start(vec![("/plain", Page::html("plain"))]).await;
    let c = crawler(CrawlSettings {
        concurrency: 2,
        browser_pages: 1,
        ..settings(BrowserMode::Off)
    });
    for (i, path) in ["/a", "/b", "/c"].iter().enumerate() {
        c.submit(request(i as u64, site.url(path), true));
    }
    // Another host: localhost, where the browser requests go to 127.0.0.1.
    c.submit(request(
        9,
        format!("http://localhost:{}/plain", other.port),
        false,
    ));
    let events = drain_closing(&c).await;
    let order: Vec<u64> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(order.len(), 4, "{events:?}");
    // The plain page needn't wait behind Chrome's queue.
    assert!(order.iter().position(|&id| id == 9) < Some(2), "{order:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_in_chrome_is_tried_again() {
    if !have_chrome() {
        return;
    }
    let site = Site::scripted(vec![(
        "/flaky",
        vec![Page::status(503, "busy"), Page::html(BUILT_BY_SCRIPT)],
    )])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/flaky"), true));
    let events = drain_closing(&c).await;
    let statuses: Vec<u16> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { response, .. } => Some(response.status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [200], "{events:?}");
    assert_eq!(c.stats().retries, 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_chrome_that_dies_is_started_again() {
    if !have_chrome() {
        return;
    }
    let site = Site::start(vec![
        ("/a", Page::html(BUILT_BY_SCRIPT)),
        ("/b", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/a"), true));
    drain_closing(&c).await;
    let first = c.browser().await.expect("Chrome was started");
    unsafe { libc::kill(first.pid().unwrap() as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !first.is_closed() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.submit(request(2, site.url("/b"), true));
    let events = drain_closing(&c).await;
    assert!(
        events
            .iter()
            .any(|(e, _)| matches!(e, Event::Fetched { id: 2, .. })),
        "{events:?}"
    );
    assert_ne!(c.browser().await.unwrap().pid(), first.pid());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_chrome_follows_obeys_the_next_sites_robots_txt() {
    if !have_chrome() {
        return;
    }
    let other = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /secret\n"),
        ),
        ("/secret", Page::html("secret")),
        ("/open", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    // localhost, so it's another host from 127.0.0.1 to the crawl.
    let other_url = |path: &str| format!("http://localhost:{}{path}", other.port);
    let site = Site::start(vec![
        (
            "/to-secret",
            Page::status(302, "").with_header("location", &other_url("/secret")),
        ),
        (
            "/to-open",
            Page::status(302, "").with_header("location", &other_url("/open")),
        ),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/to-secret"), true));
    c.submit(request(2, site.url("/to-open"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::Robots
            }
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(e, _)| matches!(e, Event::Fetched { id: 2, .. })),
        "{events:?}"
    );
    assert!(
        !other.paths().iter().any(|p| p == "/secret"),
        "Chrome never asked for the disallowed page"
    );
    assert_eq!(c.stats().dropped_robots, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_with_retry_after_is_waited_out_and_tried_again_in_chrome() {
    if !have_chrome() {
        return;
    }
    let site = Site::scripted(vec![(
        "/busy",
        vec![
            Page::status(503, "busy").with_header("retry-after", "0"),
            Page::html(BUILT_BY_SCRIPT),
        ],
    )])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/busy"), true));
    let events = drain_closing(&c).await;
    let statuses: Vec<u16> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { response, .. } => Some(response.status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [200], "{events:?}");
    let stats = c.stats();
    assert_eq!((stats.retries, stats.throttled), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_a_pages_script_starts_obeys_robots_txt_too() {
    if !have_chrome() {
        return;
    }
    let other = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /secret\n"),
        ),
        ("/secret", Page::html("secret")),
    ])
    .await;
    let secret = format!("http://localhost:{}/secret", other.port);
    let site = Site::start(vec![(
        "/moves",
        Page::html(&format!("<script>location.replace('{secret}')</script>")),
    )])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/moves"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::Robots
            }
        )),
        "{events:?}"
    );
    assert!(!other.paths().iter().any(|p| p == "/secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_chrome_follows_obeys_the_next_sites_tdmrep() {
    if !have_chrome() {
        return;
    }
    let tdm = r#"[{"location": "/reserved", "tdm-reservation": 1}]"#;
    let other = Site::start(vec![
        ("/.well-known/tdmrep.json", Page::status(200, tdm)),
        ("/reserved", Page::html("reserved")),
    ])
    .await;
    let site = Site::start(vec![(
        "/go",
        Page::status(302, "").with_header(
            "location",
            &format!("http://localhost:{}/reserved", other.port),
        ),
    )])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/go"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::TdmReserved
            }
        )),
        "{events:?}"
    );
    assert!(!other.paths().iter().any(|p| p == "/reserved"));
}
