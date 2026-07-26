//! Ignore rules for directory packing: `--ignore` globs plus a
//! `.cavsignore` file at the tree root (gitignore-lite).
//!
//! Supported syntax:
//! - `*` matches within one path segment, `?` one character;
//! - `**` matches across segments;
//! - a trailing `/` matches a directory and everything under it;
//! - a pattern without `/` matches the basename at any depth;
//! - a pattern with `/` matches from the tree root;
//! - lines starting with `#` (and blank lines) in `.cavsignore` are skipped.
//!
//! No negation (`!`) — rules only exclude. The `.cavsignore` file itself
//! is always excluded from the package.

use std::path::Path;

pub const IGNORE_FILE: &str = ".cavsignore";

#[derive(Debug, Default, Clone)]
pub struct IgnoreRules {
    patterns: Vec<String>,
}

impl IgnoreRules {
    /// CLI patterns + the root's `.cavsignore` when present.
    pub fn load(root: &Path, cli_patterns: &[String]) -> std::io::Result<Self> {
        let mut patterns: Vec<String> = cli_patterns.to_vec();
        let file = root.join(IGNORE_FILE);
        if file.is_file() {
            for line in std::fs::read_to_string(&file)?.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    patterns.push(line.to_string());
                }
            }
        }
        Ok(IgnoreRules { patterns })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// `rel` is the forward-slash relative path; `is_dir` widens trailing-`/`
    /// rules to the directory entry itself.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if rel == IGNORE_FILE {
            return true;
        }
        self.patterns
            .iter()
            .any(|p| pattern_matches(p, rel, is_dir))
    }
}

fn pattern_matches(pattern: &str, rel: &str, is_dir: bool) -> bool {
    let (pattern, dir_only) = match pattern.strip_suffix('/') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    if pattern.is_empty() {
        return false;
    }
    // Anchored (contains '/') vs basename-at-any-depth.
    let candidates: Vec<&str> = if pattern.contains('/') {
        vec![rel]
    } else {
        rel.split('/').collect()
    };
    if dir_only {
        // "logs/" hits the dir entry itself and everything under it.
        if pattern.contains('/') {
            return (is_dir && glob_match(pattern, rel)) || prefix_dir_match(pattern, rel);
        }
        let segments: Vec<&str> = rel.split('/').collect();
        for (i, seg) in segments.iter().enumerate() {
            let last = i == segments.len() - 1;
            if glob_match(pattern, seg) && (!last || is_dir) {
                return true;
            }
        }
        return false;
    }
    if pattern.contains('/') {
        return glob_match(pattern, rel);
    }
    // Basename rule: any segment matching ignores the entry (a file under
    // an ignored directory name is ignored too, like gitignore).
    candidates.iter().any(|seg| glob_match(pattern, seg))
}

/// Does `rel` live under a directory matching `pattern`?
fn prefix_dir_match(pattern: &str, rel: &str) -> bool {
    rel.char_indices()
        .filter(|&(_, c)| c == '/')
        .any(|(i, _)| glob_match(pattern, &rel[..i]))
}

/// Glob over a path: `*`/`?` stay within one segment, `**` crosses.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' if p.len() > 1 && p[1] == '*' => {
            // `**`: swallow any prefix (including slashes). A following
            // '/' may also match zero segments.
            let rest = if p.len() > 2 && p[2] == '/' {
                &p[3..]
            } else {
                &p[2..]
            };
            (0..=t.len()).any(|i| glob_rec(rest, &t[i..]))
        }
        '*' => (0..=t.len())
            .take_while(|&i| i == 0 || t[i - 1] != '/')
            .any(|i| glob_rec(&p[1..], &t[i..])),
        '?' => !t.is_empty() && t[0] != '/' && glob_rec(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && glob_rec(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(patterns: &[&str]) -> IgnoreRules {
        IgnoreRules {
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn basename_patterns_match_any_depth() {
        let r = rules(&["*.pdb", "*.dSYM"]);
        assert!(r.matches("app.pdb", false));
        assert!(r.matches("bin/x64/app.pdb", false));
        assert!(r.matches("app.dSYM", true));
        assert!(!r.matches("content.pck", false));
        assert!(!r.matches("pdb/readme.txt", false));
    }

    #[test]
    fn directory_patterns_swallow_subtrees() {
        let r = rules(&["logs/", "temp/"]);
        assert!(r.matches("logs", true));
        assert!(r.matches("logs/2026/app.log", false));
        assert!(r.matches("deep/temp/cache.bin", false));
        assert!(!r.matches("logstash.cfg", false));
        assert!(!r.matches("logs.txt", false));
    }

    #[test]
    fn anchored_patterns_match_from_root() {
        let r = rules(&["build/*.o", "docs/**/draft.md"]);
        assert!(r.matches("build/a.o", false));
        assert!(!r.matches("src/build/a.o", false));
        assert!(r.matches("docs/draft.md", false));
        assert!(r.matches("docs/a/b/draft.md", false));
        assert!(!r.matches("docs/a/b/final.md", false));
    }

    #[test]
    fn question_mark_and_star_stay_in_segment() {
        assert!(glob_match("v?.bin", "v1.bin"));
        assert!(!glob_match("v?.bin", "v12.bin"));
        assert!(glob_match("*.log", "a.log"));
        assert!(!glob_match("*.log", "a/b.log"));
        assert!(glob_match("**/b.log", "a/b.log"));
        assert!(glob_match("**", "anything/at/all"));
    }

    #[test]
    fn ignore_file_itself_is_always_excluded() {
        let r = rules(&[]);
        assert!(r.matches(".cavsignore", false));
    }

    #[test]
    fn cavsignore_file_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(IGNORE_FILE),
            "# build junk\n*.pdb\n\nlogs/\n",
        )
        .unwrap();
        let r = IgnoreRules::load(dir.path(), &["*.tmp".into()]).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r.matches("a.tmp", false));
        assert!(r.matches("x/y.pdb", false));
        assert!(r.matches("logs/a.txt", false));
    }
}
