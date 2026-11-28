//! Path globs for review filters: `*` and `?` stay inside one path segment, `**`
//! spans any number of segments, and a pattern without `/` matches the file name at
//! any depth (`*.min.js` is `**/*.min.js`). Matching is case-sensitive.

pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if pattern.is_empty() {
        return false;
    }
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let pat_segs: Vec<&str> = if pattern.contains('/') {
        pattern.split('/').filter(|s| !s.is_empty()).collect()
    } else {
        vec!["**", pattern]
    };
    segments(&pat_segs, &path_segs)
}

fn segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| segments(rest, &path[i..])),
        Some((p, rest)) => {
            !path.is_empty()
                && segment(p.as_bytes(), path[0].as_bytes())
                && segments(rest, &path[1..])
        }
    }
}

fn segment(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| segment(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && segment(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && segment(rest, &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn segments_double_stars_and_basenames() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/a/main.rs"));
        assert!(matches("src/**/*.rs", "src/main.rs"));
        assert!(matches("src/**/*.rs", "src/a/b/main.rs"));
        assert!(matches("**/dist/**", "web/dist/app.js"));
        assert!(matches("dist/**", "dist/a/b.js"));
        assert!(!matches("dist/**", "web/dist/a.js"));
        assert!(matches("*.min.js", "public/js/app.min.js"));
        assert!(matches("Gemfile.lock", "Gemfile.lock"));
        assert!(matches("Gemfile.lock", "engines/x/Gemfile.lock"));
        assert!(matches(
            "db/migrate/2026??01*.rb",
            "db/migrate/20261101_add.rb"
        ));
        assert!(!matches("db/migrate/*.rb", "db/schema.rb"));
        assert!(!matches("", "a"));
        assert!(!matches("A.rb", "a.rb"));
        assert!(matches("./app/**", "app/x.rb"));
    }
}
