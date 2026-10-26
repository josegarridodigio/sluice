//! Text `sluice init` produces: the `.sluice.toml`, the summary table and
//! the unified diff of the hook edits.

use std::fmt::Write as _;

use super::{GatePlan, Source, WeightSpec};

/// A TOML basic string.
fn toml_str(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_list(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|i| toml_str(i)).collect();
    format!("[{}]", items.join(", "))
}

/// The weight as written in the config.
pub fn weight_text(weight: WeightSpec) -> String {
    match weight {
        WeightSpec::Fixed(n) => n.to_string(),
        WeightSpec::Auto => "\"auto\"".to_owned(),
    }
}

fn gate_section(gate: &GatePlan) -> String {
    if let Some(raw) = &gate.carried {
        return raw.clone();
    }
    let mut out = String::new();
    let _ = writeln!(out, "[gates.{}]", gate.name);
    match &gate.source {
        Source::Hook(file) => {
            let _ = writeln!(out, "# From the command in {file}.");
        }
        Source::Detected(file) => {
            let _ = writeln!(out, "# Detected from {file}.");
        }
        Source::Carried => {}
    }
    let _ = writeln!(out, "cmd = {}", toml_str(&gate.cmd));
    let _ = writeln!(out, "weight = {}", weight_text(gate.weight));
    let _ = writeln!(out, "timeout = {}", toml_str(&gate.timeout));
    if !gate.hooks.is_empty() {
        let _ = writeln!(out, "hooks = {}", toml_list(&gate.hooks));
    }
    if !gate.key_inputs.is_empty() {
        let _ = writeln!(out, "key_inputs = {}", toml_list(&gate.key_inputs));
    }
    if !gate.env.is_empty() {
        let pairs: Vec<String> = gate
            .env
            .iter()
            .map(|(k, v)| format!("{k} = {}", toml_str(v)))
            .collect();
        let _ = writeln!(out, "env = {{ {} }}", pairs.join(", "));
    }
    if !gate.cache {
        let _ = writeln!(out, "cache = false");
    }
    out
}

/// The `.sluice.toml` for these gates.
pub fn config_text(
    gates: &[GatePlan],
    stacks: &[String],
    nested: &[String],
    prepare: Option<&str>,
) -> String {
    let mut out = String::from(
        "# Sluice repository configuration, written by `sluice init`.\n\
         # Commit it: the gates are shared by everyone who works on this repository.\n\
         # weight: a number is a ceiling in budget units (roughly the CPU threads the\n\
         # gate uses); \"auto\" starts at half the daemon budget and learns a lower\n\
         # weight from measured runs (needs Sluice 0.4.0). An omitted weight means 1.\n\
         version = 1\n\n\
         [defaults]\n\
         agent_hook_policy = \"fail-fast\"   # fail-fast | wait | run-unqueued\n\
         human_hook_policy = \"wait\"        # wait | run-unqueued\n\
         tail_lines = 40\n",
    );
    if !stacks.is_empty() {
        let _ = write!(out, "\n# Detected at the root: {}.\n", stacks.join(", "));
    }
    if !nested.is_empty() {
        out.push_str("# Projects below the root are not configured; add their gates by hand:\n");
        for project in nested {
            let _ = writeln!(out, "#   {project}");
        }
    }
    // Before the gates: a carried gate table would otherwise swallow these lines.
    if let Some(cmd) = prepare {
        out.push_str(
            "\n# The merge queue verifies each train in a fresh worktree that has none of the\n\
             # git-ignored dependencies; this runs once per train before the gates\n\
             # (needs Sluice 0.6.0).\n\
             [queue]\n",
        );
        let _ = writeln!(out, "prepare = {}", toml_str(cmd));
    }
    for gate in gates {
        out.push('\n');
        out.push_str(&gate_section(gate));
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

/// The summary table of the gates.
pub fn summary_table(gates: &[GatePlan]) -> String {
    let rows: Vec<[String; 5]> = gates
        .iter()
        .map(|g| {
            let hooks = if g.hooks.is_empty() {
                "-".to_owned()
            } else {
                g.hooks.join(", ")
            };
            let source = match &g.source {
                Source::Hook(file) => format!("hook ({file})"),
                Source::Detected(file) => format!("detected ({file})"),
                Source::Carried => "existing config".to_owned(),
            };
            [
                g.name.clone(),
                g.cmd.clone(),
                weight_text(g.weight).trim_matches('"').to_owned(),
                hooks,
                source,
            ]
        })
        .collect();
    let head = ["gate", "cmd", "weight", "hooks", "source"].map(str::to_owned);
    let widths: Vec<usize> = (0..5)
        .map(|c| {
            rows.iter()
                .map(|r| r[c].chars().count())
                .chain([head[c].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in std::iter::once(&head).chain(&rows) {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| pad(cell, *w))
            .collect();
        let _ = writeln!(out, "  {}", line.join("  ").trim_end());
    }
    out
}

/// One edit of a line-by-line diff.
enum Op {
    Equal,
    Delete,
    Add,
}

/// The operations that turn `a` into `b` (common prefix and suffix are cut
/// first, the middle goes through a longest-common-subsequence table).
#[allow(clippy::many_single_char_names)]
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix).map(|_| Op::Equal).collect();
    let (n, m) = (am.len(), bm.len());
    if n.saturating_mul(m) > 4_000_000 {
        ops.extend((0..n).map(|_| Op::Delete));
        ops.extend((0..m).map(|_| Op::Add));
    } else {
        let mut table = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[i][j] = if am[i] == bm[j] {
                    table[i + 1][j + 1] + 1
                } else {
                    table[i + 1][j].max(table[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Equal);
                i += 1;
                j += 1;
            } else if i < n && (j == m || table[i + 1][j] >= table[i][j + 1]) {
                ops.push(Op::Delete);
                i += 1;
            } else {
                ops.push(Op::Add);
                j += 1;
            }
        }
    }
    ops.extend((0..suffix).map(|_| Op::Equal));
    ops
}

fn display(line: &str) -> &str {
    line.strip_suffix('\n')
        .map_or(line, |l| l.strip_suffix('\r').unwrap_or(l))
}

/// A unified diff (two lines of context) of `old` into `new`; empty when
/// they are equal.
#[allow(clippy::many_single_char_names)]
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    const CONTEXT: usize = 2;
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = diff_ops(&a, &b);
    // Positions (in a and in b) before each op.
    let mut at = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for op in &ops {
        at.push((i, j));
        match op {
            Op::Equal => {
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Add => j += 1,
        }
    }
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal))
        .map(|(n, _)| n)
        .collect();
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut k = 0;
    while k < changed.len() {
        let first = changed[k];
        let mut last = first;
        while k + 1 < changed.len() && changed[k + 1] - last <= 2 * CONTEXT + 1 {
            k += 1;
            last = changed[k];
        }
        k += 1;
        let from = first.saturating_sub(CONTEXT);
        let to = (last + 1 + CONTEXT).min(ops.len());
        let (a_start, b_start) = at[from];
        let slice = &ops[from..to];
        let a_len = slice.iter().filter(|o| !matches!(o, Op::Add)).count();
        let b_len = slice.iter().filter(|o| !matches!(o, Op::Delete)).count();
        let _ = writeln!(
            out,
            "@@ -{},{a_len} +{},{b_len} @@",
            a_start + 1,
            b_start + 1
        );
        let (mut i, mut j) = (a_start, b_start);
        for op in slice {
            match op {
                Op::Equal => {
                    let _ = writeln!(out, " {}", display(a[i]));
                    i += 1;
                    j += 1;
                }
                Op::Delete => {
                    let _ = writeln!(out, "-{}", display(a[i]));
                    i += 1;
                }
                Op::Add => {
                    let _ = writeln!(out, "+{}", display(b[j]));
                    j += 1;
                }
            }
        }
    }
    out
}

/// The diff of a file that does not exist yet.
pub fn new_file_diff(path: &str, text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = format!(
        "--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for line in lines {
        let _ = writeln!(out, "+{}", display(line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_line_is_one_hunk_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nX\ne\nf\ng\n";
        assert_eq!(
            unified_diff("f", old, new),
            "--- a/f\n+++ b/f\n@@ -2,5 +2,5 @@\n b\n c\n-d\n+X\n e\n f\n"
        );
    }

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified_diff("f", "a\n", "a\n"), "");
    }

    #[test]
    fn an_insertion_is_shown() {
        let d = unified_diff("f", "a\nb\n", "a\nx\nb\n");
        assert!(d.contains("+x\n"));
        assert!(d.contains("@@ -1,2 +1,3 @@"));
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_str("say \"hi\" \\ x"), "\"say \\\"hi\\\" \\\\ x\"");
    }
}
