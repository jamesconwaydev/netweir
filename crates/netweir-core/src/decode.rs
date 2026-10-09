//! Bytes to text, finding the encoding the way a browser does: byte order
//! mark, then the Content-Type header, then a `<meta>` charset near the
//! top of the page, then a guess from the bytes themselves.

use encoding_rs::{Encoding, UTF_8};

pub fn decode(body: &[u8], content_type: Option<&str>) -> String {
    let encoding = Encoding::for_bom(body)
        .map(|(e, _)| e)
        .or_else(|| content_type.and_then(charset_param))
        .or_else(|| meta_charset(body))
        .unwrap_or_else(|| guess(body));
    // `decode` strips a BOM and replaces malformed sequences with U+FFFD.
    encoding.decode(body).0.into_owned()
}

fn charset_param(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| Encoding::for_label(v.trim().trim_matches(['"', '\'']).as_bytes()))
            .flatten()
    })
}

/// Looks for `charset=` in the first 1024 bytes, as the HTML spec's
/// prescan does. Covers both `<meta charset=x>` and the http-equiv form.
fn meta_charset(body: &[u8]) -> Option<&'static Encoding> {
    let head = &body[..body.len().min(1024)];
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = find(&lower[from..], b"charset") {
        let mut j = from + i + b"charset".len();
        while lower.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
            j += 1;
        }
        if lower.get(j) == Some(&b'=') {
            j += 1;
            while lower
                .get(j)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'"' || *b == b'\'')
            {
                j += 1;
            }
            let end = lower[j..]
                .iter()
                .position(|b| {
                    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b';' | b'>' | b'/')
                })
                .map_or(lower.len(), |p| j + p);
            if let Some(e) = Encoding::for_label(&lower[j..end]) {
                // A page can't truly be UTF-16 if we're reading ASCII here.
                return Some(
                    if e == encoding_rs::UTF_16LE || e == encoding_rs::UTF_16BE {
                        UTF_8
                    } else {
                        e
                    },
                );
            }
        }
        from = from + i + 1;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn guess(body: &[u8]) -> &'static Encoding {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(body, true);
    detector.guess(None, chardetng::Utf8Detection::Allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_by_default() {
        assert_eq!(decode("café".as_bytes(), None), "café");
    }

    #[test]
    fn header_charset_wins_over_meta() {
        let body = b"<meta charset=utf-8>caf\xe9";
        assert_eq!(
            decode(body, Some("text/html; charset=ISO-8859-1")),
            "<meta charset=utf-8>café"
        );
    }

    #[test]
    fn meta_charset_is_found() {
        let body = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\"></head>\xcf\xf0\xe8\xe2\xe5\xf2";
        assert!(decode(body, Some("text/html")).ends_with("Привет"));
        assert_eq!(
            decode(b"<meta charset='shift_jis'>\x93\xfa\x96\x7b", None),
            "<meta charset='shift_jis'>日本"
        );
    }

    #[test]
    fn bom_wins_over_everything() {
        let body = b"\xef\xbb\xbf<meta charset=latin1>\xc3\xa9";
        assert_eq!(
            decode(body, Some("text/html; charset=latin1")),
            "<meta charset=latin1>é"
        );
    }

    #[test]
    fn unlabelled_legacy_text_is_guessed() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251
            .encode("Это обычная русская страница без указания кодировки.");
        assert_eq!(
            decode(&bytes, None),
            "Это обычная русская страница без указания кодировки."
        );
    }

    #[test]
    fn bad_bytes_become_replacement_characters() {
        assert_eq!(
            decode(b"<meta charset=utf-8>a\xffb", None),
            "<meta charset=utf-8>a\u{fffd}b"
        );
    }

    #[test]
    fn quoted_and_unknown_header_charsets() {
        assert_eq!(
            decode(b"caf\xe9", Some("text/html; charset=\"latin1\"")),
            "café"
        );
        assert_eq!(
            decode("café".as_bytes(), Some("text/html; charset=klingon")),
            "café"
        );
    }
}
