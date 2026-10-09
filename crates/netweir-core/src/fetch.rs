//! Making requests that look like the profile's browser made them.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use wreq::header::{HeaderName, HeaderValue};
use wreq::{Proxy, Version, redirect};

use crate::decode::decode;
use crate::profile::Profile;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub profile: Profile,
    /// `http://`, `https://` or `socks5://` URL of a proxy for every request.
    pub proxy: Option<String>,
    /// Whole-request limit, connect to last body byte.
    pub timeout: Duration,
    /// Only for talking to local test servers with self-signed certificates.
    pub verify_certificates: bool,
}

impl FetchOptions {
    pub fn new(profile: Profile) -> FetchOptions {
        FetchOptions {
            profile,
            proxy: None,
            timeout: Duration::from_secs(30),
            verify_certificates: true,
        }
    }
}

/// A connection pool and cookie jar that sends every request as one
/// browser. Cheap to share: clones use the same pool.
#[derive(Clone)]
pub struct Fetcher {
    client: wreq::Client,
    http2_headers: Arc<Vec<(HeaderName, HeaderValue)>>,
    /// Hosts that answered over HTTP/1.1, so get no HTTP/2-only headers.
    http1_hosts: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Clone)]
pub struct Response {
    /// The final URL, after redirects.
    pub url: String,
    pub status: u16,
    /// "HTTP/1.1", "HTTP/2" and so on.
    pub version: &'static str,
    /// In the order received. Names are lowercase.
    pub headers: Vec<(String, String)>,
    /// Decompressed, not decoded.
    pub body: Bytes,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as text, in the encoding a browser would pick.
    pub fn text(&self) -> String {
        decode(&self.body, self.header("content-type"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorKind {
    /// The options were wrong: a bad URL, proxy, header or profile.
    Invalid,
    Timeout,
    /// DNS, TCP, or the proxy refusing the connection.
    Connect,
    Tls,
    TooManyRedirects,
    /// The connection broke while reading the response.
    Body,
    Other,
}

#[derive(Debug, Clone)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub message: String,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FetchError {}

impl FetchError {
    fn invalid(message: impl Into<String>) -> FetchError {
        FetchError {
            kind: FetchErrorKind::Invalid,
            message: message.into(),
        }
    }
}

impl From<wreq::Error> for FetchError {
    fn from(e: wreq::Error) -> FetchError {
        let kind = if e.is_timeout() {
            FetchErrorKind::Timeout
        } else if e.is_tls() || caused_by_tls(&e) {
            FetchErrorKind::Tls
        } else if e.is_connect() || e.is_proxy_connect() {
            FetchErrorKind::Connect
        } else if e.is_redirect() {
            FetchErrorKind::TooManyRedirects
        } else if e.is_body() || e.is_decode() || e.is_connection_reset() {
            FetchErrorKind::Body
        } else if e.is_builder() {
            FetchErrorKind::Invalid
        } else {
            FetchErrorKind::Other
        };
        // wreq's Display stops at the outermost error; the cause is what
        // tells a user why ("connection refused", "certificate expired").
        let mut message = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            message.push_str(": ");
            message.push_str(&s.to_string());
            source = s.source();
        }
        FetchError { kind, message }
    }
}

/// wreq reports a failed handshake (a bad certificate, a protocol
/// mismatch) as a connect error; the BoringSSL error is further down the
/// chain.
fn caused_by_tls(e: &wreq::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if s.is::<btls::ssl::Error>() || s.is::<btls::error::ErrorStack>() {
            return true;
        }
        source = s.source();
    }
    false
}

impl Fetcher {
    pub fn new(options: FetchOptions) -> Result<Fetcher, FetchError> {
        let emulation = options
            .profile
            .emulation()
            .map_err(|e| FetchError::invalid(e.to_string()))?;
        let mut builder = wreq::Client::builder()
            .emulation(emulation)
            .timeout(options.timeout)
            .cookie_store(true)
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .deflate(true)
            // Chrome follows up to 20 redirects.
            .redirect(redirect::Policy::limited(20))
            .tls_cert_verification(options.verify_certificates);
        if let Some(proxy) = &options.proxy {
            builder = builder
                .proxy(Proxy::all(proxy.as_str()).map_err(|e| FetchError::invalid(e.to_string()))?);
        }
        let client = builder
            .build()
            .map_err(|e| FetchError::invalid(FetchError::from(e).message))?;
        let http2_headers = options
            .profile
            .http2_headers
            .iter()
            .map(|(k, v)| {
                let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes());
                let value = HeaderValue::from_str(v);
                name.ok()
                    .zip(value.ok())
                    .ok_or_else(|| FetchError::invalid(format!("bad profile header {k}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Fetcher {
            client,
            http2_headers: Arc::new(http2_headers),
            http1_hosts: Arc::default(),
        })
    }

    /// Whether a request to `url` will go over HTTP/2: never for http://,
    /// and not for hosts that have answered over HTTP/1.1 before.
    // ponytail: the first request to an https host without HTTP/2 still
    // carries the HTTP/2-only headers. Knowing for sure needs the ALPN result
    // before the request is written, which wreq does not expose.
    fn expects_http2(&self, url: &wreq::Uri) -> bool {
        url.scheme_str() == Some("https")
            && !url.host().is_some_and(|h| {
                self.http1_hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(h)
            })
    }

    /// GETs `url`. `headers` replace the profile's header of the same name
    /// in its usual position, and new names go after the profile's own.
    pub async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Response, FetchError> {
        let uri: wreq::Uri = url
            .parse()
            .map_err(|_| FetchError::invalid(format!("bad URL {url:?}")))?;
        let mut request = self.client.get(uri.clone());
        let mut extra = Vec::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| FetchError::invalid(format!("bad header name {k:?}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|_| FetchError::invalid(format!("bad value for header {k}")))?;
            extra.push((name, value));
        }
        if self.expects_http2(&uri) {
            for (name, value) in self.http2_headers.iter() {
                if !extra.iter().any(|(n, _)| n == name) {
                    request = request.header(name.clone(), value.clone());
                }
            }
        }
        for (name, value) in extra {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        if response.version() < Version::HTTP_2
            && let Some(host) = response.uri().host().or(uri.host())
        {
            self.http1_hosts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(host.to_string());
        }
        let url = response.uri().to_string();
        let status = response.status().as_u16();
        let version = match response.version() {
            Version::HTTP_09 => "HTTP/0.9",
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2",
            Version::HTTP_3 => "HTTP/3",
            _ => "unknown",
        };
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().await?;
        Ok(Response {
            url,
            status,
            version,
            headers,
            body,
        })
    }
}
