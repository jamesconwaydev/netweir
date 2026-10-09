mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::{chrome, html, serve};
use netweir_browser::{Browser, LaunchOptions, WaitUntil};
use serde_json::json;

/// A Chrome started the usual way, with DevTools on a port, as one
/// running elsewhere would be.
struct Running {
    child: std::process::Child,
    ws: String,
    profile: std::path::PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn running() -> Option<Running> {
    let executable = chrome()?;
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // One each: Chromes sharing a profile hand off to the first.
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let profile = std::env::temp_dir().join(format!("netweir-running-{}-{n}", std::process::id()));
    let mut child = Command::new(executable)
        .args([
            "--headless",
            "--remote-debugging-port=0",
            "--no-first-run",
            &format!("--user-data-dir={}", profile.display()),
            "about:blank",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let ws = stderr
        .lines()
        .map_while(Result::ok)
        .find_map(|l| l.strip_prefix("DevTools listening on ").map(str::to_string))
        .expect("Chrome printed its DevTools URL");
    Some(Running { child, ws, profile })
}

#[tokio::test]
async fn a_running_browser_is_driven_and_left_running() {
    let Some(mut chrome) = running() else {
        return;
    };
    let server = serve(vec![("/", html("<p id=x>hello</p>"))]);
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    assert!(browser.version().starts_with("Chrome/"));
    assert_eq!(browser.profile_dir(), None);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#x').textContent")
            .await
            .unwrap(),
        json!("hello")
    );
    // It still looks like Chrome, not a headless one.
    let agent = page.evaluate("navigator.userAgent").await.unwrap();
    assert!(!agent.as_str().unwrap().contains("Headless"), "{agent}");
    browser.close().await.unwrap();
    assert!(page.is_closed() || page.title().await.is_err());
    assert!(
        chrome.child.try_wait().unwrap().is_none(),
        "Chrome kept running"
    );

    // And through the http:// address the WebSocket URL is listed at.
    let port = chrome
        .ws
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let again = Browser::connect(
        &format!("http://127.0.0.1:{port}"),
        LaunchOptions::default(),
    )
    .await
    .unwrap();
    let page = again.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    again.close().await.unwrap();
}

#[tokio::test]
async fn nothing_listening_is_an_error_that_says_where() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = Browser::connect(
        &format!("ws://127.0.0.1:{port}/devtools/browser/x"),
        LaunchOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.to_string().contains(&port.to_string()), "{err}");
}

/// The page targets a DevTools HTTP endpoint lists.
fn page_targets(port: &str) -> usize {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(
        stream,
        "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    String::from_utf8_lossy(&reply)
        .matches("\"type\": \"page\"")
        .count()
}

#[tokio::test]
async fn a_connected_browser_dropped_without_closing_cleans_up_after_itself() {
    let Some(chrome) = running() else {
        return;
    };
    let port = chrome
        .ws
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let before = page_targets(&port);
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    let _page = browser.new_page().await.unwrap();
    assert_eq!(page_targets(&port), before + 1);
    drop(_page);
    drop(browser);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while page_targets(&port) > before && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        page_targets(&port),
        before,
        "netweir's page was left behind"
    );
}

#[tokio::test]
async fn a_refusal_from_devtools_is_shown_as_it_was_given() {
    // What Chrome answers when it's asked by a host name other than an IP
    // address or localhost, as in Docker's http://chrome:9222.
    let server = common::serve(vec![(
        "/json/version",
        common::Reply {
            status: 500,
            headers: vec![],
            body: "Host header is specified and is not an IP address or localhost.".into(),
        },
    )]);
    let err = Browser::connect(&server.url, LaunchOptions::default())
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("Host header"), "{err}");
}
