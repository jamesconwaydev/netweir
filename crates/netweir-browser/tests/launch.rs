mod common;

use common::chrome;
use netweir_browser::{Browser, LaunchOptions};

#[tokio::test]
async fn closing_the_browser_ends_chrome_and_removes_its_profile() {
    let Some(executable) = chrome() else { return };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    let profile = browser.profile_dir().unwrap().to_path_buf();
    assert!(profile.exists());
    assert!(
        browser.version().starts_with("Chrome/"),
        "{}",
        browser.version()
    );
    browser.close().await.unwrap();
    assert!(!profile.exists(), "{} left behind", profile.display());
    assert!(browser.is_closed());
}

#[tokio::test]
async fn a_missing_executable_says_where_it_looked() {
    let err = Browser::launch(LaunchOptions {
        executable: Some("/nonexistent/chrome".into()),
        ..LaunchOptions::default()
    })
    .await
    .err()
    .unwrap();
    assert!(err.to_string().contains("/nonexistent/chrome"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_browser_that_dies_reads_as_closed() {
    let Some(executable) = chrome() else { return };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    let pid = browser.pid().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !browser.is_closed() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(browser.is_closed());
    assert!(browser.new_page().await.is_err());
    browser.close().await.unwrap();
    assert!(!browser.profile_dir().unwrap().exists());
}
