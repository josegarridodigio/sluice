---
title: "Fix: the guard's approval detection compares whole words (issue #2)"
tags: [sluice, fixes, guard, issue-2]
---

# Fix: the guard's approval detection compares whole words (issue #2)

## Problem

When the guard's tokenizer could not split a command (heredocs, loops,
`$(...)`), or for opaque wrappers (`sh -c`, `eval`, `sudo`, `timeout`, …),
`runs_approve` fell back to `text.contains("approve") && text.contains("queue")`.
Any prose with "approved" next to an unrelated `sluice queue status` was denied
with "Approval is reserved to humans." It happened repeatedly while developing
0.4.1 and 0.5.0 (writing plans, memory notes, and the fix itself).

## Decision

Measured in a spike (38 cases, two parsers, the guard as it was): a real
shell parser (`tree-sitter-bash` or `brush-parser`) was 36–38/38 correct, a
whole-word fallback 37/38 with no dependency, the old guard 31/38. Shims and
the daemon-side refusal already cover the approval path (an agent can always
write a script the guard never sees), so a parser's marginal gain does not
justify its weight (9.5 MB of generated C, or 33 new crates). The hand-made
fix was chosen.

## Change

`crates/sluice-gate/src/guard/matcher.rs`:

- `mentions_approve` joins the text the way the shell does before comparing
  (a backslash-newline continues the word; quotes and backslashes concatenate
  rather than split), splits on whitespace and `;&|()<>{}$`` ` ``, and returns
  true only when a word is exactly `queue` and a later one is exactly
  `approve`. "approved", "approval" and the like no longer count;
  `'appr'ove`, `appr\ove` and a line continuation inside the word still do.
- `words_approve`: a program name the shell computes (`$x queue approve`)
  is decided by `mentions_approve` instead of being allowed.

## Tests

`crates/sluice-gate/src/guard/tests.rs`,
`approval_detection_compares_whole_words_issue_2`: 9 commands that must be
allowed (the issue's false positives and similar prose in heredocs and loops)
and 29 that must be denied (direct, by path, with options, in loops,
substitutions, backquotes, `sh -c`, `bash -c`, `eval`, `env`, `timeout`,
`sudo`, quoted and escaped words, `$x`, `if`, subshells, `sluice exec --`,
pipelines, heredoc after the command, `[[ ]]`, process substitution,
background, functions, `case`, line continuation, concatenated quotes,
`while read`, unterminated quote). The existing approval tests are unchanged
and pass. Verified with `sluice run fmt`, `clippy`, `test`.

## Known remainder

A comment such as `# sluice queue approve` in a command the tokenizer refuses
is still denied (a harmless false positive). Parsing comments by hand would
add risk for no protection.
