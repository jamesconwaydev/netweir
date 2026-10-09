//! The W3C TDM Reservation Protocol: whether a site reserves its rights to
//! text and data mining, declared in `/.well-known/tdmrep.json`, a
//! `tdm-reservation` response header, or a `<meta name="tdm-reservation">`.
//! Each later source overrides the earlier one when it says anything.

use serde::Deserialize;

/// What a site declares for one URL.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reservation {
    /// `Some(true)`: rights reserved. `None`: the site says nothing.
    pub reserved: Option<bool>,
    /// Where the site's licensing terms for mining are, if it gives them.
    pub policy: Option<String>,
}

impl Reservation {
    /// `later`'s values replace these, but an absent value never clears one.
    pub fn overridden_by(self, later: Reservation) -> Reservation {
        Reservation {
            reserved: later.reserved.or(self.reserved),
            policy: later.policy.or(self.policy),
        }
    }
}

/// The rules in a site's tdmrep.json.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TdmFile {
    rules: Vec<FileRule>,
}

#[derive(Debug, Clone, PartialEq)]
struct FileRule {
    location: String,
    reserved: bool,
    policy: Option<String>,
}

#[derive(Deserialize)]
struct RawRule {
    location: String,
    #[serde(rename = "tdm-reservation")]
    reservation: u8,
    #[serde(rename = "tdm-policy")]
    policy: Option<String>,
}

impl TdmFile {
    /// Parses tdmrep.json. A file that isn't a valid rule list means the
    /// site doesn't implement the protocol, so it is `None`.
    pub fn parse(body: &str) -> Option<TdmFile> {
        let raw: Vec<RawRule> = serde_json::from_str(body).ok()?;
        let mut rules = Vec::with_capacity(raw.len());
        for r in raw {
            if r.reservation > 1 {
                return None;
            }
            rules.push(FileRule {
                location: decode(&r.location),
                reserved: r.reservation == 1,
                policy: r.policy,
            });
        }
        Some(TdmFile { rules })
    }

    /// The first rule matching `path` (path and query) decides.
    pub fn reservation(&self, path: &str) -> Reservation {
        let path = decode(path);
        self.rules
            .iter()
            .find(|r| crate::robots::matches(&r.location, &path))
            .map(|r| Reservation {
                reserved: Some(r.reserved),
                policy: r.policy.clone(),
            })
            .unwrap_or_default()
    }
}

/// What the `tdm-reservation` and `tdm-policy` headers say. Values other
/// than 0 and 1 are protocol errors and count as saying nothing.
pub fn from_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Reservation {
    let mut out = Reservation::default();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("tdm-reservation") {
            out.reserved = match v.trim() {
                "1" => Some(true),
                "0" => Some(false),
                _ => out.reserved,
            };
        } else if k.eq_ignore_ascii_case("tdm-policy") && !v.trim().is_empty() {
            out.policy = Some(v.trim().to_string());
        }
    }
    out
}

/// What `<meta name="tdm-reservation">` and `<meta name="tdm-policy">` in
/// the first part of a page say.
pub fn from_html(body: &[u8]) -> Reservation {
    let reserved =
        crate::decode::meta_content(body, "tdm-reservation").and_then(|v| match v.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        });
    let policy = crate::decode::meta_content(body, "tdm-policy").filter(|p| !p.trim().is_empty());
    Reservation { reserved, policy }
}

/// TDMRep compares paths with escapes decoded, except escapes of characters
/// reserved in URIs, which stay as they are.
fn decode(s: &str) -> String {
    const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=%";
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            && !RESERVED.contains(&v)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"[
        {"location": "/blog/free/*", "tdm-reservation": 0},
        {"location": "/blog/*", "tdm-reservation": 1, "tdm-policy": "https://example.com/policy.json"},
        {"location": "/*.pdf$", "tdm-reservation": 1}
    ]"#;

    #[test]
    fn the_first_matching_rule_decides() {
        let f = TdmFile::parse(FILE).unwrap();
        assert_eq!(f.reservation("/blog/free/post").reserved, Some(false));
        let r = f.reservation("/blog/post");
        assert_eq!(r.reserved, Some(true));
        assert_eq!(r.policy.as_deref(), Some("https://example.com/policy.json"));
        assert_eq!(f.reservation("/docs/a.pdf").reserved, Some(true));
        assert_eq!(
            f.reservation("/docs/a.pdf?x=1").reserved,
            None,
            "$ anchors the end"
        );
        assert_eq!(f.reservation("/").reserved, None);
    }

    #[test]
    fn escapes_are_decoded_except_reserved_characters() {
        let f =
            TdmFile::parse(r#"[{"location": "/caf%C3%A9 menu", "tdm-reservation": 1}]"#).unwrap();
        assert_eq!(f.reservation("/caf\u{e9}%20menu").reserved, Some(true));
        let f = TdmFile::parse(r#"[{"location": "/a%2Fb", "tdm-reservation": 1}]"#).unwrap();
        assert_eq!(
            f.reservation("/a/b").reserved,
            None,
            "%2F is a reserved /, kept encoded"
        );
        assert_eq!(f.reservation("/a%2Fb").reserved, Some(true));
    }

    #[test]
    fn matching_is_case_sensitive() {
        let f = TdmFile::parse(r#"[{"location": "/Blog", "tdm-reservation": 1}]"#).unwrap();
        assert_eq!(f.reservation("/blog").reserved, None);
    }

    #[test]
    fn a_broken_file_means_the_site_does_not_take_part() {
        for bad in [
            "",
            "{}",
            "[{\"location\": 1}]",
            "[{\"location\": \"/\", \"tdm-reservation\": 2}]",
            "not json",
        ] {
            assert!(TdmFile::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            TdmFile::parse("[]").unwrap().reservation("/x").reserved,
            None
        );
    }

    #[test]
    fn headers_and_meta() {
        let h = from_headers([("TDM-Reservation", "1"), ("tdm-policy", " https://x/p ")]);
        assert_eq!(
            h,
            Reservation {
                reserved: Some(true),
                policy: Some("https://x/p".into())
            }
        );
        assert_eq!(
            from_headers([("tdm-reservation", "yes")]).reserved,
            None,
            "a protocol error says nothing"
        );
        let m =
            from_html(br#"<html><head><meta name="tdm-reservation" content="0"><title>x</title>"#);
        assert_eq!(m.reserved, Some(false));
        assert_eq!(from_html(b"<p>nothing</p>"), Reservation::default());
    }

    #[test]
    fn later_sources_override_but_silence_does_not_clear() {
        let file = Reservation {
            reserved: Some(true),
            policy: Some("p".into()),
        };
        let header = Reservation {
            reserved: Some(false),
            policy: None,
        };
        let merged = file
            .overridden_by(header)
            .overridden_by(Reservation::default());
        assert_eq!(
            merged,
            Reservation {
                reserved: Some(false),
                policy: Some("p".into())
            }
        );
    }
}
