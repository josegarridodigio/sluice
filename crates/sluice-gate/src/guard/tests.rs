//! Table-driven tests of the whole guard (`respond`) and the injection suite.
//! Plan: `kb/sluice/design/command-control.md` section 7, A2.

#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sluice_gate_core::config::EnvMap;
use sluice_gate_core::heavy::HeavyMatcher;

use super::lex::{self, Segment};
use super::matcher::{self, Edit, Gates, World};
use super::{Fs, Inputs, respond};

const REPO_TOML: &str = r#"version = 1

[gates.test]
cmd = "cargo test --workspace"
hooks = ["pre-commit"]

[gates.lint]
cmd = "cargo   clippy --workspace -- -D warnings"

[gates.web]
cmd = 'npm run  check'

[gates.build]
cmd = "npm run build"
"#;

/// A scratch layout: `repo/` (with `.sluice.toml`, `sub/`, `web/`),
/// `plain/` (no config, `web/`), `home/` (HOME, Claude user settings, the
/// Sluice user config path) and a managed-settings path.
struct Fx {
    root: tempfile::TempDir,
}

/// What a fixture expects.
#[derive(Debug, Clone, Copy)]
enum Want {
    /// `allow` with this rewritten command.
    Allow(&'static str),
    /// `ask` with this rewritten command.
    Ask(&'static str),
    /// `deny` whose reason contains this text.
    Deny(&'static str),
    /// No output.
    Silent,
}

impl Fx {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        for d in [
            "repo/sub",
            "repo/web",
            "repo/.claude",
            "plain/web",
            "home/.claude",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::write(p.join("repo/.sluice.toml"), REPO_TOML).unwrap();
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        fs::write(self.path(rel), text).unwrap();
    }

    fn env(&self, extra: &[(&str, &str)]) -> EnvMap {
        let mut env: EnvMap = [
            ("HOME", self.path("home")),
            ("CLAUDE_PROJECT_DIR", self.path("repo")),
            ("SLUICE_CONFIG", self.path("home/sluice.toml")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.display().to_string()))
        .collect();
        for (k, v) in extra {
            if v.is_empty() {
                env.remove(*k);
            } else {
                env.insert((*k).to_owned(), (*v).to_owned());
            }
        }
        env
    }

    fn respond_raw(&self, input: &str, extra: &[(&str, &str)]) -> Option<String> {
        let env = self.env(extra);
        let managed = self.path("managed-settings.json");
        respond(
            input,
            &Inputs {
                env: &env,
                managed_settings: &managed,
                world: &Fs,
            },
        )
    }

    fn input(&self, cwd: &str, command: &str, mode: Option<&str>) -> String {
        let mut v = json!({
            "session_id": "s",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "cwd": self.path(cwd),
            "tool_input": { "command": command, "description": "run it" },
        });
        if let Some(mode) = mode {
            v["permission_mode"] = json!(mode);
        }
        v.to_string()
    }

    fn check(
        &self,
        label: &str,
        cwd: &str,
        command: &str,
        mode: Option<&str>,
        extra: &[(&str, &str)],
        want: Want,
    ) {
        let out = self.respond_raw(&self.input(cwd, command, mode), extra);
        let parsed: Option<Value> = out.as_deref().map(|o| serde_json::from_str(o).unwrap());
        let hso = parsed.as_ref().map(|v| &v["hookSpecificOutput"]);
        let decision = hso.and_then(|h| h["permissionDecision"].as_str());
        let cmd = hso.and_then(|h| h["updatedInput"]["command"].as_str());
        let reason = hso
            .and_then(|h| h["permissionDecisionReason"].as_str())
            .unwrap_or("");
        let ok = match want {
            Want::Allow(w) => decision == Some("allow") && cmd == Some(w),
            Want::Ask(w) => decision == Some("ask") && cmd == Some(w),
            Want::Deny(w) => decision == Some("deny") && reason.contains(w) && cmd.is_none(),
            Want::Silent => out.is_none(),
        };
        assert!(ok, "{label}: {command:?} want {want:?}, got {out:?}");
        if let Some(h) = hso.filter(|_| matches!(want, Want::Allow(_) | Want::Ask(_))) {
            assert_eq!(h["hookEventName"], "PreToolUse");
            let note = h["additionalContext"].as_str().unwrap();
            assert!(note.contains(cmd.unwrap()), "{label}: note {note}");
            assert!(!note.contains('\n'), "{label}: one line");
            assert_eq!(h["updatedInput"]["description"], "run it");
        }
    }
}

const BYPASS: Option<&str> = Some("bypassPermissions");

/// The zero-config classification of the Angular incident: no user config,
/// a `package.json` whose scripts run heavy and light commands.
#[test]
fn angular_and_package_scripts_are_classified_without_config() {
    let fx = Fx::new();
    fx.write(
        "plain/package.json",
        r#"{"scripts":{"test":"ng test","axe":"npm run axe:a","axe:a":"playwright test",
            "start":"ng serve","lint":"eslint ."}}"#,
    );
    for cwd in ["plain", "plain/web"] {
        for (cmd, want) in [
            (
                "npx ng test --watch=false",
                "sluice exec --label ng-test -- npx ng test --watch=false",
            ),
            (
                "npx ng build -c x",
                "sluice exec --label ng-build -- npx ng build -c x",
            ),
            ("npm run axe", "sluice exec --label run-axe -- npm run axe"),
            ("npm test", "sluice exec --label run-test -- npm test"),
            (
                "pnpm exec playwright test",
                "sluice exec --label playwright-test -- pnpm exec playwright test",
            ),
            (
                "cd web && npm run axe | tail -3",
                "cd web && sluice exec --label run-axe -- npm run axe | tail -3",
            ),
        ] {
            if cwd == "plain/web" && cmd.starts_with("cd web") {
                continue;
            }
            fx.check("zero config", cwd, cmd, BYPASS, &[], Want::Allow(want));
        }
        for cmd in [
            "npm start",
            "npx ng serve",
            "ng test --watch",
            "npm run lint",
            "npm run missing",
            "npm install",
        ] {
            fx.check("not heavy", cwd, cmd, BYPASS, &[], Want::Silent);
        }
    }
    // The script is read from the directory the command runs in.
    fx.write(
        "plain/web/package.json",
        r#"{"scripts":{"test":"echo ok"}}"#,
    );
    fx.check(
        "nearest",
        "plain/web",
        "npm test",
        BYPASS,
        &[],
        Want::Silent,
    );
    fx.write("plain/web/package.json", "{ not json");
    fx.check(
        "invalid",
        "plain/web",
        "npm test",
        BYPASS,
        &[],
        Want::Silent,
    );
}

/// Rewrites of heavy commands (no `.sluice.toml` applies).
#[test]
fn heavy_rewrites() {
    let fx = Fx::new();
    for (cmd, want) in [
        ("make", "sluice exec --label make -- make"),
        ("make -j8", "sluice exec --label make -- make -j8"),
        ("  make  ", "  sluice exec --label make -- make  "),
        (
            "cargo build --release 2>&1 | tail -20",
            "sluice exec --label cargo-build -- cargo build --release 2>&1 | tail -20",
        ),
        (
            "cargo test -p x",
            "sluice exec --label cargo-test -- cargo test -p x",
        ),
        (
            "cargo clippy",
            "sluice exec --label cargo-clippy -- cargo clippy",
        ),
        (
            "cargo nextest run",
            "sluice exec --label cargo-nextest -- cargo nextest run",
        ),
        (
            "go build ./...",
            "sluice exec --label go-build -- go build ./...",
        ),
        (
            "go test ./...",
            "sluice exec --label go-test -- go test ./...",
        ),
        (
            "docker build -t \"my app\" .",
            "sluice exec --label docker-build -- docker build -t \"my app\" .",
        ),
        (
            "docker buildx build --push .",
            "sluice exec --label docker-buildx -- docker buildx build --push .",
        ),
        (
            "docker compose build web",
            "sluice exec --label docker-compose -- docker compose build web",
        ),
        (
            "docker-compose build",
            "sluice exec --label docker-compose -- docker-compose build",
        ),
        (
            "gradle build",
            "sluice exec --label gradle-build -- gradle build",
        ),
        (
            "./gradlew build",
            "sluice exec --label gradle-build -- ./gradlew build",
        ),
        (
            "mvn package",
            "sluice exec --label mvn-package -- mvn package",
        ),
        (
            "./mvnw package",
            "sluice exec --label mvn-package -- ./mvnw package",
        ),
        ("tsc --noEmit", "sluice exec --label tsc -- tsc --noEmit"),
        ("npx tsc", "sluice exec --label tsc -- npx tsc"),
        ("pnpm exec tsc", "sluice exec --label tsc -- pnpm exec tsc"),
        ("yarn exec tsc", "sluice exec --label tsc -- yarn exec tsc"),
        ("vite build", "sluice exec --label vite-build -- vite build"),
        (
            "webpack --mode production",
            "sluice exec --label webpack -- webpack --mode production",
        ),
        ("next build", "sluice exec --label next-build -- next build"),
        ("pytest -x", "sluice exec --label pytest -- pytest -x"),
        (
            "python -m pytest",
            "sluice exec --label pytest -- python -m pytest",
        ),
        (
            "python3 -m pytest tests",
            "sluice exec --label pytest-tests -- python3 -m pytest tests",
        ),
        ("jest", "sluice exec --label jest -- jest"),
        ("vitest run", "sluice exec --label vitest-run -- vitest run"),
        (
            "npx vitest run",
            "sluice exec --label vitest-run -- npx vitest run",
        ),
        (
            "npx -y vitest",
            "sluice exec --label vitest -- npx -y vitest",
        ),
        (
            "npm run build",
            "sluice exec --label run-build -- npm run build",
        ),
        (
            "npm run-script build",
            "sluice exec --label run-build -- npm run-script build",
        ),
        ("pnpm build", "sluice exec --label run-build -- pnpm build"),
        (
            "pnpm run build",
            "sluice exec --label run-build -- pnpm run build",
        ),
        ("yarn build", "sluice exec --label run-build -- yarn build"),
        (
            "yarn run build",
            "sluice exec --label run-build -- yarn run build",
        ),
        (
            "bun run build",
            "sluice exec --label run-build -- bun run build",
        ),
        (
            "pnpm -C web build",
            "sluice exec --label run-build -- pnpm -C web build",
        ),
        (
            "cd web && pnpm build",
            "cd web && sluice exec --label run-build -- pnpm build",
        ),
        (
            "RUSTFLAGS=-Dwarnings cargo clippy",
            "RUSTFLAGS=-Dwarnings sluice exec --label cargo-clippy -- cargo clippy",
        ),
        (
            "A=1 B='x y' make",
            "A=1 B='x y' sluice exec --label make -- make",
        ),
        (
            "make && make install",
            "sluice exec --label make -- make && sluice exec --label make-install -- make install",
        ),
        (
            "make || make clean",
            "sluice exec --label make -- make || sluice exec --label make-clean -- make clean",
        ),
        (
            "make; make install;",
            "sluice exec --label make -- make; sluice exec --label make-install -- make install;",
        ),
        ("ls && make", "ls && sluice exec --label make -- make"),
        ("time make", "time sluice exec --label make -- make"),
        (
            "make | tee log",
            "sluice exec --label make -- make | tee log",
        ),
        (
            "make > out.txt 2>&1",
            "sluice exec --label make -- make > out.txt 2>&1",
        ),
        ("make 'all'", "sluice exec --label make-all -- make 'all'"),
        ("'make' all", "sluice exec --label make-all -- 'make' all"),
        (
            "make $TARGET",
            "sluice exec --label make-target -- make $TARGET",
        ),
        (
            "cargo build --features \"a b\"",
            "sluice exec --label cargo-build -- cargo build --features \"a b\"",
        ),
        (
            "make && cd web && pnpm build",
            "sluice exec --label make -- make && cd web && sluice exec --label run-build -- pnpm build",
        ),
    ] {
        fx.check("heavy", "plain", cmd, BYPASS, &[], Want::Allow(want));
    }
}

/// Gate rewrites, and the cases that fall through to `exec`.
#[test]
fn gate_rewrites() {
    let fx = Fx::new();
    for (label, cwd, cmd, want) in [
        (
            "gate verbatim",
            "repo",
            "cargo test --workspace",
            "sluice run test -v",
        ),
        (
            "gate with extra",
            "repo",
            "cargo test --workspace -- --nocapture",
            "sluice run test -v -- -- --nocapture",
        ),
        (
            "gate, other spacing",
            "repo",
            "cargo  test   --workspace",
            "sluice run test -v",
        ),
        (
            "gate with extra spaces in config",
            "repo",
            "cargo clippy --workspace -- -D warnings",
            "sluice run lint -v",
        ),
        (
            "single-quoted gate cmd",
            "repo",
            "npm run check",
            "sluice run web -v",
        ),
        (
            "gate extra args",
            "repo",
            "npm run check --silent",
            "sluice run web -v -- --silent",
        ),
        (
            "gate in other spelling",
            "repo",
            "pnpm build",
            "sluice run build -v",
        ),
        (
            "gate in other spelling (yarn)",
            "repo",
            "yarn run build",
            "sluice run build -v",
        ),
        (
            "gate spelled, with extra",
            "repo",
            "npm run build -- --prod",
            "sluice run build -v -- -- --prod",
        ),
        (
            "other spelling with extra -> exec",
            "repo",
            "pnpm build --prod",
            "sluice exec --label run-build -- pnpm build --prod",
        ),
        (
            "runner-only match -> exec",
            "repo",
            "cargo test -p x",
            "sluice exec --label cargo-test -- cargo test -p x",
        ),
        (
            "gate under cd sub -> exec",
            "repo",
            "cd sub && cargo test --workspace",
            "cd sub && sluice exec --label cargo-test -- cargo test --workspace",
        ),
        (
            "cd back to the config dir",
            "repo/sub",
            "cd .. && cargo test --workspace",
            "cd .. && sluice run test -v",
        ),
        (
            "gate from a subdirectory -> exec",
            "repo/sub",
            "cargo test --workspace",
            "sluice exec --label cargo-test -- cargo test --workspace",
        ),
        (
            "env prefix on a gate -> exec",
            "repo",
            "RUST_LOG=x cargo test --workspace",
            "RUST_LOG=x sluice exec --label cargo-test -- cargo test --workspace",
        ),
        (
            "gate first in a pipeline -> exec",
            "repo",
            "cargo test --workspace 2>&1 | tail",
            "sluice exec --label cargo-test -- cargo test --workspace 2>&1 | tail",
        ),
        (
            "gate with redirection -> exec",
            "repo",
            "cargo test --workspace > log",
            "sluice exec --label cargo-test -- cargo test --workspace > log",
        ),
        (
            "time before a gate",
            "repo",
            "time cargo test --workspace",
            "time sluice run test -v",
        ),
        (
            "dir option -> exec",
            "repo",
            "npm --prefix web run build",
            "sluice exec --label run-build -- npm --prefix web run build",
        ),
        (
            "gate and heavy",
            "repo",
            "cargo test --workspace && make",
            "sluice run test -v && sluice exec --label make -- make",
        ),
    ] {
        fx.check(label, cwd, cmd, BYPASS, &[], Want::Allow(want));
    }
}

/// Commands the guard never rewrites.
#[test]
fn refusals() {
    let fx = Fx::new();
    let long = format!("make {}", "x".repeat(4096));
    for cmd in [
        "ls -la",
        "cat x | make",
        "make &",
        "make & wait",
        "make <<EOF",
        "make <<< x",
        "make < in",
        "docker build - < Dockerfile",
        "docker build -",
        "echo $(make)",
        "echo `make`",
        "(make)",
        "{ make; }",
        "make\nmake",
        "make \\\n all",
        "echo 'unbalanced && make",
        "echo \"make\"",
        "echo make",
        "sh -c \"make\"",
        "sh -c make",
        "bash -c make",
        "bash -c \"$(cat run.sh)\"",
        "sudo make",
        "xargs make",
        "env A=1 make",
        "nohup make",
        "nice make",
        "timeout 10 make",
        "exec make",
        "eval make",
        "command make",
        "sluice exec -- make",
        "sluice run test",
        "docker ps",
        "pnpm install",
        "yarn add x",
        "npm install",
        "cargo fmt",
        "makefile",
        "make # comment",
        "cd web; make",
        "cd $HOME && make",
        "cd ~ && make",
        "cd missing && make",
        "cd web || make",
        "cd && make",
        "cd web",
        "make && cd web",
        "pushd web && make",
        "if true; then make; fi",
        "for f in a; do make; done",
        "make |& tee x",
        "time -p make",
        "make ${X}",
        "make \"$X\"",
        &long,
    ] {
        fx.check("refusal", "plain", cmd, BYPASS, &[], Want::Silent);
    }
}

/// Deny decisions (approve, legacy gate denies, suggestion mode).
#[test]
fn denies() {
    let fx = Fx::new();
    let gate_test = "gates=[\"test\"]";
    for (label, cwd, cmd, mode, extra, want) in [
        (
            "approve",
            "repo",
            "sluice queue approve t-1",
            BYPASS,
            &[][..],
            Want::Deny("Approval is reserved to humans."),
        ),
        (
            "approve without config",
            "plain",
            "sluice  queue   approve t-1",
            BYPASS,
            &[],
            Want::Deny("reserved to humans"),
        ),
        (
            "approve in a compound",
            "plain",
            "make && sluice queue approve t-1",
            BYPASS,
            &[],
            Want::Deny("reserved to humans"),
        ),
        (
            "approve with an env prefix",
            "plain",
            "A=1 sluice queue approve t-1",
            BYPASS,
            &[],
            Want::Deny("reserved to humans"),
        ),
        (
            "queue status is fine",
            "plain",
            "sluice queue status",
            BYPASS,
            &[],
            Want::Silent,
        ),
        (
            "downstream gate: legacy deny",
            "repo",
            "cat x | cargo test --workspace",
            BYPASS,
            &[],
            Want::Deny(gate_test),
        ),
        (
            "raw gate left beside a rewrite",
            "repo",
            "make && cat x | cargo test",
            BYPASS,
            &[],
            Want::Deny(gate_test),
        ),
        (
            "unparseable gate: legacy deny",
            "repo",
            "cargo test --workspace\necho done",
            BYPASS,
            &[],
            Want::Deny(gate_test),
        ),
        (
            "plan mode: legacy deny",
            "repo",
            "cargo test --workspace",
            Some("plan"),
            &[],
            Want::Deny(gate_test),
        ),
        (
            "plan mode, heavy: nothing",
            "plain",
            "make",
            Some("plan"),
            &[],
            Want::Silent,
        ),
        (
            "suggestion mode, gate",
            "repo",
            "cargo test --workspace",
            BYPASS,
            &[("SLUICE_GUARD", "deny")],
            Want::Deny("call the `verify` MCP tool with gates=[\"test\"]"),
        ),
        (
            "suggestion mode, other spelling",
            "repo",
            "pnpm build",
            BYPASS,
            &[("SLUICE_GUARD", "deny")],
            Want::Deny("gates=[\"build\"]"),
        ),
        (
            "suggestion mode, heavy",
            "plain",
            "make -j8",
            BYPASS,
            &[("SLUICE_GUARD", "deny")],
            Want::Deny(
                "This machine shares its CPU through Sluice: run `sluice exec --label make -- make -j8` instead (same output and exit code).",
            ),
        ),
        (
            "suggestion mode, nothing heavy",
            "plain",
            "ls",
            BYPASS,
            &[("SLUICE_GUARD", "deny")],
            Want::Silent,
        ),
        (
            "SLUICE_GUARD=0, gate",
            "repo",
            "cargo test --workspace",
            BYPASS,
            &[("SLUICE_GUARD", "0")],
            Want::Silent,
        ),
        (
            "SLUICE_GUARD=0, approve",
            "repo",
            "sluice queue approve t-1",
            BYPASS,
            &[("SLUICE_GUARD", "0")],
            Want::Silent,
        ),
        (
            "SLUICE_GUARD=off, heavy",
            "plain",
            "make",
            BYPASS,
            &[("SLUICE_GUARD", "off")],
            Want::Silent,
        ),
        (
            "SLUICE_GUARD=1 is the default",
            "plain",
            "make",
            BYPASS,
            &[("SLUICE_GUARD", "1")],
            Want::Allow("sluice exec --label make -- make"),
        ),
    ] {
        fx.check(label, cwd, cmd, mode, extra, want);
    }
}

/// Rows of the permission table (section 4.6) through the whole guard.
#[test]
fn permission_rows() {
    let make = "sluice exec --label make -- make";
    let rows: &[(&str, &str, &str, Option<&str>, Want)] = &[
        (
            "deny rule (user)",
            "home/.claude/settings.json",
            r#"{"permissions":{"deny":["Bash(make:*)"]}}"#,
            BYPASS,
            Want::Silent,
        ),
        (
            "ask rule (project)",
            "repo/.claude/settings.json",
            r#"{"permissions":{"ask":["Bash(make)"]}}"#,
            Some("default"),
            Want::Silent,
        ),
        (
            "deny rule (managed)",
            "managed-settings.json",
            r#"{"permissions":{"deny":["Bash"]}}"#,
            BYPASS,
            Want::Silent,
        ),
        (
            "unparseable settings",
            "repo/.claude/settings.local.json",
            "{ not json",
            BYPASS,
            Want::Silent,
        ),
        (
            "allow rule (local), default",
            "repo/.claude/settings.local.json",
            r#"{"permissions":{"allow":["Bash(make:*)"]}}"#,
            Some("default"),
            Want::Allow(make),
        ),
        (
            "allow rule, plan",
            "home/.claude/settings.json",
            r#"{"permissions":{"allow":["Bash(make)"]}}"#,
            Some("plan"),
            Want::Allow(make),
        ),
        (
            "allow rule, auto",
            "home/.claude/settings.json",
            r#"{"permissions":{"allow":["Bash(make)"]}}"#,
            Some("auto"),
            Want::Allow(make),
        ),
        ("no rules, bypass", "home/x", "", BYPASS, Want::Allow(make)),
        (
            "no rules, default",
            "home/x",
            "",
            Some("default"),
            Want::Ask(make),
        ),
        (
            "no rules, acceptEdits",
            "home/x",
            "",
            Some("acceptEdits"),
            Want::Ask(make),
        ),
        ("no rules, absent mode", "home/x", "", None, Want::Ask(make)),
        ("no rules, auto", "home/x", "", Some("auto"), Want::Silent),
        ("no rules, plan", "home/x", "", Some("plan"), Want::Silent),
        (
            "no rules, dontAsk",
            "home/x",
            "",
            Some("dontAsk"),
            Want::Silent,
        ),
        (
            "no rules, unknown mode",
            "home/x",
            "",
            Some("yolo"),
            Want::Silent,
        ),
        (
            "other tools only",
            "home/.claude/settings.json",
            r#"{"permissions":{"deny":["Read(./secret)"],"allow":["Edit"]}}"#,
            Some("default"),
            Want::Ask(make),
        ),
    ];
    for (label, file, text, mode, want) in rows {
        let fx = Fx::new();
        fx.write(file, text);
        fx.check(label, "plain", "make", *mode, &[], *want);
    }
    // CLAUDE_CONFIG_DIR replaces ~/.claude for the user settings.
    let fx = Fx::new();
    fs::create_dir_all(fx.path("cfg")).unwrap();
    fx.write(
        "cfg/settings.json",
        r#"{"permissions":{"deny":["Bash(make:*)"]}}"#,
    );
    let cfg = fx.path("cfg").display().to_string();
    fx.check(
        "CLAUDE_CONFIG_DIR deny",
        "plain",
        "make",
        BYPASS,
        &[("CLAUDE_CONFIG_DIR", &cfg)],
        Want::Silent,
    );
    fx.check(
        "unknown project dir",
        "plain",
        "make",
        BYPASS,
        &[("CLAUDE_PROJECT_DIR", "")],
        Want::Silent,
    );
    // A rule that hides the rewrite keeps today's deny for a gate.
    let fx = Fx::new();
    fx.write(
        "repo/.claude/settings.json",
        r#"{"permissions":{"ask":["Bash(cargo test:*)"]}}"#,
    );
    fx.check(
        "ask rule on a gate",
        "repo",
        "cargo test --workspace",
        Some("default"),
        &[],
        Want::Deny("gates=[\"test\"]"),
    );
}

/// Input handling: echo of `tool_input`, escapes, malformed input.
#[test]
fn inputs() {
    let fx = Fx::new();
    let input = json!({
        "cwd": fx.path("plain"),
        "permission_mode": "bypassPermissions",
        "tool_input": {
            "command": "docker build -t \"ñandú app\" . # x".replace(" # x", ""),
            "description": "Build the \"image\" \u{1F680}\n2nd line\\",
            "timeout": 600_000,
            "run_in_background": true,
            "future_field": {"a": [1, 2]},
        },
    });
    let out: Value =
        serde_json::from_str(&fx.respond_raw(&input.to_string(), &[]).unwrap()).unwrap();
    let updated = &out["hookSpecificOutput"]["updatedInput"];
    let mut expected = input["tool_input"].clone();
    expected["command"] =
        json!("sluice exec --label docker-build -- docker build -t \"ñandú app\" .");
    assert_eq!(updated, &expected);
    for (label, raw) in [
        ("malformed JSON", "{not json at all".to_owned()),
        ("empty input", String::new()),
        ("JSON array", "[]".to_owned()),
        ("no tool_input", json!({"cwd": "/"}).to_string()),
        (
            "no command",
            json!({"tool_input": {"description": "x"}}).to_string(),
        ),
        (
            "command not a string",
            json!({"tool_input": {"command": ["make"]}}).to_string(),
        ),
        (
            "empty command",
            json!({"tool_input": {"command": "  "}}).to_string(),
        ),
        (
            "other tool",
            json!({"tool_name": "Write", "tool_input": {"command": "make"}}).to_string(),
        ),
        (
            "other event",
            json!({"hook_event_name": "PostToolUse", "tool_input": {"command": "make"}})
                .to_string(),
        ),
    ] {
        assert_eq!(fx.respond_raw(&raw, &[]), None, "{label}");
    }
    // Missing cwd: the process directory is used.
    let missing =
        json!({"permission_mode": "bypassPermissions", "tool_input": {"command": "make"}});
    let out = fx.respond_raw(&missing.to_string(), &[]).unwrap();
    assert!(out.contains("sluice exec --label make -- make"), "{out}");
}

/// `[agent]` lists of the user config, and broken configs.
#[test]
fn configs() {
    let fx = Fx::new();
    fx.write(
        "home/sluice.toml",
        "version = 1\n[agent]\nheavy_commands = [\"echo\"]\n",
    );
    fx.check(
        "replaced list",
        "plain",
        "echo hi",
        BYPASS,
        &[],
        Want::Allow("sluice exec --label echo-hi -- echo hi"),
    );
    fx.check(
        "replaced list drops make",
        "plain",
        "make",
        BYPASS,
        &[],
        Want::Silent,
    );
    fx.write(
        "home/sluice.toml",
        "version = 1\n[agent]\nheavy_commands_extra = [\"bazel build\"]\n",
    );
    fx.check(
        "extra entry",
        "plain",
        "bazel build //...",
        BYPASS,
        &[],
        Want::Allow("sluice exec --label bazel-build -- bazel build //..."),
    );
    fx.check(
        "extra keeps the default",
        "plain",
        "make",
        BYPASS,
        &[],
        Want::Allow("sluice exec --label make -- make"),
    );
    fx.write("home/sluice.toml", "version = 1\n[agent]\nheavy = 1\n");
    fx.check(
        "invalid user config: no rewrite",
        "plain",
        "make",
        BYPASS,
        &[],
        Want::Silent,
    );
    fx.check(
        "invalid user config: legacy deny",
        "repo",
        "cargo test --workspace",
        BYPASS,
        &[],
        Want::Deny("gates=[\"test\"]"),
    );
    fx.check(
        "invalid user config: approve",
        "plain",
        "sluice queue approve x",
        BYPASS,
        &[],
        Want::Deny("reserved to humans"),
    );
    let fx = Fx::new();
    fx.write("repo/.sluice.toml", "version = 1\n[gates.test]\ncmd = 1\n");
    fx.check(
        "invalid repo config: nothing",
        "repo",
        "make",
        BYPASS,
        &[],
        Want::Silent,
    );
    fx.check(
        "invalid repo config: approve",
        "repo",
        "sluice queue approve x",
        BYPASS,
        &[],
        Want::Deny("reserved"),
    );
}

// ---------------------------------------------------------------------------
// Injection suite: a rewrite never makes a command do more than the original.

/// A fake world: every directory exists (as itself), `/r` holds the gates.
struct FakeWorld;

impl World for FakeWorld {
    fn resolve_dir(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    fn gates_from(&self, dir: &Path) -> Result<Option<Gates>, ()> {
        Ok((dir == Path::new("/r")).then(|| Gates {
            dir: PathBuf::from("/r"),
            gates: vec![
                ("test".into(), "cargo test --workspace".into()),
                ("build".into(), "npm run build".into()),
            ],
        }))
    }
}

fn words(cmd: &str, seg: &Segment) -> Vec<String> {
    let _ = cmd;
    seg.words.iter().map(|w| w.value.clone()).collect()
}

/// Checks every property of a rewrite of `original`; returns whether a
/// rewrite happened.
fn assert_safe(original: &str) -> bool {
    let Ok(plan) = matcher::plan(
        original,
        Path::new("/r"),
        matcher::ShellContext::default(),
        &HeavyMatcher::default(),
        &FakeWorld,
    ) else {
        return false;
    };
    let rewritten = plan.apply(original);
    // 1. Byte-exact reconstruction: removing each inserted prefix (and
    //    putting the gate words back) yields the original.
    let mut rebuilt = String::new();
    let mut pos = 0;
    let mut inserted = Vec::new();
    for edit in &plan.edits {
        let text = edit.text();
        match edit {
            Edit::Heavy { at, label } => {
                assert!(
                    !label.is_empty()
                        && label.len() <= 32
                        && label.chars().all(|c| c.is_ascii_lowercase()
                            || c.is_ascii_digit()
                            || "._-".contains(c)),
                    "label {label:?} of {original:?}"
                );
                assert_eq!(text, format!("sluice exec --label {label} -- "));
                rebuilt.push_str(&original[pos..*at]);
                pos = *at;
                inserted.push(text);
            }
            Edit::Gate {
                start, end, gate, ..
            } => {
                assert!(["test", "build"].contains(&gate.as_str()));
                // The replaced words are exactly the gate's words, as typed.
                let replaced = &original[*start..*end];
                let gate_cmd = if gate == "test" {
                    "cargo test --workspace"
                } else {
                    "npm run build"
                };
                let replaced_words: Vec<&str> = replaced.split_whitespace().collect();
                let gate_words: Vec<&str> = gate_cmd.split_whitespace().collect();
                let canonical_ok = sluice_gate_core::heavy::canonical_words(&replaced_words)
                    == sluice_gate_core::heavy::canonical_words(&gate_words);
                assert!(
                    replaced_words == gate_words || canonical_ok,
                    "{original:?} replaced {replaced:?}"
                );
                assert!(
                    !replaced.contains(['\'', '"', '$', '`', '\\', ';', '&', '|', '<', '>']),
                    "{original:?}"
                );
                rebuilt.push_str(&original[pos..*end]);
                pos = *end;
                inserted.push(text);
            }
        }
    }
    rebuilt.push_str(&original[pos..]);
    assert_eq!(rebuilt, original);
    // 2. The rewritten text is the original with only the inserted texts
    //    added at the edit positions.
    let mut expected = String::new();
    let mut pos = 0;
    for edit in &plan.edits {
        match edit {
            Edit::Heavy { at, .. } => {
                expected.push_str(&original[pos..*at]);
                expected.push_str(&edit.text());
                pos = *at;
            }
            Edit::Gate { start, end, .. } => {
                expected.push_str(&original[pos..*start]);
                expected.push_str(&edit.text());
                pos = *end;
            }
        }
    }
    expected.push_str(&original[pos..]);
    assert_eq!(rewritten, expected);
    // 3. The shell structure is unchanged: same segments, same operators,
    //    same redirections and assignments; each rewritten segment's words
    //    are the prefix words followed by the original command words.
    let a = lex::split(original).expect("planned commands parse");
    let b = lex::split(&rewritten)
        .unwrap_or_else(|| panic!("rewrite of {original:?} must parse: {rewritten:?}"));
    assert_eq!(a.len(), b.len(), "{original:?}");
    for (sa, sb) in a.iter().zip(&b) {
        assert_eq!(sa.op, sb.op);
        assert_eq!(sa.redirections, sb.redirections);
        assert_eq!(sa.assignments, sb.assignments);
        let wa = words(original, sa);
        let wb = words(&rewritten, sb);
        if wa == wb {
            continue;
        }
        let assigns = &wa[..sa.assignments];
        assert_eq!(&wb[..sb.assignments], assigns);
        let tail_a = &wa[sa.assignments..];
        let tail_b = &wb[sb.assignments..];
        let skip_time = usize::from(tail_a.first().is_some_and(|w| w == "time"));
        assert_eq!(&tail_b[..skip_time], &tail_a[..skip_time]);
        let (ta, tb) = (&tail_a[skip_time..], &tail_b[skip_time..]);
        if tb.get(1).map(String::as_str) == Some("exec") {
            assert_eq!(&tb[..2], ["sluice", "exec"]);
            assert_eq!(tb[2], "--label");
            assert_eq!(tb[4], "--");
            assert_eq!(&tb[5..], ta, "{original:?} -> {rewritten:?}");
        } else {
            assert_eq!(&tb[..2], ["sluice", "run"], "{original:?} -> {rewritten:?}");
            assert_eq!(tb[3], "-v");
            // Arguments after the gate words are kept, in order.
            let rest = tb.get(5..).unwrap_or_default();
            assert!(ta.ends_with(rest), "{original:?} -> {rewritten:?}");
        }
    }
    // 4. Nothing new can run: the inserted texts contain no shell syntax.
    for text in inserted {
        assert!(!text.contains([
            '\'', '"', '$', '`', '\\', ';', '&', '|', '<', '>', '(', ')', '\n', '*', '?', '~', '#'
        ]));
    }
    true
}

/// Crafted commands: each must either stay unchanged or pass every check.
#[test]
fn injection_crafted() {
    let crafted = [
        "make; rm -rf /",
        "make && rm -rf ~",
        "make '; rm -rf / #'",
        "make \"; rm -rf /\"",
        "make 'a'\"b\"c",
        "make \"$(rm -rf /)\"",
        "make `rm -rf /`",
        "make $(rm -rf /)",
        "make ${IFS}rm",
        "make $'\\x3b rm'",
        "make \\; rm",
        "make\trm",
        "make --label x -- rm",
        "make -- sluice exec --label x -- rm",
        "make > /etc/passwd",
        "make >& /dev/null",
        "make 2>&1 >/dev/null | rm",
        "make | sh",
        "make | sluice exec -- rm",
        "make && sh -c 'rm -rf /'",
        "cargo test --workspace; rm -rf /",
        "cargo test --workspace '; rm -rf /'",
        "cargo test --workspace \"x\" && rm",
        "cargo test --workspace>x",
        "cargo test --workspace -- $(id)",
        "npm run build -- '--x; rm'",
        "pnpm build",
        "cargo 'test' --workspace",
        "cargo test\t--workspace",
        "A='$(rm)' make",
        "A=\"$(rm)\" make",
        "A=1 B=2 C='x y' cargo build",
        "time make; time rm",
        "make #; rm",
        "make a#b",
        "make é && rm ñ",
        "make \u{a0}rm",
        "make \u{2028} rm",
        "make\u{0}rm",
        "make $X$Y",
        "make *",
        "make ~root",
        "make !rm",
        "make = =",
        "make 'it''s'",
        "cd x && make && cd .. && cargo test --workspace",
        "cd 'a b' && make",
        "cd \"x\" && make",
        "make & rm",
        "make&&rm",
        "make||rm",
        "make;rm",
        "make|rm",
        "make >>x 2>>y",
        "make &>x",
        "make 3>x",
        "make 10>x",
        "docker build -t x .",
        "docker build -f - .",
        "x=1;make",
        "make;",
        "make ; ; rm",
        "make '",
        "make \"",
        "make '\"' \"'\"",
        "npx -y make",
        "npx -- make",
        "pnpm exec -- make",
        "yarn -C x exec make",
        "python -m pytest -k 'a or b'",
        "./gradlew build; ./mvnw x",
    ];
    let mut rewritten = 0;
    for cmd in crafted {
        if assert_safe(cmd) {
            rewritten += 1;
        }
    }
    assert!(
        rewritten >= 30,
        "only {rewritten} crafted commands were rewritten"
    );
}

/// Random shell-ish strings (deterministic xorshift): every rewrite passes
/// every check.
#[test]
fn injection_fuzz() {
    const PIECES: &[&str] = &[
        "make",
        "cargo",
        "build",
        "test",
        "--workspace",
        "npm",
        "run",
        "pnpm",
        "npx",
        "tsc",
        "cd",
        "x",
        "..",
        "sluice",
        "sh",
        "-c",
        "time",
        "A=1",
        "B='a b'",
        "\"q q\"",
        "'s'",
        "$X",
        "$(id)",
        "`id`",
        "-",
        "--",
        "&&",
        "||",
        ";",
        "|",
        "&",
        ">",
        "2>&1",
        "<",
        "<<",
        "(",
        ")",
        "{",
        "}",
        "#",
        "\\",
        "\n",
        "\t",
        " ",
        "  ",
        "é",
        "*",
        "~",
        "!",
        "=",
        "rm",
        "-rf",
        "/",
        "docker",
        "compose",
        "python",
        "-m",
        "pytest",
        "\"",
        "'",
    ];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut rewrites = 0;
    for _ in 0..50_000 {
        let n = 1 + usize::try_from(next() % 10).unwrap();
        let mut cmd = String::new();
        for _ in 0..n {
            let piece = PIECES[usize::try_from(next()).unwrap() % PIECES.len()];
            cmd.push_str(piece);
            if next() % 3 != 0 {
                cmd.push(' ');
            }
        }
        if assert_safe(&cmd) {
            rewrites += 1;
        }
    }
    assert!(
        rewrites > 500,
        "only {rewrites} random commands were rewritten"
    );
}

// ---------------------------------------------------------------------------
// Alpha review fixes (kb/sluice/changes/alpha-review-fixes.md).

/// A world with symlinks: `/link` -> `/real/deep`, `/r/l` -> `/x/y`.
/// `resolve_dir` canonicalises like `realpath`, component by component.
struct SymWorld;

impl World for SymWorld {
    fn resolve_dir(&self, path: &Path) -> Option<PathBuf> {
        let links = [("/link", "/real/deep"), ("/r/l", "/x/y")];
        let mut out = PathBuf::from("/");
        for comp in path.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::Normal(c) => {
                    out.push(c);
                    if let Some((_, to)) = links.iter().find(|(from, _)| out == Path::new(from)) {
                        out = PathBuf::from(to);
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }

    fn gates_from(&self, dir: &Path) -> Result<Option<Gates>, ()> {
        FakeWorld.gates_from(dir)
    }
}

fn plan_in(cmd: &str, cwd: &str, cdpath: bool) -> Option<String> {
    matcher::plan(
        cmd,
        Path::new(cwd),
        matcher::ShellContext { cdpath },
        &HeavyMatcher::default(),
        &SymWorld,
    )
    .ok()
    .map(|p| p.apply(cmd))
}

#[test]
fn cd_targets_resolve_on_the_logical_directory() {
    // The shell's `cd l/..` from /r comes back to /r (logical), not /x.
    assert_eq!(
        plan_in("cd l/.. && cargo test --workspace", "/r", false).as_deref(),
        Some("cd l/.. && sluice run test -v")
    );
    assert_eq!(
        matcher::lexical_join(Path::new("/a/b"), "../c/./d").as_deref(),
        Some(Path::new("/a/c/d"))
    );
    assert_eq!(matcher::lexical_join(Path::new("rel"), "x"), None);
    // `..` while the logical directory differs from the canonical one: the
    // guard does not guess which one the shell follows.
    assert_eq!(
        plan_in("cd .. && cargo test --workspace", "/link/sub", false),
        None
    );
    // Without `..` the same directory is fine.
    assert!(plan_in("cd sub && cargo build", "/link", false).is_some());
}

#[test]
fn cdpath_disables_rewrites_that_change_directory() {
    assert_eq!(plan_in("cd sub && cargo build", "/r", true), None);
    // Without a `cd`, CDPATH does not matter.
    assert!(plan_in("cargo build", "/r", true).is_some());
    let env = |v: &str| EnvMap::from([("CDPATH".to_owned(), v.to_owned())]);
    assert!(matcher::ShellContext::from_env(&env("/src")).cdpath);
    assert!(!matcher::ShellContext::from_env(&env("")).cdpath);
    assert!(!matcher::ShellContext::from_env(&EnvMap::new()).cdpath);
}

#[test]
fn a_path_assignment_is_never_rewritten() {
    assert_eq!(plan_in("PATH=/evil:$PATH cargo build", "/r", false), None);
    assert_eq!(
        plan_in("PATH=/evil cargo test --workspace", "/r", false),
        None
    );
    assert_eq!(
        plan_in("FOO=1 cargo build", "/r", false).as_deref(),
        Some("FOO=1 sluice exec --label cargo-build -- cargo build")
    );
}

#[test]
fn approval_is_found_through_quotes_paths_and_wrappers() {
    for cmd in [
        "sluice queue approve t-1",
        "\"sluice\" queue approve t-1",
        "'sluice' 'queue' 'approve' t-1",
        "/home/u/.cargo/bin/sluice queue approve t-1",
        "./sluice queue approve --yes t-1",
        "sluice queue --yes approve t-1",
        "env sluice queue approve t-1",
        "env -i A=1 B=2 sluice queue approve t-1",
        "command sluice queue approve t-1",
        "nohup sluice queue approve t-1",
        "exec sluice queue approve t-1",
        "time sluice queue approve t-1",
        "sluice exec -- sluice queue approve t-1",
        "sluice exec --label x -- env sluice queue approve t-1",
        "sh -c 'sluice queue approve t-1'",
        "bash -lc \"s=sluice; \\$s queue approve t-1\"",
        "eval sluice queue approve t-1",
        "echo t-1 | xargs sluice queue approve",
        "timeout 5 sluice queue approve t-1",
        "s\\luice queue approve t-1",
        "$(echo sluice) queue approve t-1",
        "make && SLUICE_X=1 sluice queue approve t-1",
    ] {
        assert!(matcher::runs_approve(cmd), "{cmd}");
    }
    for cmd in [
        "sluice queue status",
        "sluice run test",
        "echo sluice queue approve",
        "git commit -m 'approve the queue design'",
        "sluice exec -- cargo build",
        "env FOO=1 cargo test",
        "sh -c 'cargo test'",
    ] {
        assert!(!matcher::runs_approve(cmd), "{cmd}");
    }
}

/// Containment (C5): a heavy rewrite carries, once, the line about an
/// intervention in the caller's own session (matched by the hook's
/// `session_id`), and never one about another session.
#[test]
fn the_context_note_tells_about_an_intervention_of_the_session_once() {
    let fx = Fx::new();
    let state = fx.path("home/.local/state/sluice");
    fs::create_dir_all(&state).unwrap();
    let at = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let event = |session: &str, cause: &str| {
        json!({"at": at, "session": session, "leaf": format!("s-{session}/tools"), "cause": cause})
            .to_string()
    };
    fs::write(
        state.join("interventions.jsonl"),
        format!(
            "{}\n{}\n",
            event("s", "oomd_memory_pressure"),
            event("other", "oom_kill")
        ),
    )
    .unwrap();
    let note = |fx: &Fx| -> String {
        let out = fx
            .respond_raw(&fx.input("repo", "make", BYPASS), &[])
            .expect("a rewrite");
        let v: Value = serde_json::from_str(&out).unwrap();
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = note(&fx);
    assert!(
        first.contains("Sluice stopped s-s/tools in your session at"),
        "{first}"
    );
    assert!(first.contains("systemd-oomd, memory pressure"), "{first}");
    assert!(!first.contains("s-other"), "{first}");
    assert!(!first.contains('\n'), "one line: {first}");
    let second = note(&fx);
    assert!(!second.contains("Sluice stopped"), "told once: {second}");
}
