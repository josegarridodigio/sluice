//! Package scripts as heavy-command sources.
//!
//! `npm run axe` is heavy when the script behind it runs something heavy
//! (`playwright test`), and Sluice cannot know that from the words alone.
//! This module finds the nearest `package.json`, reads `scripts.<name>`,
//! splits the body on `&&`, `||`, `;`, `|` (and `&`, newlines) and lets the
//! caller classify each piece, recursing into nested `npm run x` pieces.
//! Everything is bounded: the file at most [`MAX_PACKAGE_JSON`] bytes, the
//! nesting at [`MAX_DEPTH`], cycles end the walk. Anything unreadable or
//! invalid means "not heavy". Design: `kb/sluice/design/command-control.md`
//! section 4.5.

use std::collections::BTreeMap;
use std::path::Path;

/// Largest `package.json` read.
pub const MAX_PACKAGE_JSON: u64 = 1024 * 1024;

/// Deepest chain of scripts that call scripts.
pub const MAX_DEPTH: usize = 4;

/// Where package scripts come from (the real file system, or a fake in
/// tests). Implementations must never block for long and never fail loudly.
pub trait ScriptSource {
    /// The text of `<dir>/package.json`, `None` when it does not exist, is
    /// unreadable or is larger than [`MAX_PACKAGE_JSON`].
    fn package_json(&self, dir: &Path) -> Option<String>;
    /// True when `dir` is the root of a git repository (the walk up stops
    /// there).
    fn is_repo_root(&self, dir: &Path) -> bool;
}

/// A source with no `package.json` at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoScripts;

impl ScriptSource for NoScripts {
    fn package_json(&self, _dir: &Path) -> Option<String> {
        None
    }

    fn is_repo_root(&self, _dir: &Path) -> bool {
        false
    }
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsScripts;

impl ScriptSource for FsScripts {
    fn package_json(&self, dir: &Path) -> Option<String> {
        use std::io::Read;
        let file = std::fs::File::open(dir.join("package.json")).ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MAX_PACKAGE_JSON {
            return None;
        }
        let mut text = String::new();
        file.take(MAX_PACKAGE_JSON).read_to_string(&mut text).ok()?;
        Some(text)
    }

    fn is_repo_root(&self, dir: &Path) -> bool {
        dir.join(".git").exists()
    }
}

/// The `scripts` of one `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scripts {
    scripts: BTreeMap<String, String>,
}

impl Scripts {
    /// The scripts of the nearest `package.json` from `dir` upwards, up to
    /// the git root (or `/`). The nearest file wins even when it has no
    /// scripts; an invalid one gives `None`.
    pub fn find(src: &dyn ScriptSource, dir: &Path) -> Option<Self> {
        for d in dir.ancestors() {
            if let Some(text) = src.package_json(d) {
                return Self::parse(&text);
            }
            if src.is_repo_root(d) {
                break;
            }
        }
        None
    }

    /// Parses the `scripts` object of a `package.json` text; non-string
    /// entries are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let scripts = value
            .get("scripts")?
            .as_object()?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect();
        Some(Self { scripts })
    }

    /// The body of script `name`.
    pub fn body(&self, name: &str) -> Option<&str> {
        self.scripts.get(name).map(String::as_str)
    }
}

/// Accumulates the words and pieces of a script body.
#[derive(Default)]
struct Splitter {
    out: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    in_word: bool,
}

impl Splitter {
    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }

    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_piece(&mut self) {
        self.end_word();
        let piece = strip_launchers(&std::mem::take(&mut self.words));
        if !piece.is_empty() {
            self.out.push(piece);
        }
    }
}

/// The commands of a script body: split on `&&`, `||`, `;`, `|`, `&` and
/// newlines outside quotes, each piece as words (quotes removed) without
/// leading `NAME=value` assignments and `env` / `cross-env` launchers.
/// Empty pieces are dropped.
pub fn pieces(body: &str) -> Vec<Vec<String>> {
    let mut s = Splitter::default();
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(n) = chars.next() {
                    s.push(n);
                }
            } else {
                s.push(c);
            }
            prev = c;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                s.in_word = true;
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    s.push(n);
                }
            }
            // `2>&1` and `&>` are redirections, not separators.
            '&' if prev == '>' || prev == '<' || chars.peek() == Some(&'>') => s.push(c),
            ';' | '\n' | '|' | '&' => s.end_piece(),
            c if c.is_whitespace() => s.end_word(),
            _ => s.push(c),
        }
        prev = c;
    }
    s.end_piece();
    s.out
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn strip_launchers(words: &[String]) -> Vec<String> {
    let skip = words
        .iter()
        .take_while(|w| {
            is_assignment(w) || matches!(w.as_str(), "env" | "cross-env" | "cross-env-shell")
        })
        .count();
    words[skip..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(body: &str) -> Vec<String> {
        pieces(body).iter().map(|w| w.join(" ")).collect()
    }

    #[test]
    fn splits_on_operators_outside_quotes() {
        assert_eq!(p("ng test"), ["ng test"]);
        assert_eq!(
            p("npm run a && npm run b || echo no; ls | wc"),
            ["npm run a", "npm run b", "echo no", "ls", "wc"]
        );
        assert_eq!(p("echo 'a && b' \"c | d\""), ["echo a && b c | d"]);
        assert_eq!(p("ng build 2>&1 | tail"), ["ng build 2>&1", "tail"]);
        assert_eq!(p("ng serve & ng test"), ["ng serve", "ng test"]);
        assert_eq!(p("a\nb"), ["a", "b"]);
        assert_eq!(p(" ;; && "), Vec::<String>::new());
    }

    #[test]
    fn strips_assignments_and_env_launchers() {
        assert_eq!(p("CI=1 NODE_ENV=test ng test"), ["ng test"]);
        assert_eq!(p("cross-env FOO=1 playwright test"), ["playwright test"]);
        assert_eq!(p("env A=b vitest"), ["vitest"]);
        assert_eq!(p("a=b"), Vec::<String>::new());
    }

    #[test]
    fn parses_scripts_and_ignores_the_rest() {
        let s = Scripts::parse(r#"{"name":"x","scripts":{"a":"ng test","b":3}}"#).unwrap();
        assert_eq!(s.body("a"), Some("ng test"));
        assert_eq!(s.body("b"), None);
        assert!(Scripts::parse("{").is_none());
        assert!(Scripts::parse(r#"{"name":"x"}"#).is_none());
        assert!(Scripts::parse(r#"{"scripts":[]}"#).is_none());
    }

    struct Fake;

    impl ScriptSource for Fake {
        fn package_json(&self, dir: &Path) -> Option<String> {
            match dir.to_str()? {
                "/r/app" => Some(r#"{"scripts":{"x":"a"}}"#.to_owned()),
                "/r" | "/bad" => Some(r#"{"scripts":{"y":"b"}}"#.to_owned()),
                "/bad/sub" => Some("not json".to_owned()),
                "/above" => Some(r#"{"scripts":{"z":"c"}}"#.to_owned()),
                _ => None,
            }
        }

        fn is_repo_root(&self, dir: &Path) -> bool {
            dir == Path::new("/r") || dir == Path::new("/above/repo")
        }
    }

    #[test]
    fn finds_the_nearest_package_json_and_stops_at_the_git_root() {
        let near = Scripts::find(&Fake, Path::new("/r/app/src")).unwrap();
        assert_eq!(near.body("x"), Some("a"));
        assert_eq!(near.body("y"), None, "the nearest file wins");
        let root = Scripts::find(&Fake, Path::new("/r/lib")).unwrap();
        assert_eq!(root.body("y"), Some("b"));
        // An invalid nearest file is not skipped.
        assert!(Scripts::find(&Fake, Path::new("/bad/sub")).is_none());
        // The repository root ends the walk before an outer package.json.
        assert!(Scripts::find(&Fake, Path::new("/above/repo/inner")).is_none());
        assert!(Scripts::find(&NoScripts, Path::new("/r")).is_none());
    }

    #[test]
    fn the_real_source_reads_small_files_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(FsScripts.package_json(dir.path()).is_none());
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{}}"#).unwrap();
        assert!(FsScripts.package_json(dir.path()).is_some());
        let big = " ".repeat(usize::try_from(MAX_PACKAGE_JSON).unwrap() + 1);
        std::fs::write(dir.path().join("package.json"), big).unwrap();
        assert!(FsScripts.package_json(dir.path()).is_none());
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(FsScripts.is_repo_root(dir.path()));
    }
}
