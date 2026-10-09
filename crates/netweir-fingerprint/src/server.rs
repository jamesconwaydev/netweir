//! A local HTTPS server that records how each client connects: its
//! ClientHello, its first HTTP/2 frames or HTTP/1.1 request head. Every
//! response body is the recorded fingerprint as JSON.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::client_hello::{ClientHello, bytes_needed};
use crate::h2::{self, Http2};
use crate::ja4::ja4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    pub ja4: String,
    pub client_hello: ClientHello,
    /// "h2" or "http/1.1".
    pub protocol: String,
    pub http2: Option<Http2>,
    /// Request headers in the order and case they were sent. For HTTP/2
    /// this includes the pseudo-headers.
    pub headers: Vec<(String, String)>,
}

pub struct Server {
    pub port: u16,
    captures: mpsc::UnboundedReceiver<Capture>,
}

impl Server {
    /// Listens on 127.0.0.1 on a free port, with a self-signed certificate
    /// for `localhost`.
    pub async fn start() -> io::Result<Server> {
        Server::start_on(0).await
    }

    pub async fn start_on(port: u16) -> io::Result<Server> {
        Server::bind(port, &[b"h2", b"http/1.1"]).await
    }

    /// Offers only HTTP/1.1, to capture how a browser sends it.
    pub async fn start_http1(port: u16) -> io::Result<Server> {
        Server::bind(port, &[b"http/1.1"]).await
    }

    async fn bind(port: u16, alpn: &[&[u8]]) -> io::Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let acceptor = TlsAcceptor::from(Arc::new(tls_config(alpn)));
        let (tx, captures) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(capture) = handle(stream, acceptor).await {
                        let _ = tx.send(capture);
                    }
                });
            }
        });
        Ok(Server { port, captures })
    }

    /// The next connection's fingerprint.
    pub async fn next(&mut self) -> Option<Capture> {
        self.captures.recv().await
    }
}

fn tls_config(alpn: &[&[u8]]) -> rustls::ServerConfig {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("self-signed cert");
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .expect("certificate");
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

async fn handle(mut stream: TcpStream, acceptor: TlsAcceptor) -> io::Result<Capture> {
    let mut hello_bytes = Vec::with_capacity(2048);
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        hello_bytes.extend_from_slice(&buf[..n]);
        if bytes_needed(&hello_bytes).is_some_and(|need| hello_bytes.len() >= need) {
            break;
        }
        if hello_bytes.len() > 64 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    let client_hello = ClientHello::parse(&hello_bytes)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let ja4 = ja4(&client_hello);

    let mut tls = acceptor
        .accept(Replay {
            prefix: hello_bytes,
            pos: 0,
            inner: stream,
        })
        .await?;
    let protocol = match tls.get_ref().1.alpn_protocol() {
        Some(b"h2") => "h2",
        _ => "http/1.1",
    };
    let mut capture = Capture {
        ja4,
        client_hello,
        protocol: protocol.to_string(),
        http2: None,
        headers: Vec::new(),
    };
    if protocol == "h2" {
        let (http2, headers) = h2::read_request(&mut tls).await?;
        capture.http2 = Some(http2);
        capture.headers = headers;
        let body = serde_json::to_vec(&capture).expect("capture serialises");
        h2::respond(&mut tls, &body).await?;
    } else {
        capture.headers = read_http1_head(&mut tls).await?;
        let body = serde_json::to_vec(&capture).expect("capture serialises");
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(head.as_bytes()).await?;
        tls.write_all(&body).await?;
        tls.shutdown().await?;
    }
    Ok(capture)
}

async fn read_http1_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> io::Result<Vec<(String, String)>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 || head.len() > 64 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    Ok(text
        .split("\r\n")
        .skip(1) // request line
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect())
}

/// Serves `prefix` before reading from `inner`, so the TLS handshake sees
/// the ClientHello bytes we already consumed.
struct Replay<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
