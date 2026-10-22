# Manual end-to-end test: two Claude Code agents, one verification

This is the Phase 3 acceptance run that needs a human. It takes about
15 minutes. Record the output of every step in
`kb/sluice/changes/phase-3-acceptance.md` (a template is at the end).

## Setup

1. Build and install the current binary:

   ```sh
   cargo install --path crates/sluice-gate
   sluice --version
   ```

2. In a test repository (a scratch clone is fine), define a gate that takes
   a while and leaves a trace:

   ```sh
   sluice init
   cat > .sluice.toml <<'EOF'
   version = 1

   [gates.test]
   cmd = "echo run >> /tmp/sluice-manual-counter; sleep 150"
   weight = 2
   timeout = "10m"
   hooks = ["pre-commit"]
   EOF
   git add .sluice.toml && git commit -m "sluice config"
   rm -f /tmp/sluice-manual-counter
   ```

3. Install the plain hooks and register the MCP server for this repository:

   ```sh
   sluice hook print plain      # paste into .githooks/, then:
   chmod +x .githooks/*; git config core.hooksPath .githooks
   claude mcp add sluice -- sluice mcp
   ```

4. Add the `CLAUDE.md` block from `docs/agents.md` to the repository.

5. Create a second worktree: `git worktree add ../scratch-wt -b wt`.

## Steps

| # | Where | Do | Expected |
|---|---|---|---|
| 1 | Terminal | `sluice daemon stop; sluice status` | `no daemon running` |
| 2 | Agent A (main worktree) | "Run the verify tool for the test gate." | The tool call lasts about 150 s. Claude Code moves it to the background after 2 min and reports the result later. Result line `test PASS 150.xs (job …)`. Counter has one line. |
| 3 | Agent B (second worktree), started within 60 s of step 2 | same prompt | Result arrives at the same time as A's with `PASS` and `structured_content.results[0].coalesced = true`. Counter still has one line. |
| 4 | Terminal, during 2–3 | `sluice status` | one running job, `subscribers 2`. |
| 5 | Agent A | "Commit the file README.md with the message 'manual test'." (make a trivial change first) | `pre-commit` prints `sluice: test PASS (cached, passed …)` instantly; commit succeeds. |
| 6 | Agent B | edit a file, then "commit it" | The hook misses (new tree) and exits 3 with the three-line fail-fast message; the agent calls `verify` and retries; the commit then succeeds. |
| 7 | Terminal | `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000 claude` in the main worktree, edit a file, ask for `verify` | The 150 s gate completes although the idle window is 60 s (progress notifications every 15 s keep it alive). |
| 8 | Terminal | `sluice status --json \| jq .totals` | `runs 2+`, `coalesced 1`, `hits 1+`. |

## Template for the evidence note

```markdown
---
title: "Phase 3 acceptance evidence"
tags: [sluice, acceptance, phase-3]
---

# Phase 3 acceptance evidence (<date>)

Automated: `crates/sluice-gate/tests/mcp.rs` (<n> tests) — list them.

Manual run on <machine>, Claude Code <version>, Sluice <version>:

| Step | Output (trimmed) | Result |
|---|---|---|
| 1 | … | ok |
…
```
