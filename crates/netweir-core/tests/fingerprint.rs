//! A request made with a profile must be indistinguishable, on the wire,
//! from the browser the profile was captured from.
//!
//! The captures in `profiles/` record Chrome 154 doing four top-level
//! navigations against the recording server: a page that sets a cookie, a
//! revisit that sends it, a redirect to another site, and a redirect within
//! the site. Here netweir does the same four and every request is compared.

use netweir_core::{FetchOptions, Fetcher, Profile};
use netweir_fingerprint::{Capture, Server, differences};

fn local_fetcher(profile: &str) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    Fetcher::new(options).unwrap()
}

/// The navigations Chrome made for the captures, against `port`. Each
/// redirect is one navigation but two requests, so this makes six.
fn scenario(port: u16) -> [String; 4] {
    [
        format!("https://localhost:{port}/?set=nw"),
        format!("https://localhost:{port}/"),
        format!("https://localhost:{port}/redirect?to=https://127.0.0.1:{port}/after&set=r"),
        format!("https://localhost:{port}/redirect?to=https://localhost:{port}/final&set=s"),
    ]
}

async fn run(fetcher: &Fetcher, server: &mut Server, urls: &[String]) -> Vec<Capture> {
    let mut seen = Vec::new();
    for url in urls {
        let response = fetcher.get(url, &[]).await.unwrap();
        assert_eq!(response.status, 200, "{url}");
        let hops = if url.contains("/redirect") { 2 } else { 1 };
        for _ in 0..hops {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

fn assert_same(expected: &[Capture], actual: &[Capture], what: &str) {
    assert_eq!(expected.len(), actual.len(), "{what}: request count");
    for (e, a) in expected.iter().zip(actual) {
        let diff = differences(e, a);
        assert!(
            diff.is_empty(),
            "{what}: request {} ({}) differs from Chrome 154:\n{}",
            a.request,
            e.path,
            diff.join("\n")
        );
    }
}

#[tokio::test]
async fn chrome_154_navigations_over_http2_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.capture.json"
    ))
    .unwrap();
    // Several runs, each on fresh connections, because Chrome shuffles its
    // extensions and GREASE values per connection and so must we.
    for _ in 0..3 {
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher("chrome-154-macos");
        let urls = scenario(server.port);
        let actual = run(&fetcher, &mut server, &urls).await;
        assert_same(&expected, &actual, "HTTP/2");
    }
}

#[tokio::test]
async fn chrome_154_navigations_over_http1_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.http1.capture.json"
    ))
    .unwrap();
    let mut server = Server::start_http1(0).await.unwrap();
    let port = server.port;
    let fetcher = local_fetcher("chrome-154-macos");
    // A first request can't know a host lacks HTTP/2 (see
    // Fetcher::expects_http2), so let netweir meet both hosts first.
    let warm = [
        format!("https://localhost:{port}/warm"),
        format!("https://127.0.0.1:{port}/warm"),
    ];
    run(&fetcher, &mut server, &warm).await;
    let actual = run(&fetcher, &mut server, &scenario(port)).await;
    assert_same(&expected, &actual, "HTTP/1.1");
}

#[tokio::test]
async fn extension_order_is_shuffled_per_connection_like_chrome() {
    let mut server = Server::start().await.unwrap();
    let url = format!("https://localhost:{}/", server.port);
    let mut orders = std::collections::HashSet::new();
    for _ in 0..5 {
        let fetcher = local_fetcher("chrome");
        let captures = run(&fetcher, &mut server, std::slice::from_ref(&url)).await;
        orders.insert(captures[0].client_hello.extensions.clone());
    }
    assert!(
        orders.len() > 1,
        "five connections sent the same extension order"
    );
}

#[tokio::test]
async fn caller_headers_replace_in_place_and_new_ones_go_last() {
    for http1 in [false, true] {
        let mut server = if http1 {
            Server::start_http1(0).await
        } else {
            Server::start().await
        }
        .unwrap();
        let url = format!("https://localhost:{}/", server.port);
        let fetcher = local_fetcher("chrome");
        if http1 {
            run(&fetcher, &mut server, std::slice::from_ref(&url)).await;
        }
        let headers = [
            ("accept-language".to_string(), "fr-FR,fr;q=0.9".to_string()),
            ("X-Trace".to_string(), "1".to_string()),
        ];
        fetcher.get(&url, &headers).await.unwrap();
        let seen = server.next().await.unwrap();
        let names: Vec<&str> = seen.headers.iter().map(|(k, _)| k.as_str()).collect();
        let lang = names
            .iter()
            .position(|k| k.eq_ignore_ascii_case("accept-language"))
            .unwrap();
        assert!(
            names[lang - 1].eq_ignore_ascii_case("accept-encoding"),
            "{names:?}"
        );
        assert_eq!(seen.headers[lang].1, "fr-FR,fr;q=0.9");
        assert_eq!(
            names
                .iter()
                .filter(|k| k.eq_ignore_ascii_case("accept-language"))
                .count(),
            1
        );
        if http1 {
            // The caller's capitalisation survives on HTTP/1.1.
            assert_eq!(names[lang], "Accept-Language");
            assert_eq!(names.last(), Some(&"X-Trace"), "{names:?}");
        } else {
            // HTTP/2: Chrome's HTTP/2-only header follows the profile's own,
            // and the caller's new header comes after everything.
            assert_eq!(&names[names.len() - 2..], ["priority", "x-trace"]);
        }
    }
}

#[tokio::test]
async fn too_many_redirects_is_an_error_not_a_loop() {
    let server = Server::start().await.unwrap();
    let fetcher = local_fetcher("chrome");
    // /loop redirects to itself, with a relative Location.
    let url = format!("https://localhost:{}/loop", server.port);
    let err = fetcher.get(&url, &[]).await.unwrap_err();
    assert_eq!(
        err.kind,
        netweir_core::FetchErrorKind::TooManyRedirects,
        "{err}"
    );
}
