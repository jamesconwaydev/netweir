//! A request made with a profile must be indistinguishable, on the wire,
//! from the browser the profile was captured from.

use netweir_core::{FetchOptions, Fetcher, Profile};
use netweir_fingerprint::{Capture, Server, differences};

fn local_fetcher(profile: &str) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    Fetcher::new(options).unwrap()
}

async fn fetch_capture(
    fetcher: &Fetcher,
    server: &mut Server,
    headers: &[(String, String)],
) -> Capture {
    let url = format!("https://localhost:{}/", server.port);
    let response = fetcher.get(&url, headers).await.unwrap();
    assert_eq!(response.status, 200);
    let seen = server.next().await.unwrap();
    let echoed: Capture = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(echoed.ja4, seen.ja4);
    seen
}

#[tokio::test]
async fn chrome_154_is_indistinguishable_from_chrome_154() {
    let expected: Capture = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.capture.json"
    ))
    .unwrap();
    let mut server = Server::start().await.unwrap();
    // Several connections, because Chrome shuffles its extensions and
    // GREASE values per connection and so must we.
    for _ in 0..3 {
        let fetcher = local_fetcher("chrome-154-macos");
        let actual = fetch_capture(&fetcher, &mut server, &[]).await;
        let diff = differences(&expected, &actual);
        assert!(
            diff.is_empty(),
            "differs from Chrome 154:\n{}",
            diff.join("\n")
        );
    }
}

#[tokio::test]
async fn chrome_154_over_http1_is_indistinguishable_too() {
    let expected: Capture = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.http1.capture.json"
    ))
    .unwrap();
    let mut server = Server::start_http1(0).await.unwrap();
    let fetcher = local_fetcher("chrome-154-macos");
    // The first request can't know the server lacks HTTP/2 (see
    // Fetcher::expects_http2); from the second on it must match exactly.
    let _ = fetch_capture(&fetcher, &mut server, &[]).await;
    let actual = fetch_capture(&fetcher, &mut server, &[]).await;
    let diff = differences(&expected, &actual);
    assert!(
        diff.is_empty(),
        "differs from Chrome 154 over HTTP/1.1:\n{}",
        diff.join("\n")
    );
}

#[tokio::test]
async fn extension_order_is_shuffled_per_connection_like_chrome() {
    let mut server = Server::start().await.unwrap();
    let mut orders = std::collections::HashSet::new();
    for _ in 0..5 {
        let fetcher = local_fetcher("chrome");
        orders.insert(
            fetch_capture(&fetcher, &mut server, &[])
                .await
                .client_hello
                .extensions,
        );
    }
    assert!(
        orders.len() > 1,
        "five connections sent the same extension order"
    );
}

#[tokio::test]
async fn caller_headers_replace_in_place_and_new_ones_go_last() {
    let mut server = Server::start().await.unwrap();
    let fetcher = local_fetcher("chrome");
    let headers = [
        ("accept-language".to_string(), "fr-FR,fr;q=0.9".to_string()),
        ("x-trace".to_string(), "1".to_string()),
    ];
    let seen = fetch_capture(&fetcher, &mut server, &headers).await;
    let names: Vec<&str> = seen.headers.iter().map(|(k, _)| k.as_str()).collect();
    let lang = names.iter().position(|&k| k == "accept-language").unwrap();
    assert_eq!(names[lang - 1], "accept-encoding", "{names:?}");
    assert_eq!(seen.headers[lang].1, "fr-FR,fr;q=0.9");
    // Chrome's HTTP/2-only header still follows the profile's own, and the
    // caller's new header comes after everything.
    assert_eq!(&names[names.len() - 2..], ["priority", "x-trace"]);
    assert_eq!(names.iter().filter(|&&k| k == "accept-language").count(), 1);
}
