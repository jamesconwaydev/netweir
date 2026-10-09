//! URLs that lead a crawler in circles.

/// Whether `path` repeats the same run of segments three or more times in a
/// row (`/a/a/a`, `/x/y/x/y/x/y`): the mark of relative links that keep
/// resolving one level deeper into the same pages.
pub fn repeats(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let n = segments.len();
    for len in 1..=n / 3 {
        for start in 0..=n - 3 * len {
            let group = &segments[start..start + len];
            if group == &segments[start + len..start + 2 * len]
                && group == &segments[start + 2 * len..start + 3 * len]
            {
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
            "/a/a/a",
            "/x/y/x/y/x/y/page",
            "/shop/c/d/e/c/d/e/c/d/e",
            "/1/2/2/2/3",
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
        ] {
            assert!(!repeats(path), "{path}");
        }
    }
}
