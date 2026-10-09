//! robots.txt, as RFC 9309 defines it.
//!
//! Rules apply to the group whose user-agent line names the crawler's
//! product token (case-insensitively), or failing that to the `*` group.
//! The longest matching rule wins and `allow` wins a tie. `*` in a rule
//! matches any characters and a final `$` anchors it to the end of the path.

use std::time::Duration;

/// robots.txt larger than this is cut here, as RFC 9309 allows.
pub const MAX_BYTES: usize = 500 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Robots {
    groups: Vec<Group>,
    sitemaps: Vec<String>,
    /// Set when the file could not be fetched and everything is off limits.
    disallow_all: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    allow: bool,
    /// Normalised as `normalize` does the path.
    pattern: String,
}

impl Robots {
    /// A site with no robots.txt (it answered 4xx): everything is allowed.
    pub fn allow_all() -> Robots {
        Robots {
            groups: Vec::new(),
            sitemaps: Vec::new(),
            disallow_all: false,
        }
    }

    /// A site whose robots.txt could not be read (5xx, or no answer):
    /// RFC 9309 says to assume everything is disallowed.
    pub fn disallow_all() -> Robots {
        Robots {
            groups: Vec::new(),
            sitemaps: Vec::new(),
            disallow_all: true,
        }
    }

    pub fn parse(body: &str) -> Robots {
        let body = &body[..floor_char_boundary(body, MAX_BYTES)];
        let mut groups: Vec<Group> = Vec::new();
        let mut sitemaps = Vec::new();
        // A run of user-agent lines starts a group; a user-agent line after
        // a rule starts the next one.
        let mut collecting_agents = false;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match field.trim().to_ascii_lowercase().as_str() {
                "user-agent" => {
                    if !collecting_agents {
                        groups.push(Group::default());
                        collecting_agents = true;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.agents.push(value.to_ascii_lowercase());
                    }
                }
                field @ ("allow" | "disallow") => {
                    collecting_agents = false;
                    let Some(g) = groups.last_mut() else { continue };
                    // An empty Disallow allows everything: no rule at all.
                    if value.is_empty() {
                        continue;
                    }
                    g.rules.push(Rule {
                        allow: field == "allow",
                        pattern: normalize(value),
                    });
                }
                "crawl-delay" => {
                    collecting_agents = false;
                    if let (Some(g), Ok(secs)) = (groups.last_mut(), value.parse::<f64>())
                        && secs.is_finite()
                        && secs >= 0.0
                    {
                        g.crawl_delay = Some(secs);
                    }
                }
                "sitemap" => sitemaps.push(value.to_string()),
                _ => {}
            }
        }
        Robots {
            groups,
            sitemaps,
            disallow_all: false,
        }
    }

    /// The groups that apply to `agent`: those naming it, else the `*` ones.
    fn groups_for(&self, agent: &str) -> Vec<&Group> {
        let agent = agent.to_ascii_lowercase();
        let named: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| g.agents.contains(&agent))
            .collect();
        if !named.is_empty() {
            return named;
        }
        self.groups
            .iter()
            .filter(|g| g.agents.iter().any(|a| a == "*"))
            .collect()
    }

    /// Whether `agent` may fetch `path` (the URL's path and query).
    pub fn allowed(&self, agent: &str, path: &str) -> bool {
        if path == "/robots.txt" {
            return true;
        }
        if self.disallow_all {
            return false;
        }
        let path = normalize(if path.is_empty() { "/" } else { path });
        let mut best: Option<(usize, bool)> = None;
        for group in self.groups_for(agent) {
            for rule in &group.rules {
                if matches(&rule.pattern, &path) {
                    let len = rule.pattern.len();
                    best = match best {
                        Some((l, allow)) if l > len || (l == len && allow) => Some((l, allow)),
                        _ => Some((len, rule.allow)),
                    };
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }

    /// The Crawl-delay the site asks of `agent`, if any. Not part of RFC
    /// 9309, but widely written and honoured.
    pub fn crawl_delay(&self, agent: &str) -> Option<Duration> {
        self.groups_for(agent)
            .iter()
            .filter_map(|g| g.crawl_delay)
            .reduce(f64::max)
            // Too large for a Duration means "as slow as allowed"; the
            // crawler caps it at its max_delay.
            .map(|s| Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX))
    }

    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }
}

fn floor_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    (0..=at).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

/// RFC 9309 compares paths after percent-encoding normalisation: encoded
/// unreserved characters are decoded, other escapes have uppercase hex, and
/// non-ASCII characters are encoded as UTF-8.
fn normalize(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // `get` rather than indexing: the two bytes after % may not be
        // characters on their own (`%aé`).
        if b == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                out.push(v as char);
            } else {
                out.push('%');
                out.push_str(&hex.to_ascii_uppercase());
            }
            i += 3;
            continue;
        }
        if b.is_ascii() {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        i += 1;
    }
    out
}

/// A robots.txt pattern against a normalised path: a prefix match where `*`
/// matches any run of characters and a trailing `$` must meet the end.
pub(crate) fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let p = pattern.as_bytes();
    let s = path.as_bytes();
    // Iterative wildcard match with one backtrack point, linear in practice.
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut star_si) = (None, 0);
    loop {
        if pi == p.len() {
            if !anchored || si == s.len() {
                return true;
            }
        } else if p[pi] == b'*' {
            star = Some(pi);
            star_si = si;
            pi += 1;
            continue;
        } else if si < s.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
            continue;
        }
        match star {
            Some(sp) if star_si < s.len() => {
                star_si += 1;
                si = star_si;
                pi = sp + 1;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROBOTS: &str = "
# a comment
User-agent: netweir
User-Agent: OtherBot
Disallow: /private
Allow: /private/ok
Crawl-delay: 2.5

User-agent: *
Disallow: /
Allow: /public
Disallow: /*.pdf$
Allow: /public/*.pdf$

Sitemap: https://example.com/sitemap.xml
";

    #[test]
    fn our_own_group_wins_over_the_star_group() {
        let r = Robots::parse(ROBOTS);
        assert!(r.allowed("netweir", "/"));
        assert!(r.allowed("NetWeir", "/anything"));
        assert!(!r.allowed("netweir", "/private/x"));
        assert!(r.allowed("netweir", "/private/ok/x"), "longer allow wins");
        assert!(r.allowed("otherbot", "/public"));
    }

    #[test]
    fn everyone_else_gets_the_star_group() {
        let r = Robots::parse(ROBOTS);
        assert!(!r.allowed("somebot", "/"));
        assert!(r.allowed("somebot", "/public/page"));
        assert!(
            !r.allowed("somebot", "/files/report.pdf"),
            "/*.pdf$ outranks /"
        );
        assert!(
            r.allowed("somebot", "/public.pdf"),
            "/public and /*.pdf$ are both 7 octets: a tie, so allow"
        );
        assert!(
            r.allowed("somebot", "/public/report.pdf"),
            "the longest rule, an allow"
        );
        assert!(
            r.allowed("somebot", "/public/report.pdf?x"),
            "$ anchors at the end; the query breaks it"
        );
    }

    #[test]
    fn ties_go_to_allow_and_robots_txt_is_always_allowed() {
        let r = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a\n");
        assert!(r.allowed("x", "/a/b"));
        assert!(Robots::disallow_all().allowed("x", "/robots.txt"));
    }

    #[test]
    fn empty_disallow_and_missing_files_allow_everything() {
        assert!(Robots::parse("User-agent: *\nDisallow:\n").allowed("x", "/admin"));
        assert!(Robots::parse("").allowed("x", "/admin"));
        assert!(Robots::allow_all().allowed("x", "/admin"));
        assert!(!Robots::disallow_all().allowed("x", "/"));
    }

    #[test]
    fn escapes_cut_short_by_a_multibyte_character_are_kept() {
        let r = Robots::parse("User-agent: *\nDisallow: /%aé\nDisallow: /x%\nDisallow: /y%4\n");
        assert!(!r.allowed("netweir", "/%aé"));
        assert!(!r.allowed("netweir", "/x%"));
        assert!(r.allowed("netweir", "/other"));
        assert_eq!(
            Robots::parse("User-agent: *\nCrawl-delay: 1e300\n").crawl_delay("x"),
            Some(Duration::MAX)
        );
    }

    #[test]
    fn any_text_parses_without_panicking() {
        // A fixed-seed xorshift, so a failure reproduces.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let alphabet: Vec<char> = "%aAé€😀*$/:\n \r#\u{feff}disallow:Allow-user agent0123456789"
            .chars()
            .collect();
        for _ in 0..2000 {
            let mut text = String::new();
            for _ in 0..(x % 200) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                text.push(alphabet[(x % alphabet.len() as u64) as usize]);
            }
            let r = Robots::parse(&text);
            let _ = r.allowed("netweir", &text);
            let _ = r.crawl_delay("netweir");
        }
    }

    #[test]
    fn crawl_delay_and_sitemaps() {
        let r = Robots::parse(ROBOTS);
        assert_eq!(r.crawl_delay("netweir"), Some(Duration::from_millis(2500)));
        assert_eq!(r.crawl_delay("somebot"), None);
        assert_eq!(r.sitemaps(), ["https://example.com/sitemap.xml"]);
        assert_eq!(
            Robots::parse("User-agent: *\nCrawl-delay: -1\n").crawl_delay("x"),
            None
        );
    }

    #[test]
    fn percent_encoding_is_compared_normalised() {
        let r = Robots::parse(
            "User-agent: *\nDisallow: /a%3cb\nDisallow: /%7Euser\nDisallow: /caf\u{e9}\n",
        );
        assert!(!r.allowed("x", "/a%3Cb"), "hex case doesn't matter");
        assert!(
            !r.allowed("x", "/~user/x"),
            "%7E is ~, an unreserved character"
        );
        assert!(
            !r.allowed("x", "/caf%C3%A9"),
            "non-ASCII in the file matches its UTF-8 escape"
        );
    }

    #[test]
    fn wildcards() {
        assert!(matches("/a*b", "/axxb"));
        assert!(matches("/a*b", "/axxbyy"), "prefix match");
        assert!(!matches("/a*b$", "/axxbyy"));
        assert!(matches("/a*b$", "/axbxb"));
        assert!(matches("*", "/anything"));
        assert!(matches("/*/end$", "/x/y/end"));
        assert!(!matches("/abc", "/ab"));
    }

    #[test]
    fn rules_before_any_user_agent_are_ignored_and_junk_is_skipped() {
        let r =
            Robots::parse("Disallow: /\nnonsense line\nUser-agent: *\nDisallow: /x\nfoo: bar\n");
        assert!(r.allowed("a", "/"));
        assert!(!r.allowed("a", "/x"));
    }

    #[test]
    fn huge_files_are_cut_at_500_kib() {
        let mut body = "User-agent: *\n".to_string();
        body.push_str(&"Allow: /filler\n".repeat(MAX_BYTES / 15));
        body.push_str("Disallow: /late\n");
        assert!(
            Robots::parse(&body).allowed("x", "/late"),
            "rules past the cut are not read"
        );
    }
}
