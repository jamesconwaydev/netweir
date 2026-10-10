//! A request made with a profile must be indistinguishable, on the wire,
//! from the browser the profile was captured from.
//!
//! The captures in `profiles/` record Chrome 154 doing four top-level
//! navigations against the recording server: a page that sets a cookie, a
//! revisit that sends it, a redirect to another site, and a redirect within
//! the site. Here netweir does the same four and every request is compared.

use netweir_core::{FetchOptions, Fetcher, Outgoing, Profile};
use netweir_fingerprint::{Capture, Server, differences, extension_order_differs};

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
            "{what}: request {} ({}) differs from the browser:\n{}",
            a.request,
            e.path,
            diff.join("\n")
        );
    }
}

/// `assert_same`, plus the extension order, for a browser that doesn't
/// shuffle it.
fn assert_same_in_order(expected: &[Capture], actual: &[Capture], what: &str) {
    assert_same(expected, actual, what);
    for (e, a) in expected.iter().zip(actual) {
        if let Some(diff) = extension_order_differs(e, a) {
            panic!("{what}: request {} ({}): {diff}", a.request, e.path);
        }
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

#[tokio::test]
async fn firefox_156_navigations_over_http2_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/firefox-156-macos.capture.json"
    ))
    .unwrap();
    for _ in 0..2 {
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher("firefox-156-macos");
        let urls = scenario(server.port);
        let actual = run(&fetcher, &mut server, &urls).await;
        assert_same_in_order(&expected, &actual, "Firefox over HTTP/2");
    }
}

#[tokio::test]
async fn firefox_156_navigations_over_http1_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/firefox-156-macos.http1.capture.json"
    ))
    .unwrap();
    let mut server = Server::start_http1(0).await.unwrap();
    let port = server.port;
    let fetcher = local_fetcher("firefox");
    let warm = [
        format!("https://localhost:{port}/warm"),
        format!("https://127.0.0.1:{port}/warm"),
    ];
    run(&fetcher, &mut server, &warm).await;
    let actual = run(&fetcher, &mut server, &scenario(port)).await;
    assert_same_in_order(&expected, &actual, "Firefox over HTTP/1.1");
}

#[tokio::test]
async fn safari_27_navigations_over_http2_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/safari-27-macos.capture.json"
    ))
    .unwrap();
    for _ in 0..2 {
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher("safari-27-macos");
        let urls = scenario(server.port);
        let actual = run(&fetcher, &mut server, &urls).await;
        assert_same_in_order(&expected, &actual, "Safari over HTTP/2");
    }
}

#[tokio::test]
async fn safari_27_navigations_over_http1_are_indistinguishable() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/safari-27-macos.http1.capture.json"
    ))
    .unwrap();
    let mut server = Server::start_http1(0).await.unwrap();
    let port = server.port;
    let fetcher = local_fetcher("safari");
    // Safari had met localhost over HTTP/1.1 before this navigation (and so
    // offered it only http/1.1), but not 127.0.0.1.
    let warm = [format!("https://localhost:{port}/warm")];
    run(&fetcher, &mut server, &warm).await;
    let actual = run(&fetcher, &mut server, &scenario(port)).await;
    let expected = either_pooled_connection(expected, &actual);
    assert_same_in_order(&expected, &actual, "Safari over HTTP/1.1");
}

/// Safari kept two connections to localhost: one opened before it learned
/// the origin was HTTP/1.1-only, which still carries the h2 offer, and one
/// opened after. Which of the two a request rides is Safari's pooling, not
/// something a site can tell apart, so a request Safari sent on a
/// connection opened before the scenario may carry either connection's
/// ALPN offer, as long as Safari made that offer to the same host and
/// netweir sent it on a connection it had already used. A new connection
/// is held to Safari's offer exactly.
fn either_pooled_connection(mut expected: Vec<Capture>, actual: &[Capture]) -> Vec<Capture> {
    let opened_here = |c: &Capture| {
        expected
            .iter()
            .any(|o| o.connection == c.connection && o.request == 0)
    };
    let pooled: Vec<bool> = expected.iter().map(|e| !opened_here(e)).collect();
    let offers: Vec<(Option<String>, Vec<String>, String)> = expected
        .iter()
        .map(|e| {
            let hello = &e.client_hello;
            (hello.server_name.clone(), hello.alpn.clone(), e.ja4.clone())
        })
        .collect();
    for ((e, a), pooled) in expected.iter_mut().zip(actual).zip(pooled) {
        if !pooled || a.request == 0 {
            continue;
        }
        let host = &e.client_hello.server_name;
        if let Some((_, alpn, ja4)) = offers
            .iter()
            .find(|(h, alpn, _)| h == host && *alpn == a.client_hello.alpn)
        {
            e.client_hello.alpn = alpn.clone();
            e.ja4 = ja4.clone();
        }
    }
    expected
}

/// What Chrome did after the navigations, in the post captures: submitted
/// a form, posted JSON from a script, signed in through a form answered
/// with a redirect, and followed a link. Each on the page that started it.
async fn post_scenario(fetcher: &Fetcher, server: &mut Server) -> Vec<Capture> {
    let base = format!("https://localhost:{}", server.port);
    let page = |path: &str| format!("{base}{path}");
    // The cookie the captured requests carry.
    fetcher.get(&page("/?set=nw"), &[]).await.unwrap();
    server.next().await.unwrap();
    let steps = [
        (
            "/submitted",
            Outgoing::form(b"q=rust&page=2".to_vec(), None, Some(page("/form"))),
            1,
        ),
        (
            "/api",
            Outgoing::fetch(
                "POST",
                Some(br#"{"q":"rust","page":2}"#.to_vec()),
                Some("application/json".into()),
                Some(page("/fetch")),
            ),
            1,
        ),
        (
            "/redirect?to=/after",
            Outgoing::form(b"user=ada".to_vec(), None, Some(page("/form-redirect"))),
            2,
        ),
        ("/linked", Outgoing::link(page("/link")), 1),
    ];
    let mut seen = Vec::new();
    for (path, outgoing, requests) in steps {
        let response = fetcher.send(&page(path), &outgoing, &[]).await.unwrap();
        assert_eq!(response.status, 200, "{path}");
        for _ in 0..requests {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

#[tokio::test]
async fn chrome_154_forms_and_scripts_post_like_chrome_over_http2() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.post.capture.json"
    ))
    .unwrap();
    let mut server = Server::start().await.unwrap();
    let fetcher = local_fetcher("chrome-154-macos");
    let actual = post_scenario(&fetcher, &mut server).await;
    assert_same(&expected, &actual, "Chrome posting over HTTP/2");
}

#[tokio::test]
async fn chrome_154_forms_and_scripts_post_like_chrome_over_http1() {
    let expected: Vec<Capture> = serde_json::from_str(include_str!(
        "../../../profiles/chrome-154-macos.post.http1.capture.json"
    ))
    .unwrap();
    let mut server = Server::start_http1(0).await.unwrap();
    let fetcher = local_fetcher("chrome-154-macos");
    // As for the navigations: let netweir meet the host first, so it knows
    // there's no HTTP/2 to offer.
    let warm = [format!("https://localhost:{}/warm", server.port)];
    run(&fetcher, &mut server, &warm).await;
    let actual = post_scenario(&fetcher, &mut server).await;
    assert_same(&expected, &actual, "Chrome posting over HTTP/1.1");
}

#[tokio::test]
async fn a_form_from_another_site_says_so_and_gives_only_its_origin() {
    let mut server = Server::start().await.unwrap();
    let fetcher = local_fetcher("chrome-154-macos");
    let page = "https://shop.example/basket?id=7#items";
    let outgoing = Outgoing::form(b"a=1".to_vec(), None, Some(page.into()));
    let url = format!("https://localhost:{}/submitted", server.port);
    fetcher.send(&url, &outgoing, &[]).await.unwrap();
    let capture = server.next().await.unwrap();
    let header = |name: &str| {
        capture
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("sec-fetch-site"), Some("cross-site"));
    assert_eq!(header("origin"), Some("https://shop.example"));
    assert_eq!(header("referer"), Some("https://shop.example/"));
    assert_eq!(
        (capture.method.as_str(), capture.body.as_str()),
        ("POST", "a=1")
    );
}
