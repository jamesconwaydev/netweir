//! Reading sitemaps: the XML `urlset` and `sitemapindex` formats of
//! sitemaps.org, gzipped or not, and plain-text sitemaps of one URL per
//! line.

/// What a sitemap lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Pages.
    Urlset,
    /// More sitemaps.
    Index,
}

/// One `<url>` or `<sitemap>` entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub loc: String,
    pub lastmod: Option<String>,
    pub changefreq: Option<String>,
    pub priority: Option<f64>,
    /// `<xhtml:link rel="alternate" href=...>`: the page in other languages.
    pub alternates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sitemap {
    pub kind: Kind,
    pub entries: Vec<Entry>,
}

/// Parses `body`, unzipping it first if it's gzip, to at most `max` bytes.
///
/// Entities a document declares are left as written, never expanded, so a
/// "billion laughs" sitemap costs no more than its own size.
pub fn parse(body: &[u8], max: usize) -> Result<Sitemap, String> {
    let data = if body.starts_with(&[0x1f, 0x8b]) {
        gunzip(body, max)?
    } else if body.len() > max {
        return Err(format!(
            "the sitemap is larger than the limit of {max} bytes"
        ));
    } else {
        body.to_vec()
    };
    let text = String::from_utf8_lossy(&data);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with('<') {
        xml(text)
    } else {
        plain(text)
    }
}

fn gunzip(body: &[u8], max: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("the sitemap isn't valid gzip: {e}"))?;
    if out.len() > max {
        return Err(format!(
            "the sitemap unzips to more than the limit of {max} bytes"
        ));
    }
    Ok(out)
}

/// One http(s) URL per line; anything else is skipped.
fn plain(text: &str) -> Result<Sitemap, String> {
    let entries: Vec<Entry> = text
        .lines()
        .map(str::trim)
        .filter(|l| url::Url::parse(l).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
        .map(|l| Entry {
            loc: l.to_string(),
            ..Entry::default()
        })
        .collect();
    if entries.is_empty() {
        return Err("not a sitemap: neither XML nor a list of URLs".into());
    }
    Ok(Sitemap {
        kind: Kind::Urlset,
        entries,
    })
}

fn xml(text: &str) -> Result<Sitemap, String> {
    use quick_xml::events::Event;
    let bad = |e: &dyn std::fmt::Display| format!("the sitemap isn't valid XML: {e}");
    let mut reader = quick_xml::Reader::from_str(text);
    let mut kind = None;
    // Local names of the open elements, outermost first.
    let mut open: Vec<String> = Vec::new();
    let mut entry: Option<Entry> = None;
    let mut entries = Vec::new();
    let mut buf = String::new();
    loop {
        match reader.read_event().map_err(|e| bad(&e))? {
            Event::Start(e) => {
                let name = local(e.local_name().as_ref());
                match (open.len(), name.as_str()) {
                    (0, "urlset") => kind = Some(Kind::Urlset),
                    (0, "sitemapindex") => kind = Some(Kind::Index),
                    (1, "url" | "sitemap") if kind.is_some() => entry = Some(Entry::default()),
                    _ => {}
                }
                open.push(name);
                buf.clear();
            }
            Event::Empty(e) => {
                // <xhtml:link rel="alternate" href="..."/> directly in a <url>.
                if open.len() == 2
                    && local(e.local_name().as_ref()) == "link"
                    && let Some(entry) = entry.as_mut()
                {
                    let attr = |name: &str| {
                        e.attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == name)
                            .and_then(|a| {
                                a.normalized_value(quick_xml::XmlVersion::default())
                                    .ok()
                                    .map(|v| v.into_owned())
                            })
                    };
                    if attr("rel").is_some_and(|r| r.eq_ignore_ascii_case("alternate"))
                        && let Some(href) = attr("href")
                    {
                        entry.alternates.push(href.trim().to_string());
                    }
                }
            }
            Event::Text(t) => buf.push_str(&t.into_inner()),
            Event::CData(c) => buf.push_str(&c.into_inner()),
            Event::GeneralRef(r) => match r.resolve_char_ref().map_err(|e| bad(&e))? {
                Some(c) => buf.push(c),
                None => {
                    let name = r.into_inner();
                    match name.as_ref() {
                        "amp" => buf.push('&'),
                        "lt" => buf.push('<'),
                        "gt" => buf.push('>'),
                        "quot" => buf.push('"'),
                        "apos" => buf.push('\''),
                        // Declared by the document: kept as written.
                        other => {
                            buf.push('&');
                            buf.push_str(other);
                            buf.push(';');
                        }
                    }
                }
            },
            Event::End(_) => {
                let name = open.pop().unwrap_or_default();
                match (open.len(), name.as_str()) {
                    // A field directly inside <url> or <sitemap>.
                    (2, field) => {
                        if let Some(entry) = entry.as_mut() {
                            let value = buf.trim().to_string();
                            match field {
                                "loc" => entry.loc = value,
                                "lastmod" => entry.lastmod = Some(value),
                                "changefreq" => entry.changefreq = Some(value),
                                "priority" => entry.priority = value.parse().ok(),
                                _ => {}
                            }
                        }
                    }
                    (1, "url" | "sitemap") => {
                        if let Some(entry) = entry.take().filter(|e| !e.loc.is_empty()) {
                            entries.push(entry);
                        }
                    }
                    _ => {}
                }
                buf.clear();
            }
            Event::Eof if !open.is_empty() => {
                return Err(format!(
                    "the sitemap ends part way through <{}>",
                    open.join("><")
                ));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let kind = kind.ok_or("not a sitemap: no <urlset> or <sitemapindex>")?;
    Ok(Sitemap { kind, entries })
}

fn local(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1 << 20;

    #[test]
    fn a_urlset_gives_each_page_with_what_it_says_about_it() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:xhtml="http://www.w3.org/1999/xhtml"
        xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
  <url>
    <loc> https://example.com/a?x=1&amp;y=2 </loc>
    <lastmod>2026-10-01</lastmod>
    <changefreq>daily</changefreq>
    <priority>0.8</priority>
    <xhtml:link rel="alternate" hreflang="de" href="https://example.com/de/a"/>
    <image:image><image:loc>https://cdn.example.com/a.jpg</image:loc></image:image>
  </url>
  <url><loc><![CDATA[https://example.com/b]]></loc></url>
</urlset>"#;
        let map = parse(xml, MAX).unwrap();
        assert_eq!(map.kind, Kind::Urlset);
        assert_eq!(
            map.entries,
            [
                Entry {
                    loc: "https://example.com/a?x=1&y=2".into(),
                    lastmod: Some("2026-10-01".into()),
                    changefreq: Some("daily".into()),
                    priority: Some(0.8),
                    alternates: vec!["https://example.com/de/a".into()],
                },
                Entry {
                    loc: "https://example.com/b".into(),
                    ..Entry::default()
                },
            ]
        );
    }

    #[test]
    fn an_index_lists_more_sitemaps() {
        let xml = br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/s1.xml.gz</loc><lastmod>2026-09-30</lastmod></sitemap>
  <sitemap><loc>https://example.com/s2.xml</loc></sitemap>
</sitemapindex>"#;
        let map = parse(xml, MAX).unwrap();
        assert_eq!(map.kind, Kind::Index);
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(
            locs,
            [
                "https://example.com/s1.xml.gz",
                "https://example.com/s2.xml"
            ]
        );
        assert_eq!(map.entries[0].lastmod.as_deref(), Some("2026-09-30"));
    }

    #[test]
    fn a_gzipped_sitemap_is_unzipped() {
        use std::io::Write;
        let xml = b"<urlset><url><loc>https://example.com/z</loc></url></urlset>";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(xml).unwrap();
        let map = parse(&gz.finish().unwrap(), MAX).unwrap();
        assert_eq!(map.entries[0].loc, "https://example.com/z");
    }

    #[test]
    fn a_gzipped_sitemap_is_unzipped_no_further_than_the_limit() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&vec![b' '; 10_000_000]).unwrap();
        let err = parse(&gz.finish().unwrap(), 100_000).unwrap_err();
        assert!(err.contains("100000"), "{err}");
    }

    #[test]
    fn a_text_sitemap_is_one_url_per_line() {
        let text = b"https://example.com/1\n\n  https://example.com/2\r\nnot a url\n";
        let map = parse(text, MAX).unwrap();
        assert_eq!(map.kind, Kind::Urlset);
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(locs, ["https://example.com/1", "https://example.com/2"]);
    }

    #[test]
    fn entities_a_document_declares_are_never_expanded() {
        // The "billion laughs": each entity ten of the one before.
        let mut xml = String::from("<?xml version=\"1.0\"?><!DOCTYPE urlset [<!ENTITY a0 \"ha\">");
        for i in 1..10 {
            let prev = format!("&a{};", i - 1).repeat(10);
            xml.push_str(&format!("<!ENTITY a{i} \"{prev}\">"));
        }
        xml.push_str("]><urlset><url><loc>https://example.com/&a9;</loc></url></urlset>");
        let result = parse(xml.as_bytes(), MAX);
        // Refused, or kept unexpanded; never a billion "ha"s.
        if let Ok(map) = result {
            assert!(map.entries.iter().all(|e| e.loc.len() < 100), "{map:?}");
        }
    }

    #[test]
    fn a_sitemap_cut_off_part_way_is_an_error() {
        let err = parse(b"<urlset><url><loc>https://example.com/a</loc>", MAX).unwrap_err();
        assert!(err.contains("part way"), "{err}");
    }

    #[test]
    fn neither_xml_nor_urls_is_an_error() {
        assert!(parse(b"<html><body>Not found</body></html>", MAX).is_err());
        assert!(parse(b"just some words", MAX).is_err());
    }
}
