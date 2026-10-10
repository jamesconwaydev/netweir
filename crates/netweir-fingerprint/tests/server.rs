//! The capture server against a plain rustls client, whose ClientHello we
//! know enough about to check the parser.

use std::sync::Arc;

use netweir_fingerprint::{Server, is_grease};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

#[derive(Debug)]
struct AcceptAnything;

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn connect(port: u16) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnything))
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap()
}

/// Sends one HTTP/1.1 request and reads exactly one response (by
/// Content-Length), leaving the connection open.
async fn exchange(tls: &mut tokio_rustls::client::TlsStream<TcpStream>, path: &str) -> String {
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nX-Probe: 1\r\nAccept: */*\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; length];
    tls.read_exact(&mut body).await.unwrap();
    head + &String::from_utf8(body).unwrap()
}

async fn request(port: u16) -> String {
    exchange(&mut connect(port).await, "/").await
}

#[tokio::test]
async fn records_the_client_hello_and_echoes_it() {
    let mut server = Server::start().await.unwrap();
    let response = request(server.port).await;
    let capture = server.next().await.unwrap();

    let hello = &capture.client_hello;
    assert_eq!(hello.server_name.as_deref(), Some("localhost"));
    assert_eq!(hello.alpn, ["http/1.1"]);
    assert!(hello.supported_versions.contains(&0x0304));
    assert!(
        hello.ciphers.contains(&0x1301),
        "TLS_AES_128_GCM_SHA256 missing"
    );
    assert!(
        !hello.ciphers.iter().any(|&c| is_grease(c)),
        "rustls does not send GREASE"
    );
    assert!(capture.ja4.starts_with("t13d"), "{}", capture.ja4);
    assert!(
        capture.ja4.contains("h1_"),
        "ALPN http/1.1 gives h1: {}",
        capture.ja4
    );

    assert_eq!(capture.protocol, "http/1.1");
    let names: Vec<&str> = capture.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["Host", "X-Probe", "Accept"]);
    assert!(capture.http2.is_none());

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(
        response.contains(&capture.ja4),
        "the body echoes the capture"
    );
    assert_eq!(
        (capture.connection, capture.request, capture.path.as_str()),
        (0, 0, "/")
    );
}

#[tokio::test]
async fn records_every_request_on_a_kept_alive_connection() {
    let mut server = Server::start_http1(0).await.unwrap();
    let mut tls = connect(server.port).await;
    exchange(&mut tls, "/one").await;
    exchange(&mut tls, "/two").await;
    let (a, b) = (server.next().await.unwrap(), server.next().await.unwrap());
    assert_eq!((a.connection, a.request, a.path.as_str()), (0, 0, "/one"));
    assert_eq!((b.connection, b.request, b.path.as_str()), (0, 1, "/two"));
    assert_eq!(a.ja4, b.ja4);
}

#[tokio::test]
async fn sets_cookies_and_redirects_on_request() {
    let server = Server::start().await.unwrap();
    let mut tls = connect(server.port).await;
    let set = exchange(&mut tls, "/page?set=session").await;
    assert!(set.contains("set-cookie: session=1; Path=/\r\n"), "{set}");
    let moved = exchange(&mut tls, "/redirect?to=https://127.0.0.1:9/next&set=hop").await;
    assert!(moved.starts_with("HTTP/1.1 302 Found\r\n"), "{moved}");
    assert!(
        moved.contains("location: https://127.0.0.1:9/next\r\n"),
        "{moved}"
    );
    assert!(moved.contains("set-cookie: hop=1; Path=/\r\n"), "{moved}");
}

#[tokio::test]
async fn hangs_up_on_anything_that_is_not_tls() {
    let server = Server::start().await.unwrap();
    let mut tcp = TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    tcp.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), tcp.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "server kept a non-TLS connection open: {read:?}"
    );
}

#[tokio::test]
async fn records_the_method_and_body_of_a_post() {
    let mut server = Server::start_http1(0).await.unwrap();
    let mut tls = connect(server.port).await;
    let body = "q=rust&page=2";
    let request = format!(
        "POST /submitted HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    // The answer comes once the body has arrived, and the connection
    // carries on.
    let mut answer = [0u8; 15];
    tls.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"HTTP/1.1 200 OK");
    let capture = server.next().await.unwrap();
    assert_eq!(capture.method, "POST");
    assert_eq!(capture.body, body);
    assert_eq!(capture.path, "/submitted");
}

#[tokio::test]
async fn serves_pages_whose_scripts_post() {
    let server = Server::start().await.unwrap();
    let mut tls = connect(server.port).await;
    let form = exchange(&mut tls, "/form").await;
    assert!(form.contains("content-type: text/html"), "{form}");
    assert!(
        form.contains(r#"<form method="post" action="/submitted""#),
        "{form}"
    );
    let fetch = exchange(&mut tls, "/fetch").await;
    assert!(fetch.contains("fetch(\"/api\""), "{fetch}");
}

#[test]
fn grease_values() {
    for v in [0x0a0a, 0x1a1a, 0xfafa] {
        assert!(is_grease(v));
    }
    for v in [0x0a1a, 0x1301, 0x0000, 0xffff] {
        assert!(!is_grease(v));
    }
}
