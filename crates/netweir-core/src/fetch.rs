//! Making requests that look like the profile's browser made them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use url::Url;
use wreq::header::{HeaderName, HeaderValue, LOCATION, OrigHeaderMap};
use wreq::{Proxy, Uri, Version, redirect};

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

/// The jar, with cookies in the order the profile's browser sends them:
/// longest path first, then oldest first (Chrome, Firefox) or newest first
/// (Safari).
struct OrderedJar {
    jar: Arc<wreq::cookie::Jar>,
    newest_first: bool,
}

impl wreq::cookie::CookieStore for OrderedJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, uri: &Uri) {
        self.jar.set_cookies(cookie_headers, uri);
    }

    fn cookies(&self, uri: &Uri, version: Version) -> wreq::cookie::Cookies {
        if !self.newest_first {
            return self.jar.cookies(uri, version);
        }
        // The jar sends longest path first, then oldest first, and only it
        // knows which is older. So take its order and reverse each run of
        // equal path length. The run boundaries come from the matching
        // cookies' paths; a cookie with the same name and value under two
        // paths reads the same either way, so give each the longest unused.
        let wreq::cookie::Cookies::Uncompressed(oldest_first) =
            self.jar.cookies(uri, Version::HTTP_2)
        else {
            return wreq::cookie::Cookies::Empty;
        };
        let mut paths: HashMap<String, Vec<usize>> = HashMap::new();
        for c in self.jar.matches(uri.clone()) {
            let pair = format!("{}={}", c.name(), c.value());
            paths
                .entry(pair)
                .or_default()
                .push(c.path().unwrap_or("/").len());
        }
        let mut sent: Vec<(usize, HeaderValue)> = Vec::with_capacity(oldest_first.len());
        for value in oldest_first {
            let longest = value.to_str().ok().and_then(|pair| {
                let lengths = paths.get_mut(pair)?;
                let i = (0..lengths.len()).max_by_key(|&i| lengths[i])?;
                Some(lengths.swap_remove(i))
            });
            match longest {
                Some(length) => sent.push((length, value)),
                // The jar changed between the two reads; this one request
                // goes out in the jar's own order.
                None => return self.jar.cookies(uri, version),
            }
        }
        if sent.is_empty() {
            return wreq::cookie::Cookies::Empty;
        }
        for run in sent.chunk_by_mut(|a, b| a.0 == b.0) {
            run.reverse();
        }
        if matches!(version, Version::HTTP_2 | Version::HTTP_3) {
            wreq::cookie::Cookies::Uncompressed(sent.into_iter().map(|(_, v)| v).collect())
        } else {
            let pairs: Vec<&str> = sent.iter().filter_map(|(_, v)| v.to_str().ok()).collect();
            HeaderValue::from_str(&pairs.join("; "))
                .map(wreq::cookie::Cookies::Compressed)
                .unwrap_or(wreq::cookie::Cookies::Empty)
        }
    }
}

/// A connection pool and cookie jar that sends every request as one
/// browser. Cheap to share: clones use the same pool.
#[derive(Clone)]
pub struct Fetcher {
    client: wreq::Client,
    http2_headers: Arc<Vec<(HeaderName, HeaderValue)>>,
    /// Header order for a navigation's first request, and after a redirect.
    navigation_order: Arc<Vec<String>>,
    redirect_order: Arc<Vec<String>>,
    /// https origins (`host:port`) that answered over HTTP/1.1, so get no
    /// HTTP/2-only headers.
    http1_hosts: Arc<Mutex<HashSet<String>>>,
    jar: Arc<wreq::cookie::Jar>,
    /// Whether to offer only http/1.1 to origins known to answer with it.
    remembers_http1: bool,
}

#[derive(Debug, Clone)]
pub struct Response {
    /// The final URL, after redirects.
    pub url: String,
    pub status: u16,
    /// "HTTP/1.1", "HTTP/2" and so on, or "browser" for a page Chrome
    /// rendered.
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
    pub(crate) fn invalid(message: impl Into<String>) -> FetchError {
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

/// What one request of a navigation gave.
#[derive(Debug)]
pub enum Hop {
    Done(Response),
    /// A redirect, to this absolute URL.
    Redirect(String),
}

/// Chrome follows at most 20 redirects.
const MAX_REDIRECTS: usize = 20;

impl Fetcher {
    pub fn new(options: FetchOptions) -> Result<Fetcher, FetchError> {
        let jar = Arc::new(wreq::cookie::Jar::default());
        let profile = &options.profile;
        let emulation = profile
            .emulation()
            .map_err(|e| FetchError::invalid(e.to_string()))?;
        let mut builder = wreq::Client::builder()
            .emulation(emulation)
            .timeout(options.timeout)
            .cookie_provider(Arc::new(OrderedJar {
                jar: jar.clone(),
                newest_first: profile.cookies_newest_first,
            }))
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .deflate(true)
            // Redirects are followed in `get`, one request per hop, so each
            // hop gets its own cookies, header order and HTTP/2-only headers.
            .redirect(redirect::Policy::none())
            .tls_cert_verification(options.verify_certificates);
        if let Some(proxy) = &options.proxy {
            builder = builder
                .proxy(Proxy::all(proxy.as_str()).map_err(|e| FetchError::invalid(e.to_string()))?);
        }
        let client = builder
            .build()
            .map_err(|e| FetchError::invalid(FetchError::from(e).message))?;
        let http2_headers = profile
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
            navigation_order: Arc::new(profile.header_order(false)),
            redirect_order: Arc::new(profile.header_order(true)),
            http1_hosts: Arc::default(),
            jar,
            remembers_http1: profile.remembers_http1,
        })
    }

    /// Whether a request to `url` will go over HTTP/2: never for http://,
    /// and not for https origins that have answered over HTTP/1.1 before.
    // ponytail: the first request to an https origin without HTTP/2 still
    // carries the HTTP/2-only headers. Knowing for sure needs the ALPN result
    // before the request is written, which wreq does not expose; the fix
    // belongs in wreq (drop marked headers when the connection is HTTP/1).
    fn expects_http2(&self, url: &Uri) -> bool {
        url.scheme_str() == Some("https")
            && !https_origin(url).is_some_and(|o| {
                self.http1_hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&o)
            })
    }

    /// Remembers that an https origin answered over HTTP/1.1.
    fn note_version(&self, url: &Uri, version: Version) {
        if version < Version::HTTP_2
            && let Some(origin) = https_origin(url)
        {
            self.http1_hosts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(origin);
        }
    }

    /// GETs `url`, following redirects like a browser's navigation does.
    /// `headers` replace the profile's header of the same name in its usual
    /// position; new names go after the profile's own, keeping the caller's
    /// capitalisation on HTTP/1.1.
    /// Adds cookies a browser holds, as if the browser's responses had
    /// set them here.
    pub fn add_cookies(&self, cookies: &[netweir_browser::Cookie]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        for c in cookies {
            // A leading dot marks a cookie for the domain and its
            // subdomains; without it, the cookie is the host's alone.
            let host = c.domain.trim_start_matches('.');
            let mut line = format!("{}={}; Path={}", c.name, c.value, c.path);
            if c.domain.starts_with('.') {
                line.push_str(&format!("; Domain={host}"));
            }
            if c.expires >= 0.0 {
                line.push_str(&format!("; Max-Age={}", (c.expires - now).max(0.0) as u64));
            }
            if c.secure {
                line.push_str("; Secure");
            }
            if c.http_only {
                line.push_str("; HttpOnly");
            }
            if let Some(same_site) = &c.same_site {
                line.push_str(&format!("; SameSite={same_site}"));
            }
            let scheme = if c.secure { "https" } else { "http" };
            self.jar.add(line, format!("{scheme}://{host}{}", c.path));
        }
    }

    pub async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Response, FetchError> {
        let mut current = url.to_string();
        for hop in 0..=MAX_REDIRECTS {
            match self.hop(&current, headers, hop).await? {
                Hop::Done(response) => return Ok(response),
                Hop::Redirect(next) => current = next,
            }
        }
        unreachable!("the last hop returns a response or an error")
    }

    /// One request of a navigation: the response, or where it redirects.
    /// `hop` counts the redirects already followed, which sets the header
    /// order Chrome uses after one and, at 20, makes another an error. A
    /// crawler that follows redirects itself, so each hop goes through its
    /// own checks, calls this instead of `get`.
    pub async fn hop(
        &self,
        url: &str,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<Hop, FetchError> {
        let mut caller = Vec::with_capacity(headers.len());
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| FetchError::invalid(format!("bad header name {k:?}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|_| FetchError::invalid(format!("bad value for header {k}")))?;
            caller.push((k.as_str(), name, value));
        }
        let current = parse_url(url)?;
        let uri: Uri = current
            .as_str()
            .parse()
            .map_err(|_| FetchError::invalid(format!("bad URL {url:?}")))?;
        let response = self.send(&uri, &caller, hop > 0).await?;
        self.note_version(&uri, response.version());
        let location = response
            .status()
            .is_redirection()
            .then(|| response.headers().get(LOCATION))
            .flatten()
            .and_then(|l| l.to_str().ok());
        let Some(location) = location else {
            return Ok(Hop::Done(
                into_response(current.to_string(), response).await?,
            ));
        };
        if hop >= MAX_REDIRECTS {
            return Err(FetchError {
                kind: FetchErrorKind::TooManyRedirects,
                message: format!("more than {MAX_REDIRECTS} redirects, the last to {url}"),
            });
        }
        let next = current
            .join(location)
            .map_err(|_| FetchError::invalid(format!("redirect to a bad URL {location:?}")))?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(FetchError::invalid(format!(
                "redirect to an unsupported URL {next}"
            )));
        }
        Ok(Hop::Redirect(next.into()))
    }

    async fn send(
        &self,
        uri: &Uri,
        caller: &[(&str, HeaderName, HeaderValue)],
        redirected: bool,
    ) -> Result<wreq::Response, FetchError> {
        let base = if redirected {
            &self.redirect_order
        } else {
            &self.navigation_order
        };
        let mut order = OrigHeaderMap::new();
        for name in base.iter() {
            order.insert(name.clone());
        }
        for (raw, name, _) in caller {
            if !base.iter().any(|b| b.eq_ignore_ascii_case(name.as_str())) {
                order.insert(raw.to_string());
            }
        }
        let mut request = self.client.get(uri.clone()).orig_headers(order);
        if self.remembers_http1 && uri.scheme_str() == Some("https") && !self.expects_http2(uri) {
            // As Safari does: an origin that answered over HTTP/1.1 is only
            // offered http/1.1 from then on.
            request = request.version(Version::HTTP_11);
        }
        if self.expects_http2(uri) {
            for (name, value) in self.http2_headers.iter() {
                if !caller.iter().any(|(_, n, _)| n == name) {
                    request = request.header(name.clone(), value.clone());
                }
            }
        }
        for (_, name, value) in caller {
            request = request.header(name.clone(), value.clone());
        }
        Ok(request.send().await?)
    }
}

fn parse_url(url: &str) -> Result<Url, FetchError> {
    let parsed = Url::parse(url).map_err(|_| FetchError::invalid(format!("bad URL {url:?}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(FetchError::invalid(format!(
            "only http and https URLs can be fetched, not {url:?}"
        )));
    }
    Ok(parsed)
}

/// `host:port` of an https URL, the unit HTTP/2 support is known by.
fn https_origin(url: &Uri) -> Option<String> {
    (url.scheme_str() == Some("https")).then(|| {
        format!(
            "{}:{}",
            url.host().unwrap_or(""),
            url.port_u16().unwrap_or(443)
        )
    })
}

async fn into_response(url: String, response: wreq::Response) -> Result<Response, FetchError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use wreq::cookie::CookieStore;

    fn fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new(Profile::named("chrome").unwrap())).unwrap()
    }

    #[test]
    fn plain_http_answers_say_nothing_about_https() {
        let f = fetcher();
        let https: Uri = "https://example.com/".parse().unwrap();
        f.note_version(&"http://example.com/".parse().unwrap(), Version::HTTP_11);
        assert!(f.expects_http2(&https));
        f.note_version(&https, Version::HTTP_11);
        assert!(!f.expects_http2(&https));
        // Another port on the same host is another server.
        assert!(f.expects_http2(&"https://example.com:8443/".parse().unwrap()));
        assert!(!f.expects_http2(&"http://example.com/".parse().unwrap()));
    }

    #[test]
    fn only_http_and_https_urls_are_fetched() {
        assert!(parse_url("https://example.com/a?b").is_ok());
        for bad in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "example.com",
            "",
            "https://",
        ] {
            assert_eq!(
                parse_url(bad).unwrap_err().kind,
                FetchErrorKind::Invalid,
                "{bad:?}"
            );
        }
    }

    fn safari_cookies(jar: &OrderedJar, url: &str, version: Version) -> Vec<String> {
        match jar.cookies(&url.parse().unwrap(), version) {
            wreq::cookie::Cookies::Uncompressed(v) => {
                v.iter().map(|h| h.to_str().unwrap().to_owned()).collect()
            }
            wreq::cookie::Cookies::Compressed(h) => vec![h.to_str().unwrap().to_owned()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn safari_sends_newest_first_whatever_other_sites_set() {
        let jar = OrderedJar {
            jar: Arc::new(wreq::cookie::Jar::default()),
            newest_first: true,
        };
        let set = |url: &str, cookie: &str| {
            let value = HeaderValue::from_str(cookie).unwrap();
            jar.set_cookies(&mut std::iter::once(&value), &url.parse().unwrap());
        };
        // A host-only cookie of the same name, set earlier on another site,
        // mustn't make this site's sid look older than it is.
        set("https://a.example/", "sid=a; Path=/");
        set("https://b.example/", "t=x; Path=/");
        set("https://b.example/", "sid=b; Path=/");
        set("https://b.example/deep/", "p=1; Path=/deep");
        assert_eq!(
            safari_cookies(&jar, "https://b.example/deep/x", Version::HTTP_2),
            ["p=1", "sid=b", "t=x"]
        );
        assert_eq!(
            safari_cookies(&jar, "https://b.example/deep/x", Version::HTTP_11),
            ["p=1; sid=b; t=x"]
        );
        for version in [Version::HTTP_11, Version::HTTP_2] {
            assert!(matches!(
                jar.cookies(&"https://c.example/".parse().unwrap(), version),
                wreq::cookie::Cookies::Empty
            ));
        }
    }
}
