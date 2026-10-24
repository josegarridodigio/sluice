# Sluice: instructions for coding agents working on this repository

- Everything is in English: code, comments, commits, docs, knowledge base.
- Read `kb/sluice/plan/master-plan.md` before starting any task, and search
  the Pando knowledge base (`kb_search_documents`) for prior context.
- After every change, add or update a document under `kb/sluice/changes/`
  (or `kb/sluice/fixes/`) with what changed, files and symbols touched, why,
  and how it was verified. Decisions go to `kb/sluice/design/adr/`.
- Every increment must pass `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace`. Run the full suite once at the end of a task,
  not after every edit.
- Git operations in the product go through the `git` binary, wrapped in
  `sluice-gate-core`; never use a git library.
- Never modify the user's index or working tree from product code.
- Do not push or publish anything without the user's approval.
- Never commit personal or environment data: no home paths (`/home/<user>/...`;
  write `~/`, `$HOME` or `<repo>`), user names, host names, IP addresses or
  private project/client names, in code, scripts, docs, the KB, commit
  messages or GitHub issues. Scripts take paths from the environment
  (`$HOME`, `$ROOT`), never hard-coded. Describe other machines generically
  (for example "a second machine, 6 cores, 12 threads").
- Reference repositories live in `refs/` (gitignored); query them through
  Pando's code index (`ref-gate-cache`, `ref-agent-task-queue`, `ref-rmcp`,
  `ref-mergetrain`) instead of reading them file by file.
