//! The agent client's permission settings, as far as the guard needs them.
//!
//! Claude Code evaluates deny and ask rules against the command a hook
//! returns, so a rewrite could hide a rule written for the original. The
//! guard therefore reads the `Bash` rules itself and decides between `allow`,
//! `ask` and no rewrite (design `command-control.md` section 4.6). Deny and
//! ask rules are matched liberally (any doubt counts as a match), allow
//! rules strictly (any doubt counts as not allowed).

use std::path::{Path, PathBuf};

use sluice_gate_core::config::EnvMap;

use super::lex::Segment;

/// System-wide managed settings of Claude Code.
pub const MANAGED_SETTINGS: &str = "/etc/claude-code/managed-settings.json";

/// One `Bash` permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// `Bash`, `Bash(*)`: every command.
    Any,
    /// `Bash(npm run build)`: exactly this command.
    Exact(String),
    /// `Bash(npm run test:*)`: this command, optionally followed by arguments.
    Prefix(String),
    /// `Bash(git * main)`: `*` matches any text.
    Glob(String),
    /// Something that names `Bash` but has a shape the guard does not know.
    Unknown,
}

/// The `Bash` rules of every settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    /// `permissions.allow`
    pub allow: Vec<Rule>,
    /// `permissions.ask`
    pub ask: Vec<Rule>,
    /// `permissions.deny`
    pub deny: Vec<Rule>,
}

/// What the guard answers for a rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `allow` with the rewritten input.
    Allow,
    /// `ask` with the rewritten input (the user sees it in the prompt).
    Ask,
    /// No output: the original goes through the normal flow.
    Nothing,
}

/// Parses one permission string; `None` for rules of other tools.
pub fn parse_rule(text: &str) -> Option<Rule> {
    if text == "Bash" {
        return Some(Rule::Any);
    }
    if !text.starts_with("Bash") {
        return None;
    }
    let Some(inner) = text.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')) else {
        return Some(Rule::Unknown);
    };
    let inner = inner.trim();
    Some(if inner.is_empty() {
        Rule::Unknown
    } else if inner == "*" || inner == ":*" {
        Rule::Any
    } else if let Some(prefix) = inner.strip_suffix(":*") {
        Rule::Prefix(prefix.to_owned())
    } else if inner.contains('*') {
        Rule::Glob(inner.to_owned())
    } else {
        Rule::Exact(inner.to_owned())
    })
}

/// Adds the rules of one settings JSON text.
///
/// # Errors
/// When the text is not JSON or `permissions` has an unexpected shape.
pub fn add_settings(rules: &mut Rules, text: &str) -> Result<(), ()> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| ())?;
    let Some(root) = value.as_object() else {
        return Err(());
    };
    let Some(perms) = root.get("permissions") else {
        return Ok(());
    };
    let perms = perms.as_object().ok_or(())?;
    for (key, list) in [
        ("allow", &mut rules.allow),
        ("ask", &mut rules.ask),
        ("deny", &mut rules.deny),
    ] {
        let Some(entries) = perms.get(key) else {
            continue;
        };
        for entry in entries.as_array().ok_or(())? {
            if let Some(rule) = parse_rule(entry.as_str().ok_or(())?) {
                list.push(rule);
            }
        }
    }
    Ok(())
}

/// The settings files Claude Code reads, in no particular order: user
/// (`$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`),
/// project and local (`$CLAUDE_PROJECT_DIR/.claude/settings{,.local}.json`)
/// and managed. `None` when the user or project location is unknown.
pub fn settings_paths(env: &EnvMap, managed: &Path) -> Option<Vec<PathBuf>> {
    let user = match env.get("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env.get("HOME").filter(|v| !v.is_empty())?).join(".claude"),
    };
    let project =
        PathBuf::from(env.get("CLAUDE_PROJECT_DIR").filter(|v| !v.is_empty())?).join(".claude");
    Some(vec![
        user.join("settings.json"),
        project.join("settings.json"),
        project.join("settings.local.json"),
        managed.to_path_buf(),
    ])
}

/// Reads the rules of every existing file; missing files are skipped.
///
/// # Errors
/// When a file exists but cannot be read or parsed.
pub fn load(paths: &[PathBuf]) -> Result<Rules, ()> {
    let mut rules = Rules::default();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(text) => add_settings(&mut rules, &text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    Ok(rules)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `*` matches any text (including none); everything else literally.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().expect("split yields one part");
    let Some(mut tail) = text.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return tail.is_empty();
    };
    for part in middle {
        match tail.find(part) {
            Some(i) => tail = &tail[i + part.len()..],
            None => return false,
        }
    }
    tail.ends_with(last)
}

/// Liberal match: does `rule` possibly apply to a command starting like
/// `text` (already whitespace-normalised)?
fn matches_liberally(rule: &Rule, text: &str) -> bool {
    match rule {
        Rule::Any | Rule::Unknown => true,
        Rule::Exact(e) | Rule::Prefix(e) => text.starts_with(&normalize(e)),
        Rule::Glob(g) => {
            let g = normalize(g);
            glob_match(&format!("{g}*"), text)
                || g.strip_suffix(" *")
                    .is_some_and(|head| glob_match(&format!("{head}*"), text))
        }
    }
}

/// Strict match: does `rule` certainly allow exactly `text`?
fn allows(rule: &Rule, text: &str) -> bool {
    match rule {
        Rule::Any => true,
        Rule::Unknown => false,
        Rule::Exact(e) => text == e,
        Rule::Prefix(p) => {
            text == p
                || text
                    .strip_prefix(p.as_str())
                    .is_some_and(|r| r.starts_with(' '))
        }
        Rule::Glob(g) => glob_match(g, text),
    }
}

/// Texts a deny or ask rule is compared with: the whole command and, per
/// segment, its text, its text from the command word on and its unquoted
/// words; for the original and for the rewrite.
fn candidates(command: &str, segments: &[Segment]) -> Vec<String> {
    let mut out = vec![normalize(command)];
    for seg in segments {
        out.push(normalize(&command[seg.start..seg.end]));
        if let Some(first) = seg.command_words().first() {
            out.push(normalize(&command[first.start..seg.end]));
            let words: Vec<&str> = seg
                .command_words()
                .iter()
                .map(|w| w.value.as_str())
                .collect();
            out.push(normalize(&words.join(" ")));
            // From each later word on as well (a rule on the command behind
            // `time`, or behind the inserted prefix).
            for w in &seg.command_words()[1..] {
                out.push(normalize(&command[w.start..seg.end]));
            }
        }
    }
    out
}

/// Every segment of `command` is allowed by an allow rule (strictly).
fn covered(rules: &Rules, command: &str, segments: &[Segment]) -> bool {
    !segments.is_empty()
        && segments.iter().all(|seg| {
            let text = command[seg.start..seg.end].trim();
            seg.redirections == 0 && rules.allow.iter().any(|r| allows(r, text))
        })
}

/// The decision table of section 4.6.
pub fn decide(
    rules: &Rules,
    permission_mode: Option<&str>,
    original: (&str, &[Segment]),
    rewritten: (&str, &[Segment]),
) -> Decision {
    let texts: Vec<String> = candidates(original.0, original.1)
        .into_iter()
        .chain(candidates(rewritten.0, rewritten.1))
        .collect();
    let restricted = rules
        .deny
        .iter()
        .chain(&rules.ask)
        .any(|rule| texts.iter().any(|t| matches_liberally(rule, t)));
    if restricted {
        return Decision::Nothing;
    }
    let allowed = covered(rules, original.0, original.1);
    match permission_mode.unwrap_or("default") {
        "bypassPermissions" => Decision::Allow,
        "plan" | "auto" | "dontAsk" => {
            if allowed {
                Decision::Allow
            } else {
                Decision::Nothing
            }
        }
        "default" | "acceptEdits" => {
            if allowed {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        _ => Decision::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::lex::split;

    #[test]
    fn rule_shapes() {
        assert_eq!(parse_rule("Bash"), Some(Rule::Any));
        assert_eq!(parse_rule("Bash(*)"), Some(Rule::Any));
        assert_eq!(
            parse_rule("Bash(npm run test:*)"),
            Some(Rule::Prefix("npm run test".into()))
        );
        assert_eq!(
            parse_rule("Bash(git * main)"),
            Some(Rule::Glob("git * main".into()))
        );
        assert_eq!(parse_rule("Bash(make)"), Some(Rule::Exact("make".into())));
        assert_eq!(parse_rule("Bash()"), Some(Rule::Unknown));
        assert_eq!(parse_rule("Bash(make"), Some(Rule::Unknown));
        assert_eq!(parse_rule("Bashful"), Some(Rule::Unknown));
        assert_eq!(parse_rule("Read(./x)"), None);
        assert_eq!(parse_rule("mcp__sluice__verify"), None);
    }

    #[test]
    fn settings_parsing() {
        let mut r = Rules::default();
        add_settings(
            &mut r,
            r#"{"permissions":{"allow":["Bash(make:*)","Read"],"deny":["Bash(rm:*)"]},"env":{}}"#,
        )
        .unwrap();
        assert_eq!(r.allow, [Rule::Prefix("make".into())]);
        assert_eq!(r.deny, [Rule::Prefix("rm".into())]);
        assert_eq!(r.ask.len(), 0);
        add_settings(&mut r, "{}").unwrap();
        for bad in [
            "",
            "not json",
            "[]",
            r#"{"permissions":[]}"#,
            r#"{"permissions":{"allow":"Bash"}}"#,
            r#"{"permissions":{"deny":[1]}}"#,
        ] {
            assert!(add_settings(&mut Rules::default(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths() {
        let env: EnvMap = [
            ("HOME".to_owned(), "/h".to_owned()),
            ("CLAUDE_PROJECT_DIR".to_owned(), "/p".to_owned()),
        ]
        .into();
        let got = settings_paths(&env, Path::new("/m.json")).unwrap();
        assert_eq!(
            got,
            [
                PathBuf::from("/h/.claude/settings.json"),
                PathBuf::from("/p/.claude/settings.json"),
                PathBuf::from("/p/.claude/settings.local.json"),
                PathBuf::from("/m.json"),
            ]
        );
        let mut with_dir = env.clone();
        with_dir.insert("CLAUDE_CONFIG_DIR".into(), "/c".into());
        assert_eq!(
            settings_paths(&with_dir, Path::new("/m.json")).unwrap()[0],
            PathBuf::from("/c/settings.json")
        );
        let mut no_project = env;
        no_project.remove("CLAUDE_PROJECT_DIR");
        assert_eq!(settings_paths(&no_project, Path::new("/m.json")), None);
    }

    #[test]
    fn globs() {
        assert!(glob_match("git * main", "git push origin main"));
        assert!(!glob_match("git * main", "git push origin main2"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(glob_match("make", "make"));
        assert!(!glob_match("make", "make x"));
    }

    #[test]
    fn strict_and_liberal() {
        assert!(allows(&Rule::Prefix("make".into()), "make"));
        assert!(allows(&Rule::Prefix("make".into()), "make install"));
        assert!(!allows(&Rule::Prefix("make".into()), "makefoo"));
        assert!(!allows(&Rule::Exact("make".into()), "make install"));
        assert!(!allows(&Rule::Glob("ls *".into()), "ls"));
        assert!(!allows(&Rule::Unknown, "ls"));
        assert!(matches_liberally(&Rule::Glob("ls *".into()), "ls"));
        assert!(matches_liberally(
            &Rule::Exact("make".into()),
            "make deploy"
        ));
        assert!(matches_liberally(
            &Rule::Prefix("make  deploy".into()),
            "make deploy x"
        ));
        assert!(matches_liberally(&Rule::Unknown, "anything"));
        assert!(!matches_liberally(&Rule::Prefix("rm".into()), "make"));
    }

    fn decide_for(rules: &Rules, mode: Option<&str>, original: &str, rewritten: &str) -> Decision {
        let o = split(original).unwrap();
        let r = split(rewritten).unwrap();
        decide(rules, mode, (original, &o), (rewritten, &r))
    }

    fn rules(allow: &[&str], ask: &[&str], deny: &[&str]) -> Rules {
        let conv = |l: &[&str]| l.iter().filter_map(|s| parse_rule(s)).collect();
        Rules {
            allow: conv(allow),
            ask: conv(ask),
            deny: conv(deny),
        }
    }

    /// Every row of the table in section 4.6.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn permission_table() {
        let o = "make deploy && make";
        let w =
            "sluice exec --label make-deploy -- make deploy && sluice exec --label make -- make";
        let none = Rules::default();
        for (label, r, mode, want) in [
            (
                "deny rule",
                rules(&[], &[], &["Bash(make deploy:*)"]),
                Some("bypassPermissions"),
                Decision::Nothing,
            ),
            (
                "ask rule",
                rules(&[], &["Bash(make deploy)"], &[]),
                Some("default"),
                Decision::Nothing,
            ),
            (
                "bare Bash deny",
                rules(&["Bash"], &[], &["Bash"]),
                Some("bypassPermissions"),
                Decision::Nothing,
            ),
            (
                "bare Bash ask",
                rules(&[], &["Bash"], &[]),
                Some("default"),
                Decision::Nothing,
            ),
            (
                "ask rule on sluice",
                rules(&[], &["Bash(sluice exec:*)"], &[]),
                Some("default"),
                Decision::Nothing,
            ),
            (
                "unknown deny shape",
                rules(&[], &[], &["Bash(make"]),
                Some("default"),
                Decision::Nothing,
            ),
            (
                "plan, not allowed",
                none.clone(),
                Some("plan"),
                Decision::Nothing,
            ),
            (
                "auto, not allowed",
                none.clone(),
                Some("auto"),
                Decision::Nothing,
            ),
            (
                "dontAsk, not allowed",
                none.clone(),
                Some("dontAsk"),
                Decision::Nothing,
            ),
            (
                "plan, allowed",
                rules(&["Bash(make:*)"], &[], &[]),
                Some("plan"),
                Decision::Allow,
            ),
            (
                "dontAsk, allowed",
                rules(&["Bash(make:*)"], &[], &[]),
                Some("dontAsk"),
                Decision::Allow,
            ),
            (
                "bypass",
                none.clone(),
                Some("bypassPermissions"),
                Decision::Allow,
            ),
            (
                "default, allowed",
                rules(&["Bash(make:*)"], &[], &[]),
                Some("default"),
                Decision::Allow,
            ),
            (
                "default, bare Bash allow",
                rules(&["Bash"], &[], &[]),
                Some("default"),
                Decision::Allow,
            ),
            (
                "default, partly allowed",
                rules(&["Bash(make deploy)"], &[], &[]),
                Some("default"),
                Decision::Ask,
            ),
            ("default", none.clone(), Some("default"), Decision::Ask),
            (
                "acceptEdits",
                none.clone(),
                Some("acceptEdits"),
                Decision::Ask,
            ),
            ("absent mode", none.clone(), None, Decision::Ask),
            (
                "unknown mode",
                none.clone(),
                Some("future"),
                Decision::Nothing,
            ),
            (
                "other tools only",
                rules(&["Read"], &["Edit"], &["Write"]),
                Some("default"),
                Decision::Ask,
            ),
            (
                "unrelated deny",
                rules(&[], &[], &["Bash(rm:*)"]),
                Some("bypassPermissions"),
                Decision::Allow,
            ),
        ] {
            assert_eq!(decide_for(&r, mode, o, w), want, "{label}");
        }
        // Redirections never count as allowed; an exact rule needs the exact text.
        let r = rules(&["Bash(cargo build:*)"], &[], &[]);
        assert_eq!(
            decide_for(
                &r,
                Some("default"),
                "cargo build 2>&1",
                "sluice exec --label cargo-build -- cargo build 2>&1"
            ),
            Decision::Ask
        );
        assert_eq!(
            decide_for(
                &r,
                Some("default"),
                "cargo build --release",
                "sluice exec --label cargo-build -- cargo build --release"
            ),
            Decision::Allow
        );
    }

    #[test]
    fn load_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        std::fs::write(&a, r#"{"permissions":{"ask":["Bash(make:*)"]}}"#).unwrap();
        let missing = dir.path().join("missing.json");
        let r = load(&[a.clone(), missing]).unwrap();
        assert_eq!(r.ask, [Rule::Prefix("make".into())]);
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{").unwrap();
        assert!(load(&[a, bad]).is_err());
        assert!(
            load(&[dir.path().to_path_buf()]).is_err(),
            "a directory cannot be read"
        );
    }
}
