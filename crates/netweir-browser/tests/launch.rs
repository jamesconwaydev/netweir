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

#[cfg(unix)]
#[tokio::test]
async fn a_chrome_that_wont_start_says_why() {
    use std::os::unix::fs::PermissionsExt;
    // Stands in for a Chrome that can't start, and says so on stderr, as a
    // Chrome without a usable sandbox does.
    let dir = std::env::temp_dir().join(format!("netweir-nochrome-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("chrome");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'starting up' >&2\necho 'FATAL: No usable sandbox!' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = Browser::launch(LaunchOptions {
        executable: Some(fake),
        ..LaunchOptions::default()
    })
    .await
    .err()
    .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(err.to_string().contains("No usable sandbox"), "{err}");
}
