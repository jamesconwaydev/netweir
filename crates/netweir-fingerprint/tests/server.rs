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

async fn request(port: u16) -> String {
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
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Probe: 1\r\nAccept: */*\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    tls.read_to_string(&mut response).await.unwrap();
    response
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
