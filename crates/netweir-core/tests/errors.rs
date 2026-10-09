use std::time::Duration;

use netweir_core::{FetchErrorKind, FetchOptions, Fetcher, Profile};

fn fetcher() -> Fetcher {
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_secs(5);
    Fetcher::new(options).unwrap()
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_error() {
    // Bind and drop to find a port nothing is listening on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = fetcher()
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Connect, "{err}");
}

#[tokio::test]
async fn a_self_signed_certificate_is_a_tls_error() {
    let server = netweir_fingerprint::Server::start().await.unwrap();
    let err = fetcher()
        .get(&format!("https://localhost:{}/", server.port), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Tls, "{err}");
}

#[tokio::test]
async fn a_silent_server_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _held = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_millis(300);
    let err = Fetcher::new(options)
        .unwrap()
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Timeout, "{err}");
}

#[tokio::test]
async fn bad_input_is_invalid() {
    let f = fetcher();
    for url in ["not a url", "ftp://example.com/", ""] {
        let err = f.get(url, &[]).await.unwrap_err();
        assert_eq!(err.kind, FetchErrorKind::Invalid, "{url:?}: {err}");
    }
    let bad_header = [("bad header".to_string(), "x".to_string())];
    let err = f
        .get("https://example.com/", &bad_header)
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Invalid);

    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.proxy = Some("not a proxy".into());
    assert_eq!(
        Fetcher::new(options).err().unwrap().kind,
        FetchErrorKind::Invalid
    );
}
