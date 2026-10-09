//! Canonical URLs and request fingerprints, so a crawl fetches each page
//! once however its links are written.

use sha2::{Digest, Sha256};
use url::Url;

/// Query parameters that only track where a click came from. Two URLs
/// differing only in these are the same page.
const TRACKING: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "twclid",
    "ttclid",
    "li_fat_id",
];

fn is_tracking(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_") || TRACKING.contains(&key.as_str())
}

/// The form of `url` used to tell pages apart: scheme and host lowercased,
/// default port and fragment dropped, tracking parameters removed and the
/// rest of the query sorted. `None` for anything but an http(s) URL.
///
/// The query is read and rewritten as form data, so `?x` and `?x=` are one
/// page, and so are `a+b` and `a%20b`. Paths and hosts are compared as the
/// URL parser normalises them; `/%7Euser` and `/~user`, or `e.com.` and
/// `e.com`, stay different, as servers can treat them differently.
pub fn canonical(url: &str) -> Option<String> {
    let mut u = Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    u.set_fragment(None);
    let mut pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !is_tracking(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        // A stable sort keeps repeated keys in their original order.
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        u.query_pairs_mut().clear().extend_pairs(&pairs);
    }
    Some(u.into())
}

/// A request's identity for deduplication: a 128-bit digest of its method
/// and canonical URL.
pub type Fingerprint = [u8; 16];

pub fn fingerprint(method: &str, url: &str) -> Option<Fingerprint> {
    let canon = canonical(url)?;
    let mut h = Sha256::new();
    h.update(method.to_ascii_uppercase().as_bytes());
    h.update(b" ");
    h.update(canon.as_bytes());
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_page_written_differently_is_one_url() {
        let same = [
            "https://Example.COM/a?b=2&a=1",
            "https://example.com:443/a?a=1&b=2",
            "https://example.com/a?a=1&b=2#section",
            "https://example.com/a?utm_source=x&a=1&b=2&fbclid=abc",
            "HTTPS://example.com/a?a=1&UTM_Medium=y&b=2",
        ];
        let canon: Vec<String> = same.iter().map(|u| canonical(u).unwrap()).collect();
        assert!(
            canon.iter().all(|c| c == "https://example.com/a?a=1&b=2"),
            "{canon:?}"
        );
    }

    #[test]
    fn different_pages_stay_different() {
        let a = canonical("https://example.com/a?x=1").unwrap();
        for other in [
            "https://example.com/a?x=2",
            "https://example.com/A?x=1",
            "http://example.com/a?x=1",
            "https://example.com:8443/a?x=1",
            "https://example.com/a/?x=1",
            "https://www.example.com/a?x=1",
        ] {
            assert_ne!(canonical(other).unwrap(), a, "{other}");
        }
    }

    #[test]
    fn repeated_keys_keep_their_order() {
        assert_eq!(
            canonical("https://e.com/?b=1&a=2&a=1").unwrap(),
            "https://e.com/?a=2&a=1&b=1"
        );
    }

    #[test]
    fn only_tracking_parameters_leaves_no_query() {
        assert_eq!(
            canonical("https://e.com/p?utm_source=a&gclid=b").unwrap(),
            "https://e.com/p"
        );
    }

    #[test]
    fn only_web_urls_have_a_canonical_form() {
        for bad in [
            "ftp://e.com/",
            "mailto:a@b.c",
            "/relative",
            "",
            "javascript:alert(1)",
            "https://",
        ] {
            assert!(canonical(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn fingerprints_follow_method_and_canonical_url() {
        let a = fingerprint("GET", "https://e.com/?b=1&a=2#x").unwrap();
        assert_eq!(a, fingerprint("get", "https://E.com/?a=2&b=1").unwrap());
        assert_ne!(a, fingerprint("POST", "https://e.com/?a=2&b=1").unwrap());
        assert_ne!(a, fingerprint("GET", "https://e.com/?a=3&b=1").unwrap());
    }
}
