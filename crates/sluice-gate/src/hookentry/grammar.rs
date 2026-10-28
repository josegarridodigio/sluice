//! The conservative line grammar of husky and plain hook files.
//!
//! A file is understood only when every physical line is one of: blank, a
//! comment, a first-line shebang for `sh` or `bash`, husky's bootstrap line
//! (`. "$(dirname -- "$0")/_/husky.sh"`), `set -e` before the first command,
//! a trailing `exit 0`, or a list of simple commands joined by `&&`. A simple
//! command is words made of literal text, `'single'` and `"double"` quoted
//! strings without expansions. Anything else (variables, `$(...)`, backticks,
//! globs, redirections, pipes, `||`, `;`, control flow, functions, `cd`,
//! `export`, here-documents, line continuations, trailing comments) makes the
//! whole file "not understood" and it is delegated. Positional parameters
//! (`$1`, `"$@"`) are rejected too: the hook's arguments are not part of a
//! unit's identity yet.

/// One simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// The words as written, joined by single spaces (what `sh -c` runs).
    pub text: String,
    /// The words after quote removal.
    pub argv: Vec<String>,
}

/// An understood hook file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Script {
    /// `set -e` (or `#!/bin/sh -e`) precedes the first command.
    pub errexit: bool,
    /// One `&&` list per line, in file order.
    pub lists: Vec<Vec<Cmd>>,
    /// The last line is `exit 0`.
    pub exit_ok: bool,
    /// A husky bootstrap line sources `_/husky.sh`.
    pub sources_husky: bool,
}

/// Words that are shell syntax or builtins, never run as a unit.
const SHELL_WORDS: &[&str] = &[
    "!", "{", "}", ".", ":", "[", "[[", "alias", "bg", "break", "case", "cd", "command",
    "continue", "do", "done", "echo", "elif", "else", "esac", "eval", "exec", "exit", "export",
    "false", "fc", "fg", "fi", "for", "function", "getopts", "hash", "if", "in", "jobs", "kill",
    "local", "printf", "pwd", "read", "readonly", "return", "set", "shift", "source", "test",
    "then", "time", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias",
    "unset", "until", "wait", "while", "declare", "let",
];

/// Characters a bare (unquoted) word may contain.
fn bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
}

/// Husky's v4 to v8 bootstrap line, in its two spellings.
fn is_husky_source(line: &str) -> bool {
    line == r#". "$(dirname -- "$0")/_/husky.sh""# || line == r#". "$(dirname "$0")/_/husky.sh""#
}

/// Checks a shebang; returns whether it asks for errexit (`#!/bin/sh -e`).
fn shebang(line: &str) -> Result<bool, String> {
    let mut words = line.trim_start_matches("#!").split_whitespace();
    let program = words.next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or_default();
    let rest: Vec<&str> = words.collect();
    match (name, rest.as_slice()) {
        ("sh" | "bash", ["-e"]) => Ok(true),
        ("sh" | "bash", []) | ("env", ["sh" | "bash"]) => Ok(false),
        _ => Err(format!("the shebang `{line}` is not plain sh or bash")),
    }
}

/// Why `$` cannot be accepted, naming the case that matters most.
fn dollar_reason(next: Option<char>) -> String {
    match next {
        Some(c) if c.is_ascii_digit() || c == '@' || c == '*' => {
            "uses the hook's arguments ($1, \"$@\")".to_owned()
        }
        _ => "uses a variable or a substitution ($)".to_owned(),
    }
}

/// A word being read: its text as written and after quote removal.
#[derive(Default)]
struct Word {
    raw: String,
    value: String,
}

/// Splits a line into words and `&&` operators (`None`).
fn tokenize(line: &str) -> Result<Vec<Option<Word>>, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' || chars[i] == '\t' {
            i += 1;
            continue;
        }
        if chars[i] == '&'
            && chars.get(i + 1) == Some(&'&')
            && chars.get(i + 2).is_none_or(|c| *c == ' ' || *c == '\t')
        {
            tokens.push(None);
            i += 2;
            continue;
        }
        let (word, next) = read_word(&chars, i)?;
        tokens.push(Some(word));
        i = next;
    }
    Ok(tokens)
}

/// Reads one word starting at `start`; returns it and the index after it.
fn read_word(chars: &[char], start: usize) -> Result<(Word, usize), String> {
    let mut word = Word::default();
    let mut i = start;
    while i < chars.len() && chars[i] != ' ' && chars[i] != '\t' {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                let close = chars[i + 1..]
                    .iter()
                    .position(|x| *x == c)
                    .ok_or("has an unterminated quote")?;
                let inner: String = chars[i + 1..i + 1 + close].iter().collect();
                check_quoted(&inner, c)?;
                word.raw.push(c);
                word.raw.push_str(&inner);
                word.raw.push(c);
                word.value.push_str(&inner);
                i += close + 2;
            }
            '$' => return Err(dollar_reason(chars.get(i + 1).copied())),
            c if bare(c) => {
                word.raw.push(c);
                word.value.push(c);
                i += 1;
            }
            c => return Err(format!("uses the shell syntax {c:?}")),
        }
    }
    Ok((word, i))
}

/// A double-quoted string must have no expansion; single quotes are literal.
fn check_quoted(inner: &str, quote: char) -> Result<(), String> {
    if quote == '"' {
        if let Some(pos) = inner.find('$') {
            return Err(dollar_reason(inner[pos + 1..].chars().next()));
        }
        if inner.contains(['`', '\\', '!']) {
            return Err("has an expansion or an escape inside double quotes".to_owned());
        }
    }
    Ok(())
}

/// True for `NAME=value` first words (an environment assignment).
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// One element of an `&&` list.
fn command(words: &[Word]) -> Result<Cmd, String> {
    let first = words.first().ok_or("has an empty command around &&")?;
    if is_assignment(&first.raw) {
        return Err("sets a variable for a command".to_owned());
    }
    if SHELL_WORDS.contains(&first.value.as_str()) {
        return Err(format!("uses the shell word or builtin `{}`", first.value));
    }
    Ok(Cmd {
        text: words
            .iter()
            .map(|w| w.raw.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        argv: words.iter().map(|w| w.value.clone()).collect(),
    })
}

/// Parses one line into an `&&` list.
fn parse_list(line: &str) -> Result<Vec<Cmd>, String> {
    let mut list = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    for token in tokenize(line)? {
        match token {
            Some(word) => words.push(word),
            None => list.push(command(&std::mem::take(&mut words))?),
        }
    }
    list.push(command(&words)?);
    Ok(list)
}

/// Parses a hook file.
///
/// # Errors
///
/// Why the file is not understood (the reason shown by `hooks status` and
/// recorded when the hook is delegated).
pub fn parse(text: &str) -> Result<Script, String> {
    let mut script = Script::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if script.exit_ok && !line.is_empty() && !line.starts_with('#') {
            return Err("has commands after `exit 0`".to_owned());
        }
        if n == 0 && line.starts_with("#!") {
            script.errexit = shebang(line)?;
        } else if line.is_empty() || line.starts_with('#') {
            // Blank lines and comments do nothing.
        } else if is_husky_source(line) {
            script.sources_husky = true;
        } else if line == "set -e" && script.lists.is_empty() {
            script.errexit = true;
        } else if line == "exit 0" {
            script.exit_ok = true;
        } else {
            script
                .lists
                .push(parse_list(line).map_err(|e| format!("line {}: {e}", n + 1))?);
        }
    }
    if script.lists.is_empty() {
        return Err("has no command to run".to_owned());
    }
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(script: &Script) -> Vec<Vec<&str>> {
        script
            .lists
            .iter()
            .map(|l| l.iter().map(|c| c.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn real_world_shapes_are_understood() {
        let accepted = [
            "npm test\n",
            "#!/usr/bin/env sh\n. \"$(dirname -- \"$0\")/_/husky.sh\"\n\nnpx lint-staged\n",
            "npm run typecheck\nnpm run lint\npnpm run test\n",
            "pnpm pretty-quick --staged --ignore-path .prettierignore\npnpm audit\n",
            "npx nano-staged\nnode scripts/generate-exports.js --check\n",
            "npm run test && npm run build\n",
            "git add .\n",
            "pnpm install --frozen-lockfile --silent\n",
            "npx prettier --write 'src/**/*.ts'\n",
            "npm run lint -- --max-warnings=0\n",
            "# comment\nnpx lint-staged\nexit 0\n",
            "node \"scripts/a b.js\" --msg=\"two words\"\n",
            "'./node_modules/.bin/eslint' .\n",
        ];
        for text in accepted {
            assert!(parse(text).is_ok(), "{text:?}: {:?}", parse(text));
        }
    }

    #[test]
    fn real_world_shapes_that_need_a_shell_are_rejected() {
        let rejected = [
            "if ! command -v go; then\n  exit 1\nfi\n",
            "branch=$(git rev-parse --abbrev-ref HEAD)\n",
            "npm run typecheck || {\n  exit 1\n}\n",
            "while read local_ref local_sha remote_ref remote_sha\ndo\n  echo\ndone\n",
            "git diff --cached --name-only | xargs prettier --write\n",
            "npx prettier $(git diff --cached --name-only) --write\n",
            "npx commitlint --edit $1\n",
            "npx --no -- commitlint --edit \"$@\"\n",
            "npm test > /tmp/out\n",
            "npm test; npm run lint\n",
            "npm test &\n",
            "FOO=1 npm test\n",
            "unset GIT_DIR\nnpm test\n",
            "export CI=1\nnpm test\n",
            "cd frontend\nnpm test\n",
            "set -eu\nnpm test\n",
            "npm test\nset -e\n",
            "npm run a \\\n  && npm run b\n",
            "echo hello\n",
            "git diff --quiet -- **/*.ts\n",
            "npm test # why\n",
            "npm run `echo lint`\n",
            "npm run \"$X\"\n",
            "#!/usr/bin/env node\nconsole.log(1)\n",
            "#!/bin/zsh\nnpm test\n",
            "npm test\r\n\r\nnpm run lint &&\n",
            ". .husky/common.sh\nnpm test\n",
            "npm test\nexit 1\n",
            "exit 0\nnpm test\n",
            "npm run a &&& npm run b\n",
            "\n# only comments\n",
            "cat <<EOF\nx\nEOF\n",
            "foo() {\n  npm test\n}\nfoo\n",
        ];
        for text in rejected {
            assert!(parse(text).is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn lists_keep_the_words_as_written_and_after_quote_removal() {
        let s =
            parse("#!/bin/sh\nset -e\nnpm run a && node 'x y.js' \"--z=1 2\"\nnpm test\nexit 0\n")
                .unwrap();
        assert!(s.errexit && s.exit_ok && !s.sources_husky);
        assert_eq!(
            texts(&s),
            vec![
                vec!["npm run a", "node 'x y.js' \"--z=1 2\""],
                vec!["npm test"]
            ]
        );
        assert_eq!(s.lists[0][1].argv, ["node", "x y.js", "--z=1 2"]);
    }

    #[test]
    fn the_husky_bootstrap_is_recognised_and_marked() {
        let s = parse(". \"$(dirname \"$0\")/_/husky.sh\"\nnpm test\n").unwrap();
        assert!(s.sources_husky);
        assert_eq!(s.lists.len(), 1);
    }

    #[test]
    fn the_reason_names_the_arguments_case() {
        let e = parse("npx commitlint --edit $1\n").unwrap_err();
        assert!(e.contains("arguments"), "{e}");
        let e = parse("npm run \"$X\"\n").unwrap_err();
        assert!(e.contains("variable"), "{e}");
    }
}
