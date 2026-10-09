//! URLs that lead a crawler in circles.

/// Whether `path` repeats the same run of segments in a row, the mark of
/// relative links that keep resolving one level deeper into the same pages:
/// a run of two or more segments three times (`/x/y/x/y/x/y`), or a single
/// segment four times (`/a/a/a/a`). One segment three times is a real shape
/// too often to refuse: dates (`/01/01/01`), sharded files (`/1/1/1/x.jpg`).
pub fn repeats(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let n = segments.len();
    for len in 1..=n / 3 {
        let times = if len == 1 { 4 } else { 3 };
        if len * times > n {
            continue;
        }
        for start in 0..=n - len * times {
            let group = &segments[start..start + len];
            if (1..times).all(|k| group == &segments[start + k * len..start + (k + 1) * len]) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::repeats;

    #[test]
    fn three_in_a_row_is_a_trap() {
        for path in [
            "/a/a/a/a",
            "/x/y/x/y/x/y/page",
            "/shop/c/d/e/c/d/e/c/d/e",
            "/1/2/2/2/2/3",
        ] {
            assert!(repeats(path), "{path}");
        }
    }

    #[test]
    fn ordinary_paths_are_not() {
        for path in [
            "/",
            "",
            "/a/a",
            "/x/y/x/y/page",
            "/2024/10/10/story",
            "/a/b/a/c/a",
            "/catalogue/page-1/page-1",
            // Real shapes with one segment three times: dates, sharded
            // assets, letter indexes, language paths.
            "/01/01/01/story",
            "/images/1/1/1/photo.jpg",
            "/dictionary/a/a/a",
            "/en/en/en",
        ] {
            assert!(!repeats(path), "{path}");
        }
    }
}
