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
    let profile = std::env::temp_dir().join(format!("netweir-running-{}", std::process::id()));
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
