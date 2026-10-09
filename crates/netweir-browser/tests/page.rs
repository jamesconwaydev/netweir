mod common;

use std::time::Duration;

use common::{Reply, browser, html, serve};
use netweir_browser::{Error, WaitUntil};
use serde_json::json;

const SHORT: Option<Duration> = Some(Duration::from_millis(800));

#[tokio::test]
async fn goto_reports_the_documents_status_headers_and_final_url() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            Reply {
                status: 200,
                headers: vec![
                    ("Content-Type", "text/html".into()),
                    ("X-Test", "yes".into()),
                ],
                body: "<title>home</title>".into(),
            },
        ),
        (
            "/moved",
            Reply {
                status: 302,
                headers: vec![("Location", "/".into())],
                body: String::new(),
            },
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    let r = page
        .goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert!(
        r.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-test") && v == "yes"),
        "{:?}",
        r.headers
    );
    assert_eq!(page.title().await.unwrap(), "home");

    let r = page
        .goto(&format!("{}/missing", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.status, 404, "an HTTP error is returned, not raised");

    let r = page
        .goto(
            &format!("{}/moved", server.url),
            WaitUntil::DomContentLoaded,
            None,
        )
        .await
        .unwrap();
    assert_eq!((r.status, r.url.clone()), (200, format!("{}/", server.url)));
    assert_eq!(page.url(), format!("{}/", server.url));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_refused_connection_is_a_navigation_error() {
    let Some(browser) = browser().await else {
        return;
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let page = browser.new_page().await.unwrap();
    let err = page
        .goto(&format!("http://127.0.0.1:{port}/"), WaitUntil::Load, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation(m) if m.contains("ERR_CONNECTION_REFUSED")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn content_is_the_page_after_its_scripts_ran() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            "<!DOCTYPE html><body><script>document.body.insertAdjacentHTML('beforeend', '<p id=made>by script</p>')</script></body>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let html = page.content().await.unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
    assert!(html.contains(r#"<p id="made">by script</p>"#), "{html}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_runs_in_the_page_and_its_helpers_stay_hidden() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<script>var pageVar = 41</script>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(page.evaluate("pageVar + 1").await.unwrap(), json!(42));
    assert_eq!(
        page.evaluate("() => [1, 'a', {b: null}]").await.unwrap(),
        json!([1, "a", {"b": null}])
    );
    assert_eq!(
        page.evaluate("new Promise(r => setTimeout(() => r('later'), 10))")
            .await
            .unwrap(),
        json!("later")
    );
    assert_eq!(page.evaluate("undefined").await.unwrap(), json!(null));
    let err = page
        .evaluate("(() => { throw new Error('boom') })()")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("boom")),
        "{err}"
    );
    // The helpers live in an isolated world.
    page.content().await.unwrap();
    assert_eq!(
        page.evaluate("typeof __netweir").await.unwrap(),
        json!("undefined")
    );
    browser.close().await.unwrap();
}

const ACTIONS: &str = r#"<!DOCTYPE html>
<style>
  button { width: 120px; height: 40px; }
  #wrap { position: relative; width: 120px; height: 40px; }
  #cover { position: absolute; left: 0; top: 0; width: 100%; height: 100%; }
</style>
<script>
  window.clicks = [];
  const later = (f) => setTimeout(f, 300);
  addEventListener('click', (e) => e.target.id && clicks.push(e.target.id));
</script>
<button id=shows style="display:none">a</button>
<button id=enables disabled>b</button>
<button id=never style="visibility:hidden">c</button>
<div id=wrap><button id=covered>d</button><div id=cover></div></div>
<form id=form onsubmit="event.preventDefault(); window.submitted = document.querySelector('#name').value">
  <input id=name value="old">
</form>
<input id=fixed readonly value="x">
<script>
  later(() => {
    document.querySelector('#shows').style.display = '';
    document.querySelector('#enables').disabled = false;
  });
</script>
"#;

#[tokio::test]
async fn click_waits_until_the_element_can_be_clicked() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ACTIONS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#shows", None).await.unwrap();
    page.click("#enables", None).await.unwrap();

    let err = page.click("#never", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("visible")),
        "{err}"
    );
    let err = page.click("#covered", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("covering")),
        "{err}"
    );
    let err = page.click("#nothing", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("nothing matched")),
        "{err}"
    );

    page.evaluate("document.querySelector('#cover').remove()")
        .await
        .unwrap();
    page.click("#covered", None).await.unwrap();
    assert_eq!(
        page.evaluate("clicks").await.unwrap(),
        json!(["shows", "enables", "covered"])
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn fill_replaces_the_text_and_press_sends_keys() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ACTIONS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.evaluate("document.querySelector('#cover').remove()")
        .await
        .unwrap();
    page.fill("#name", "hello", None).await.unwrap();
    page.press("!").await.unwrap();
    page.press("Enter").await.unwrap();
    assert_eq!(page.evaluate("submitted").await.unwrap(), json!("hello!"));
    page.fill("#name", "", None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#name').value")
            .await
            .unwrap(),
        json!("")
    );
    let err = page.fill("#fixed", "y", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("editable")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn wait_for_follows_the_element_through_its_states() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            "<script>setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<p id=late>here</p>'), 300)</script><p id=gone>x</p><script>setTimeout(() => document.querySelector('#gone').remove(), 300)</script>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.wait_for("#late", "visible", None).await.unwrap();
    page.wait_for("#gone", "detached", None).await.unwrap();
    page.wait_for("#gone", "hidden", None).await.unwrap();
    let err = page
        .wait_for("#never", "attached", SHORT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout(_)));
    assert!(matches!(
        page.wait_for("#late", "shown", None).await,
        Err(Error::Invalid(_))
    ));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn screenshots_are_png() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html("<div style='height:3000px;background:red'></div>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let viewport = page.screenshot(false).await.unwrap();
    let full = page.screenshot(true).await.unwrap();
    assert!(viewport.starts_with(b"\x89PNG\r\n\x1a\n"));
    // Height is bytes 20..24 of the IHDR chunk.
    let height = |png: &[u8]| u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert!(
        height(&full) >= 3000 && height(&viewport) < 3000,
        "{} {}",
        height(&full),
        height(&viewport)
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn pages_keep_their_cookies_to_their_own_context() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html".into()),
                ("Set-Cookie", "session=abc; Path=/; HttpOnly".into()),
            ],
            body: "<p>hi</p>".into(),
        },
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let cookies = page.cookies().await.unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c.name == "session" && c.value == "abc" && c.http_only),
        "{cookies:?}"
    );

    let other = browser.new_page().await.unwrap();
    assert!(
        other.cookies().await.unwrap().is_empty(),
        "a new page shares nothing"
    );

    let context = browser.new_context().await.unwrap();
    let (a, b) = (
        context.new_page().await.unwrap(),
        context.new_page().await.unwrap(),
    );
    a.set_cookies(&cookies).await.unwrap();
    assert_eq!(
        b.cookies().await.unwrap(),
        cookies,
        "pages in one context share cookies"
    );
    context.close().await.unwrap();
    page.close().await.unwrap();
    assert!(page.is_closed());
    browser.close().await.unwrap();
}

#[tokio::test]
async fn closing_a_page_ends_a_navigation_waiting_on_it() {
    let Some(browser) = browser().await else {
        return;
    };
    // Accepts and never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", silent.local_addr().unwrap());
    let page = browser.new_page().await.unwrap();
    let waiting = tokio::spawn({
        let page = page.clone();
        async move {
            page.goto(&url, WaitUntil::Load, Some(Duration::from_secs(20)))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    page.close().await.unwrap();
    let r = waiting.await.unwrap();
    assert!(matches!(r, Err(Error::PageClosed)), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_returns_values_json_cant_hold() {
    let Some(browser) = browser().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    assert_eq!(page.evaluate("NaN").await.unwrap(), json!("NaN"));
    assert_eq!(
        page.evaluate("-Infinity").await.unwrap(),
        json!("-Infinity")
    );
    assert_eq!(
        page.evaluate("10n ** 20n").await.unwrap(),
        json!("100000000000000000000")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn fill_refuses_a_select() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html("<select id=s><option>a</option></select>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page.fill("#s", "a", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("editable")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn an_endless_timeout_is_capped_not_a_panic() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<p id=x>x</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, Some(Duration::MAX))
        .await
        .unwrap();
    page.click("#x", Some(Duration::MAX)).await.unwrap();
    page.wait_for("#x", "visible", Some(Duration::MAX))
        .await
        .unwrap();
    browser.close().await.unwrap();
}
