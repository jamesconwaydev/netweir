mod common;

use common::chrome;
use netweir_browser::{Browser, LaunchOptions};

#[tokio::test]
async fn closing_the_browser_ends_chrome_and_removes_its_profile() {
    let Some(executable) = chrome() else { return };
    let browser = Browser::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await
    .unwrap();
    let profile = browser.profile_dir().to_path_buf();
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
