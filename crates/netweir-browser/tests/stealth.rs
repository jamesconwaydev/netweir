//! A page that looks for automation the way bot detection does, from page
//! script, and a check that netweir never sent what it promises not to.

mod common;

use common::{browser, html, serve};
use netweir_browser::WaitUntil;
use serde_json::json;

const DETECT: &str = r#"<!DOCTYPE html><script>
window.findings = (async () => {
  const workerAgent = await new Promise((resolve) => {
    const w = new Worker(URL.createObjectURL(new Blob(['postMessage(navigator.userAgent)'])));
    w.onmessage = (m) => resolve(m.data);
  });
  const hints = await navigator.userAgentData.getHighEntropyValues(['architecture', 'fullVersionList', 'platformVersion']);
  return {
    webdriver: navigator.webdriver,
    agent: navigator.userAgent,
    workerAgent,
    brands: navigator.userAgentData.brands.map((b) => b.brand),
    architecture: hints.architecture,
    fullVersions: hints.fullVersionList.length,
  };
})();
</script>"#;

#[tokio::test]
async fn a_page_finds_no_sign_of_automation() {
    let Some(browser) = browser().await else {
        return;
    };
    let mut detect = html(DETECT);
    // Asks for the high-entropy hints on the next request.
    detect
        .headers
        .push(("Accept-CH", "Sec-CH-UA-Full-Version-List".into()));
    let server = serve(vec![("/", detect), ("/again", html("<p>again</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let found = page.evaluate("findings").await.unwrap();

    assert_eq!(found["webdriver"], json!(false), "{found}");
    for agent in [&found["agent"], &found["workerAgent"]] {
        let agent = agent.as_str().unwrap();
        assert!(
            agent.contains("Chrome/") && !agent.contains("Headless"),
            "{agent}"
        );
    }
    assert!(!found["brands"].to_string().contains("Headless"), "{found}");
    assert_ne!(
        found["architecture"],
        json!(""),
        "high-entropy hints are empty: {found}"
    );
    assert_ne!(found["fullVersions"], json!(0), "{found}");

    page.goto(&format!("{}/again", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let headers = server.headers_for("/again");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no {name} in {headers:?}"))
    };
    assert!(!header("user-agent").contains("Headless"));
    assert!(!header("sec-ch-ua").contains("Headless"));
    assert!(header("sec-ch-ua-full-version-list").contains("Chrome"));

    // Runtime.enable used to show itself to page script through console
    // previews that read an error's stack; Chrome 154 no longer does, so
    // the only check left is that it's never sent.
    page.click("p", None).await.unwrap();
    page.fill("p", "", Some(std::time::Duration::from_millis(100)))
        .await
        .unwrap_err();
    page.screenshot(false).await.unwrap();
    let sent = browser.methods_sent();
    for forbidden in ["Runtime.enable", "Console.enable", "Log.enable"] {
        assert!(
            !sent.iter().any(|m| m == forbidden),
            "{forbidden} was sent: {sent:?}"
        );
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn pages_present_the_brands_netweir_is_told_to() {
    let Some(executable) = common::chrome() else {
        return;
    };
    // Chrome for Testing calls itself Chromium alone; netweir's profiles
    // know what Google Chrome of the same version says.
    let brands: netweir_browser::Brands = std::sync::Arc::new(|major: &str| {
        Some(format!(
            "\"Chromium\";v=\"{major}\", \"Brand X\";v=\"{major}\", \"Not A(Brand\";v=\"99\""
        ))
    });
    let browser = netweir_browser::Browser::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        brands: Some(brands),
        ..Default::default()
    })
    .await
    .unwrap();
    let major = browser
        .version()
        .trim_start_matches("Chrome/")
        .split('.')
        .next()
        .unwrap()
        .to_string();
    let mut page_html = html("<p>x</p>");
    page_html
        .headers
        .push(("Accept-CH", "Sec-CH-UA-Full-Version-List".into()));
    let server = serve(vec![("/", page_html), ("/again", html("<p>again</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let seen = page
        .evaluate("navigator.userAgentData.getHighEntropyValues(['fullVersionList']).then(h => [navigator.userAgentData.brands.map(b => b.brand + '/' + b.version), h.fullVersionList.map(b => b.brand + '/' + b.version)])")
        .await
        .unwrap();
    let full = browser.version().trim_start_matches("Chrome/").to_string();
    assert_eq!(
        seen,
        json!([
            [
                "Chromium/".to_string() + &major,
                "Brand X/".to_string() + &major,
                "Not A(Brand/99".to_string()
            ],
            [
                "Chromium/".to_string() + &full,
                "Brand X/".to_string() + &full,
                "Not A(Brand/99.0.0.0".to_string()
            ],
        ])
    );
    page.goto(&format!("{}/again", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let sent = server.headers_for("/again");
    let ua = sent
        .iter()
        .find(|(k, _)| k == "sec-ch-ua")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(ua.contains("\"Brand X\""), "{ua}");
    browser.close().await.unwrap();
}
