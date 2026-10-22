//! Path-scoped cache keys: which staged entries a gate depends on, and the
//! hash of exactly those entries (see `kb/sluice/design/path-scoped-keys.md`).
//!
//! Pure: no I/O, no git spawn. The entries come from the `ls-files -s -z`
//! output the snapshot already produces ([`StagedEntries`]).
//!
//! Matching follows git's `:(glob)` pathspec semantics (what
//! `git --glob-pathspecs ls-files -- <pattern>` selects), root-relative,
//! byte-wise and case-sensitive:
//!
//! - the whole pattern compared literally matches the path itself and, as a
//!   directory, everything under it (`docs` and `docs/` match `docs/a.md`);
//! - a pattern with `*`, `?` or `[` must then match the full path with
//!   `wildmatch(WM_PATHNAME)`: `*` and `?` never cross `/`, `**/`, `/**/` and
//!   a trailing `/**` span directories. Like git, the literal part before the
//!   first wildcard is compared first and only the rest goes to wildmatch.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::config::{Gate, REPO_CONFIG_FILE};
use crate::tree::{StagedEntries, StagedEntry};

/// Domain-separation header hashed first into every scope hash.
pub const SCOPE_DOMAIN: &str = "sluice-scope-v1";

/// Prefix of a scoped tree field in the cache key material.
pub const SCOPE_KEY_PREFIX: &str = "scope-v1:";

/// SHA-256 (64 lowercase hex characters) of the entries selected by a scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeHash(String);

impl ScopeHash {
    /// The hash as a lowercase hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ScopeHash {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value))
        } else {
            Err(format!(
                "invalid scope hash '{value}': expected 64 lowercase hex characters"
            ))
        }
    }
}

impl fmt::Display for ScopeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How many entries a scope selects out of the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeCount {
    /// Entries matched by the patterns (`.sluice.toml` counts only when the
    /// patterns select it).
    pub files: usize,
    /// All staged entries.
    pub total: usize,
}

/// One validated pattern.
#[derive(Debug, Clone)]
struct Pattern {
    text: String,
    /// Length of the literal part before the first wildcard
    /// (`text.len()` for a literal pattern).
    nowildcard_len: usize,
}

impl Pattern {
    fn new(text: &str) -> Self {
        let nowildcard_len = text.bytes().position(is_glob_special).unwrap_or(text.len());
        Self {
            text: text.to_owned(),
            nowildcard_len,
        }
    }

    fn matches(&self, path: &[u8]) -> bool {
        pattern_matches(self.text.as_bytes(), self.nowildcard_len, path)
    }
}

/// The path scope of a gate: `paths` (optional) and `paths_ignore`.
#[derive(Debug, Clone)]
pub struct PathScope {
    paths: Option<Vec<Pattern>>,
    ignore: Vec<Pattern>,
}

impl PathScope {
    /// The scope of `gate`, `None` when it declares neither `paths` nor
    /// `paths_ignore` (an unscoped gate keys on the root tree).
    #[must_use]
    pub fn from_gate(gate: &Gate) -> Option<Self> {
        Self::new(gate.paths.as_deref(), &gate.paths_ignore)
    }

    /// A scope from raw patterns (already validated by the config parser);
    /// `None` when both lists are absent or empty.
    #[must_use]
    pub fn new(paths: Option<&[String]>, ignore: &[String]) -> Option<Self> {
        if paths.is_none() && ignore.is_empty() {
            return None;
        }
        Some(Self {
            paths: paths.map(|p| p.iter().map(|s| Pattern::new(s)).collect()),
            ignore: ignore.iter().map(|s| Pattern::new(s)).collect(),
        })
    }

    /// `true` when `path` is selected by the patterns: (`paths` unset or any
    /// of them matches) and no `paths_ignore` pattern matches. The forced
    /// inclusion of `.sluice.toml` is not applied here.
    #[must_use]
    pub fn matches(&self, path: &[u8]) -> bool {
        let included = self
            .paths
            .as_ref()
            .is_none_or(|ps| ps.iter().any(|p| p.matches(path)));
        included && !self.ignore.iter().any(|p| p.matches(path))
    }

    /// Indices into `paths` of the patterns that match no entry.
    #[must_use]
    pub fn unmatched_paths(&self, entries: &StagedEntries) -> Vec<usize> {
        let Some(paths) = &self.paths else {
            return Vec::new();
        };
        paths
            .iter()
            .enumerate()
            .filter(|(_, p)| !entries.iter().any(|e| p.matches(e.path)))
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of selected entries and of all entries.
    #[must_use]
    pub fn count(&self, entries: &StagedEntries) -> ScopeCount {
        let mut count = ScopeCount { files: 0, total: 0 };
        for e in entries.iter() {
            count.total += 1;
            if self.matches(e.path) {
                count.files += 1;
            }
        }
        count
    }

    /// The scope hash (design section 4, "Algorithm"): header, patterns in
    /// config order, the `.sluice.toml` entry (or `absent`), then every
    /// selected entry in path byte order as `mode SP oid TAB path`.
    #[must_use]
    pub fn hash(&self, entries: &StagedEntries) -> ScopeHash {
        let config = REPO_CONFIG_FILE.as_bytes();
        let mut cfg: Option<StagedEntry<'_>> = None;
        let mut selected: Vec<StagedEntry<'_>> = Vec::new();
        for e in entries.iter() {
            if e.path == config {
                cfg = Some(e);
            } else if self.matches(e.path) {
                selected.push(e);
            }
        }
        // Index order is path byte order; never let the hash depend on it.
        if !selected.windows(2).all(|w| w[0].path < w[1].path) {
            selected.sort_by(|a, b| a.path.cmp(b.path));
        }

        let mut h = Sha256::new();
        let mut field = |parts: &[&[u8]]| {
            for p in parts {
                h.update(p);
            }
            h.update([0u8]);
        };
        field(&[SCOPE_DOMAIN.as_bytes()]);
        for p in self.paths.iter().flatten() {
            field(&[b"p:", p.text.as_bytes()]);
        }
        for p in &self.ignore {
            field(&[b"i:", p.text.as_bytes()]);
        }
        field(&[]);
        match cfg {
            Some(e) => field(&[config, b" ", e.mode, b" ", e.oid]),
            None => field(&[config, b" absent"]),
        }
        for e in &selected {
            field(&[e.mode, b" ", e.oid, b"\t", e.path]);
        }
        ScopeHash(hex::encode(h.finalize()))
    }
}

/// git's `is_glob_special`: `*`, `?`, `[` and `\`.
fn is_glob_special(b: u8) -> bool {
    matches!(b, b'*' | b'?' | b'[' | b'\\')
}

/// git's `match_pathspec_item` for a `:(glob)` pathspec without magic.
fn pattern_matches(pattern: &[u8], nowildcard_len: usize, path: &[u8]) -> bool {
    // The whole pattern compared literally: the path itself or, as a
    // directory, anything below it.
    if let Some(rest) = path.strip_prefix(pattern)
        && (rest.is_empty() || pattern.last() == Some(&b'/') || rest[0] == b'/')
    {
        return true;
    }
    if nowildcard_len >= pattern.len() {
        return false;
    }
    // git_fnmatch: the literal prefix first, then wildmatch on the rest.
    if path.len() < nowildcard_len || path[..nowildcard_len] != pattern[..nowildcard_len] {
        return false;
    }
    wildmatch(&pattern[nowildcard_len..], &path[nowildcard_len..])
}

const WM_MATCH: i8 = 0;
const WM_NOMATCH: i8 = 1;
const WM_ABORT_ALL: i8 = -1;
const WM_ABORT_TO_STARSTAR: i8 = -2;

/// `wildmatch(pattern, text, WM_PATHNAME)`, a port of git's `wildmatch.c`.
#[must_use]
pub fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    dowild(pattern, 0, text, 0) == WM_MATCH
}

/// Byte at `i`, or NUL past the end (paths never contain NUL).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[allow(clippy::too_many_lines)] // a faithful port reads best as one function
fn dowild(pat: &[u8], mut p: usize, text: &[u8], mut t: usize) -> i8 {
    loop {
        let mut p_ch = at(pat, p);
        if p_ch == 0 {
            break;
        }
        let mut t_ch = at(text, t);
        if t_ch == 0 && p_ch != b'*' {
            return WM_ABORT_ALL;
        }
        match p_ch {
            b'\\' => {
                // Literal match with the next character (the config rejects
                // backslashes; kept for a faithful `wildmatch`).
                p += 1;
                if t_ch != at(pat, p) {
                    return WM_NOMATCH;
                }
            }
            b'?' => {
                // Anything but '/'.
                if t_ch == b'/' {
                    return WM_NOMATCH;
                }
            }
            b'*' => {
                p += 1;
                let match_slash = if at(pat, p) == b'*' {
                    let prev_is_boundary = p < 2 || pat[p - 2] == b'/';
                    while at(pat, p) == b'*' {
                        p += 1;
                    }
                    let next = at(pat, p);
                    if prev_is_boundary
                        && (next == 0 || next == b'/' || (next == b'\\' && at(pat, p + 1) == b'/'))
                    {
                        // `**/` may match nothing: try the rest right here.
                        if next == b'/' && dowild(pat, p + 1, text, t) == WM_MATCH {
                            return WM_MATCH;
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if at(pat, p) == 0 {
                    // Trailing `**` matches everything; trailing `*` only
                    // when no slash is left.
                    if !match_slash && text[t.min(text.len())..].contains(&b'/') {
                        return WM_NOMATCH;
                    }
                    return WM_MATCH;
                } else if !match_slash && at(pat, p) == b'/' {
                    // One `*` followed by a slash matches up to the next one.
                    let Some(off) = text[t.min(text.len())..].iter().position(|b| *b == b'/')
                    else {
                        return WM_NOMATCH;
                    };
                    t += off;
                    p += 1;
                    t += 1;
                    continue;
                }
                loop {
                    if t_ch == 0 {
                        break;
                    }
                    let next = at(pat, p);
                    if !is_glob_special(next) {
                        // Skip ahead to the next occurrence of the literal.
                        loop {
                            t_ch = at(text, t);
                            if t_ch == 0 || (!match_slash && t_ch == b'/') || t_ch == next {
                                break;
                            }
                            t += 1;
                        }
                        if t_ch != next {
                            return if match_slash {
                                WM_ABORT_ALL
                            } else {
                                WM_ABORT_TO_STARSTAR
                            };
                        }
                    }
                    let matched = dowild(pat, p, text, t);
                    if matched != WM_NOMATCH {
                        if !match_slash || matched != WM_ABORT_TO_STARSTAR {
                            return matched;
                        }
                    } else if !match_slash && t_ch == b'/' {
                        return WM_ABORT_TO_STARSTAR;
                    }
                    t += 1;
                    t_ch = at(text, t);
                }
                return WM_ABORT_ALL;
            }
            b'[' => {
                p += 1;
                p_ch = at(pat, p);
                if p_ch == b'^' {
                    p_ch = b'!';
                }
                let negated = p_ch == b'!';
                if negated {
                    p += 1;
                    p_ch = at(pat, p);
                }
                let mut prev_ch: u8 = 0;
                let mut matched = false;
                loop {
                    if p_ch == 0 {
                        return WM_ABORT_ALL;
                    }
                    if p_ch == b'\\' {
                        p += 1;
                        p_ch = at(pat, p);
                        if p_ch == 0 {
                            return WM_ABORT_ALL;
                        }
                        if t_ch == p_ch {
                            matched = true;
                        }
                    } else if p_ch == b'-'
                        && prev_ch != 0
                        && at(pat, p + 1) != 0
                        && at(pat, p + 1) != b']'
                    {
                        p += 1;
                        p_ch = at(pat, p);
                        if p_ch == b'\\' {
                            p += 1;
                            p_ch = at(pat, p);
                            if p_ch == 0 {
                                return WM_ABORT_ALL;
                            }
                        }
                        if t_ch <= p_ch && t_ch >= prev_ch {
                            matched = true;
                        }
                        p_ch = 0; // so that prev_ch becomes 0
                    } else if p_ch == b'[' && at(pat, p + 1) == b':' {
                        p += 2;
                        let s = p;
                        while at(pat, p) != 0 && at(pat, p) != b']' {
                            p += 1;
                        }
                        p_ch = at(pat, p);
                        if p_ch == 0 {
                            return WM_ABORT_ALL;
                        }
                        if p == s || pat[p - 1] != b':' {
                            // No ":]": an ordinary '[' in the set.
                            p = s - 2;
                            p_ch = b'[';
                            if t_ch == p_ch {
                                matched = true;
                            }
                        } else {
                            match char_class(&pat[s..p - 1], t_ch) {
                                Some(true) => matched = true,
                                Some(false) => {}
                                None => return WM_ABORT_ALL,
                            }
                            p_ch = 0;
                        }
                    } else if t_ch == p_ch {
                        matched = true;
                    }
                    prev_ch = p_ch;
                    p += 1;
                    p_ch = at(pat, p);
                    if p_ch == b']' {
                        break;
                    }
                }
                if matched == negated || t_ch == b'/' {
                    return WM_NOMATCH;
                }
            }
            _ => {
                if t_ch != p_ch {
                    return WM_NOMATCH;
                }
            }
        }
        p += 1;
        t += 1;
    }
    if at(text, t) == 0 {
        WM_MATCH
    } else {
        WM_NOMATCH
    }
}

/// `[:name:]` classes (ASCII only, as git's sane ctype); `None` when the
/// class name is unknown (git aborts the match).
fn char_class(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => (0x20..=0x7e).contains(&c),
        b"punct" => c.is_ascii_punctuation(),
        b"space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r'),
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{base_env, git};
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    const OID_A: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    const OID_B: &str = "8baef1b4abc478178b004d62031cf7fe6db6f903";

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scope(paths: &[&str], ignore: &[&str]) -> PathScope {
        PathScope::new(Some(&owned(paths)), &owned(ignore)).expect("scoped")
    }

    fn ignore_only(ignore: &[&str]) -> PathScope {
        PathScope::new(None, &owned(ignore)).expect("scoped")
    }

    fn m(s: &PathScope, path: &str) -> bool {
        s.matches(path.as_bytes())
    }

    fn entries(records: &[(&str, &str, &str)]) -> StagedEntries {
        let mut raw = Vec::new();
        for (mode, oid, path) in records {
            raw.extend_from_slice(format!("{mode} {oid} 0\t{path}").as_bytes());
            raw.push(0);
        }
        StagedEntries::from_raw(raw)
    }

    #[test]
    fn literal_patterns_match_the_path_and_everything_below() {
        for pat in ["docs", "docs/"] {
            let s = scope(&[pat], &[]);
            assert!(m(&s, "docs/a.md") && m(&s, "docs/x/y.md"), "{pat}");
            assert!(!m(&s, "docs2/a.md") && !m(&s, "doc"), "{pat}");
        }
        assert!(m(&scope(&["docs"], &[]), "docs"));
        assert!(!m(&scope(&["docs/"], &[]), "docs"));
        assert!(!m(&scope(&["doc"], &[]), "docs/x"));
        assert!(m(&scope(&["Cargo.toml"], &[]), "Cargo.toml"));
        assert!(!m(&scope(&["Cargo.toml"], &[]), "crates/a/Cargo.toml"));
    }

    #[test]
    fn wildcards_follow_wm_pathname() {
        let s = scope(&["src/*.rs"], &[]);
        assert!(m(&s, "src/x.rs") && !m(&s, "src/a/y.rs"));
        let s = scope(&["**/*.rs"], &[]);
        assert!(m(&s, "top.rs") && m(&s, "src/a/y.rs") && !m(&s, "src/a/y.md"));
        let s = scope(&["src/**"], &[]);
        assert!(m(&s, "src/a") && m(&s, "src/a/b/c") && !m(&s, "src") && !m(&s, "srcx/a"));
        let s = scope(&["a/**/b"], &[]);
        assert!(m(&s, "a/b") && m(&s, "a/x/y/b") && !m(&s, "a/x/y/bc"));
        let s = scope(&["f?.txt"], &[]);
        assert!(m(&s, "f1.txt") && !m(&s, "f12.txt") && !m(&s, "f/.txt"));
        let s = scope(&["f[0-1].txt"], &[]);
        assert!(m(&s, "f0.txt") && m(&s, "f1.txt") && !m(&s, "f2.txt"));
        let s = scope(&["f[!0-1].txt"], &[]);
        assert!(!m(&s, "f0.txt") && m(&s, "f2.txt"));
        let s = scope(&["f[[:digit:]].txt"], &[]);
        assert!(m(&s, "f7.txt") && !m(&s, "fa.txt"));
        // Byte-wise and case-sensitive.
        assert!(!m(&scope(&["*.RS"], &[]), "a.rs"));
    }

    #[test]
    fn a_wildcard_pattern_does_not_recurse() {
        let s = scope(&["src/1*"], &[]);
        assert!(m(&s, "src/1") && m(&s, "src/12"));
        assert!(!m(&s, "src/12/x.rs") && !m(&s, "src/1/x.rs"));
        let s = scope(&["src/1*/**"], &[]);
        assert!(m(&s, "src/12/x.rs") && m(&s, "src/1/a/b.rs") && !m(&s, "src/2/x.rs"));
    }

    #[test]
    fn ignore_alone_combined_and_never_removing_the_config() {
        let s = ignore_only(&["docs", "**/*.png"]);
        assert!(m(&s, "src/a.rs") && m(&s, "README.md"));
        assert!(!m(&s, "docs/a.md") && !m(&s, "web/x.png"));

        let s = scope(&["crates/**", "Cargo.toml"], &["crates/**/*.md"]);
        assert!(m(&s, "crates/a/lib.rs") && m(&s, "Cargo.toml"));
        assert!(!m(&s, "crates/a/README.md") && !m(&s, "docs/x.rs"));

        // `.sluice.toml` is hashed even when ignored or not selected.
        let e = entries(&[
            ("100644", OID_A, ".sluice.toml"),
            ("100644", OID_A, "src/a.rs"),
        ]);
        let e2 = entries(&[
            ("100644", OID_B, ".sluice.toml"),
            ("100644", OID_A, "src/a.rs"),
        ]);
        for s in [
            scope(&["src"], &[".sluice.toml"]),
            scope(&["src"], &[]),
            ignore_only(&["*"]),
        ] {
            assert_ne!(s.hash(&e), s.hash(&e2));
        }
        assert!(PathScope::new(None, &[]).is_none());
    }

    #[test]
    fn count_and_unmatched_patterns() {
        let e = entries(&[
            ("100644", OID_A, ".sluice.toml"),
            ("100644", OID_A, "docs/a.md"),
            ("100644", OID_A, "src/a.rs"),
            ("100644", OID_A, "src/b.rs"),
        ]);
        let s = scope(&["src", "nothing/**"], &[]);
        assert_eq!(s.count(&e), ScopeCount { files: 2, total: 4 });
        assert_eq!(s.unmatched_paths(&e), vec![1]);
        assert_eq!(scope(&["nothing"], &[]).count(&e).files, 0);
        assert_eq!(
            ignore_only(&["docs"]).unmatched_paths(&e),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn hash_changes_with_every_input_and_ignores_order() {
        let base = [
            ("100644", OID_A, ".sluice.toml"),
            ("100644", OID_A, "src/a.rs"),
            ("100755", OID_B, "src/b.rs"),
            ("120000", OID_A, "src/link"),
            ("100644", OID_B, "docs/x.md"),
        ];
        let s = scope(&["src"], &[]);
        let h = s.hash(&entries(&base));

        let mut shuffled = base;
        shuffled.reverse();
        shuffled.swap(0, 2);
        assert_eq!(s.hash(&entries(&shuffled)), h, "order must not matter");

        let mut mode = base;
        mode[1].0 = "100755";
        assert_ne!(s.hash(&entries(&mode)), h, "mode");
        let mut oid = base;
        oid[2].1 = OID_A;
        assert_ne!(s.hash(&entries(&oid)), h, "oid");
        let mut path = base;
        path[1].2 = "src/c.rs";
        assert_ne!(s.hash(&entries(&path)), h, "path");
        assert_ne!(scope(&["src/"], &[]).hash(&entries(&base)), h, "pattern");
        assert_ne!(scope(&["src"], &["x"]).hash(&entries(&base)), h, "ignore");
        // Out-of-scope changes do not matter.
        let mut outside = base;
        outside[4].1 = OID_A;
        assert_eq!(s.hash(&entries(&outside)), h);
        // Absent config differs from present.
        assert_ne!(s.hash(&entries(&base[1..])), h, "absent config");
    }

    #[test]
    fn golden_vector_matches_byte_layout() {
        let e = entries(&[
            ("100644", OID_A, ".sluice.toml"),
            ("100644", OID_B, "docs/x.md"),
            ("100755", OID_B, "src/b.rs"),
            ("100644", OID_A, "src/ñ é.rs"),
        ]);
        let s = scope(&["src/**", "docs"], &["docs/**/*.md"]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"sluice-scope-v1\0p:src/**\0p:docs\0i:docs/**/*.md\0\0");
        bytes.extend_from_slice(format!(".sluice.toml 100644 {OID_A}\0").as_bytes());
        bytes.extend_from_slice(format!("100755 {OID_B}\tsrc/b.rs\0").as_bytes());
        bytes.extend_from_slice(format!("100644 {OID_A}\tsrc/ñ é.rs\0").as_bytes());
        let hash = s.hash(&e);
        assert_eq!(hash.as_str(), hex::encode(Sha256::digest(&bytes)));
        assert_eq!(
            hash.as_str(),
            "cba803ed4717a471e7d34099b784b03575ca033354cb49f421d873f40a36c560",
            "the scope hash layout is frozen"
        );

        let absent = entries(&[("100644", OID_A, "src/b.rs")]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"sluice-scope-v1\0i:x\0\0.sluice.toml absent\0");
        bytes.extend_from_slice(format!("100644 {OID_A}\tsrc/b.rs\0").as_bytes());
        assert_eq!(
            ignore_only(&["x"]).hash(&absent).as_str(),
            hex::encode(Sha256::digest(&bytes))
        );
    }

    /// Paths for the differential test: unicode, spaces, dots, deep
    /// nesting, glob characters in names.
    fn corpus() -> Vec<String> {
        let mut out = owned(&[
            "top.rs",
            "README.md",
            "Cargo.toml",
            ".sluice.toml",
            "doc",
            "docs2/a.md",
            "docs/a.md",
            "docs/x/y.md",
            "a b/c d.txt",
            "ñandú/ü.rs",
            "日本/語.md",
            ".hidden/.x",
            "a.b.c/d..e",
            "src/1",
            "src/12/x.rs",
            "src/1x.rs",
            "src/[ab].rs",
            "src/a.rs",
            "src/b.rs",
            "lit*/x",
            "litx",
            "srcx/a",
            "s/c/x",
            "x",
            "xy/z",
            "Makefile",
            ".dot",
        ]);
        for d1 in ["src", "crates/a", "crates/b", "docs", "web", "a/b/c/d/e"] {
            for d2 in ["", "x/", "12/", "1x/", "deep/er/est/"] {
                for f in [
                    "m.rs", "n.md", "o.txt", "P.RS", "q r.rs", "é.md", ".dot", "Makefile",
                ] {
                    out.push(format!("{d1}/{d2}{f}"));
                }
            }
        }
        for i in 0..300 {
            let ext = ["txt", "rs", "md"][i % 3];
            out.push(format!("gen/{}/{}/f{i}.{ext}", i % 7, i % 11));
        }
        out.sort();
        out.dedup();
        // A path cannot be both a file and a directory in an index.
        let dirs: std::collections::BTreeSet<String> = out
            .iter()
            .flat_map(|p| {
                p.match_indices('/')
                    .map(|(i, _)| p[..i].to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        out.retain(|p| !dirs.contains(p));
        out
    }

    const PATTERNS: &[&str] = &[
        "docs",
        "docs/",
        "doc",
        "src",
        "src/",
        "src/*.rs",
        "**/*.rs",
        "src/**",
        "a/**/e",
        "a/**/m.rs",
        "src/1*",
        "src/1*/**",
        "src/?.rs",
        "src/[0-1]*",
        "src/[!0-9]*",
        "src/[^a]*",
        "*.md",
        "*",
        "**",
        "**/",
        "*/",
        "*/*.md",
        "**/*.md",
        "**/x/**",
        "crates/*/x/*",
        "crates/**/*.md",
        "crates/a",
        "gen/[0-3]/**",
        "gen/*/1*/*",
        "gen/**/f1?.txt",
        "gen/**/f1??.*",
        "**/Makefile",
        "Makefile",
        "**/.dot",
        ".*",
        "**/.*",
        "a b",
        "a b/*",
        "ñandú/**",
        "**/ü.rs",
        "日本/*",
        "a.b.c/*..e",
        "lit*",
        "lit*/x",
        "src/[ab].rs",
        "src/[[:digit:]]*",
        "src/[[:alpha:]].rs",
        "**/P.RS",
        "**/*.RS",
        "web/**/*",
        "web/*/*",
        "docs/**/y.md",
        "docs/*/y.md",
        "deep",
        "**/deep/**",
        "*/deep/er/*",
        "a/b/c/d/e/deep/er/est/*",
        "a**/e",
        "src**",
        "s*c/**",
        "x*",
        "gen/0/0/f0.txt",
        ".sluice.toml",
        "**/er/**/*.txt",
        "crates/**",
        "nonexistent",
        "*/12",
        "*/12/*",
        "src/12",
        "src/1",
        "src/1x/",
        "[sd]*/**",
        "[sd]*",
        "*/*/*/*/*/*",
        "gen/*",
        "gen/**/",
        "a/**",
        "a/**/**/e/*",
        "[]a]*",
        "src/[a-]*",
    ];

    /// What `git --glob-pathspecs ls-files` selects for a pattern set.
    fn git_select(dir: &std::path::Path, paths: Option<&[&str]>, ignore: &[&str]) -> Vec<String> {
        let mut args = owned(&["--glob-pathspecs", "ls-files", "-z", "--"]);
        args.extend(paths.unwrap_or_default().iter().map(|p| (*p).to_owned()));
        args.extend(ignore.iter().map(|p| format!(":(exclude){p}")));
        let out = Command::new("git")
            .args(&args)
            .current_dir(dir)
            .env_clear()
            .envs(base_env())
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8(r.to_vec()).unwrap())
            .collect()
    }

    /// Design section 9, case 5: the in-process selection equals git's.
    #[test]
    fn selection_equals_git_glob_pathspecs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        let corpus = corpus();
        assert!(corpus.len() >= 500, "corpus has {} paths", corpus.len());
        let mut info = Vec::new();
        for p in &corpus {
            info.extend_from_slice(format!("100644 {OID_A}\t{p}").as_bytes());
            info.push(0);
        }
        let mut child = Command::new("git")
            .args(["update-index", "-z", "--index-info"])
            .current_dir(dir)
            .env_clear()
            .envs(base_env())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&info).unwrap();
        assert!(child.wait().unwrap().success());
        let listed = git_select(dir, None, &[]);
        assert_eq!(listed.len(), corpus.len(), "index holds the corpus");
        assert!(PATTERNS.len() >= 60);

        let mut sets: Vec<(Option<Vec<&str>>, Vec<&str>)> = Vec::new();
        for p in PATTERNS {
            sets.push((Some(vec![*p]), vec![]));
            sets.push((None, vec![*p]));
        }
        let n = PATTERNS.len();
        for i in 0..n {
            sets.push((
                Some(vec![PATTERNS[i], PATTERNS[(i * 7 + 3) % n]]),
                vec![PATTERNS[(i * 13 + 5) % n]],
            ));
        }
        sets.push((Some(vec!["src/1*/**", ".sluice.toml"]), vec!["src/12/**"]));
        sets.push((Some(vec!["crates", "docs"]), vec!["**/*.md"]));
        sets.push((None, vec!["docs", "gen/**"]));

        // Each list is asked of git separately. `ls-files -- <paths>
        // :(exclude)<ignore>` would be wrong as an oracle: git strips the
        // common literal prefix of the positive pathspecs from every item,
        // exclusions included, so `crates/a :(exclude)**/x/**` excludes
        // nothing and `crates/a :(exclude)**/src/**` excludes everything
        // (git 2.53). The documented semantics are per pattern.
        for (paths, ignore) in &sets {
            let excluded = if ignore.is_empty() {
                Vec::new()
            } else {
                git_select(dir, Some(ignore), &[])
            };
            let expected: Vec<String> = git_select(dir, paths.as_deref(), &[])
                .into_iter()
                .filter(|p| !excluded.contains(p))
                .collect();
            let p = paths.as_deref().map(owned);
            let s = PathScope::new(p.as_deref(), &owned(ignore)).unwrap();
            let got: Vec<String> = listed
                .iter()
                .filter(|path| s.matches(path.as_bytes()))
                .cloned()
                .collect();
            assert_eq!(got, expected, "paths {paths:?} ignore {ignore:?}");
        }
    }
}
