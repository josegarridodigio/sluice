---
title: "Queue trust W5: human configuration bootstrap (--trust-member-config)"
tags: [sluice, changes, merge-queue, runner, security]
---

# Queue trust W5: `--trust-member-config`

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] section 6. Plan:
[[sluice/plan/queue-trust-plan.md]] wave W5. Design amended: [[sluice/design/merge-queue.md]]
sections 2, 3, 5 (step 1 and 2), 8, 9, 12, 14 and 15.

## What changed

- **Protocol.** Optional `trust_member_config` (omitted when false) on `queue-enqueue`
  (`EnqueueRequest`) and `queue-retry`; `queue-retry` also gets an optional `env` (agent-detection
  variables, only sent with the flag), because the retry carried no environment the daemon could
  classify. Feature `queue-trust-config` (`FEATURE_QUEUE_TRUST_CONFIG`) in `welcome.features`.
  `QueueTrainView` gets optional `config_source` (`"member"` for a trusted train) and `config_diff`;
  mirrored by `TrainSchema` in the MCP server. The frozen `status --json` schema test is untouched
  (the fields are skipped when absent).
- **Authorisation (daemon, `extension.rs`).** `trust_refusal` (next to `agent_approval_refusal`, both
  built on `agent_origin`): `not-allowed` for `via: mcp` and for a CLI whose environment is detected
  as an agent's, regardless of `[queue] allow_agent_approval`. Enqueue also refuses `caller: agent`.
  `approve` refuses a train with `config_source = member` the same way, before the state checks.
  The messages say a branch that picks its own gates needs a person.
- **Store.** `queue_jobs.trust_config INTEGER NOT NULL DEFAULT 0`, `queue_trains.config_source TEXT`,
  `queue_trains.config_hash TEXT`, by `add_column_if_missing` (idempotent, no `queue_schema` bump).
  `JobRow.trust_config`, `NewJob.trust_config`, `JobEdit.trust_config`, `TrainRow.config_source/
  config_hash`, `NewTrain.config_source/config_hash`. A plain retry writes `trust_config = false`,
  a trusted one `true`.
- **Runner.** `prepare_and_claim`: for a trusted first job reads `git show <head>:.sluice.toml`
  (`trust::read_member_config`: absent -> no gates, hash `absent`; invalid -> the job fails at once with
  `member_config_invalid`, detail `{error}`, no loop), skips reading the base's config (a broken base
  file no longer blocks it), takes gates/`[queue]` settings from the member and `max = 1`. `claim` takes
  the trusted job alone, stops a batch before it, and claims nothing when the first queued job is not
  exactly the one whose config was read. The train records `config_source`/`config_hash`; the prepare
  attribution probe uses the member's prepare command because the spec is derived from that config.
- **Train hash.** `TrainHashInput.config_hash: Option<&str>`: when set, the fields `config` and the hash
  are appended after the gate keys; `None` for ordinary trains, whose hash is unchanged.
- **Vouch.** `find` ignores `config_changed` for trusted trains, so the refusal hint does not claim the
  train ran the base's gates (the generic "add it to `[queue] gates`" hint applies).
- **Diff.** `trust::config_diff`: `git diff --no-color --no-ext-diff --no-textconv <base>...<head> --
  .sluice.toml`, or a note plus the whole member file when the base has none; capped at 64 KiB with a
  truncation note. `queue-status` computes it (off the async threads) for trusted trains that are
  `verifying` or `ready`.
- **CLI.** `--trust-member-config` on `enqueue` and `retry` (sending the agent-detection variables);
  `missing_feature` refuses the flag, before sending, when the daemon lacks `queue-trust-config`.
  `render_train` prints `CONFIG FROM BRANCH <branch>: this train runs the branch's own .sluice.toml` and
  the indented diff; the approval plan reuses it, so it comes before the prompt; the "this train runs the
  base's gates" remark is suppressed for trusted trains. MCP: no flag; `queue_status` text prints the
  warning and says where the diff is.
- **Docs.** `docs/merge-queue.md` ("Fixing the queue's own configuration"), `docs/agents.md`,
  `docs/cli.md`, design sections listed above (decision row 10, 2026-10-26).

## Files and symbols

- `crates/sluice-gate-proto/src/lib.rs`: `FEATURE_QUEUE_TRUST_CONFIG`, `EnqueueRequest.trust_member_config`,
  `ClientMessage::QueueRetry`, `QueueTrainView.config_source/config_diff`
- `crates/sluice-gate-queue/src/trust.rs` (new): `read_member_config`, `config_diff`, `cap`
- `crates/sluice-gate-queue/src/{extension,runner,store,model,vouch}.rs`: `trust_refusal`, `agent_origin`,
  `Retry`, `status`, `approve`, `Trust`, `is_trusted`, `claim`, `prepare_and_claim`, `prepare`,
  `TrainHashInput`, `finish_train`, `current_hash`
- `crates/sluice-gate/src/{main,queue}.rs`, `crates/sluice-gate-mcp/src/queue.rs`

## New reasons, fields, columns, feature

Reason `member_config_invalid`; fields above; columns `trust_config`, `config_source`, `config_hash`;
feature `queue-trust-config`.

## Verification

`cargo check --workspace --all-targets -j 4` clean; `cargo fmt` applied; the suite, clippy and fmt
check run once after the last wave through Sluice `verify`. New tests: proto
(`the_trust_flag_and_the_config_view_are_optional_on_the_wire`); model
(`an_ordinary_train_hash_is_the_encoding_of_earlier_releases`: fixed vector computed by hand from the
encoding, plus the trusted extension); store (`the_trust_columns_are_added_in_place_and_round_trip`);
`trust.rs` (`a_long_diff_is_cut_at_a_character_boundary_with_a_note`); CLI unit
(`a_daemon_without_the_feature_does_not_get_the_trust_flag`,
`a_trusted_train_shows_the_warning_and_the_diff_before_the_approval_prompt`); MCP unit
(`a_trusted_train_is_flagged_and_does_not_claim_the_bases_gates`); queue integration
(`trusting_the_members_config_is_refused_to_agents`,
`a_human_can_land_a_fix_of_the_configuration_the_target_cannot_pass`,
`a_plain_retry_clears_the_trust`, `an_invalid_member_config_fails_the_trusted_job_once`); e2e
(`trusting_the_members_config_gets_past_a_broken_target_config`, real binary: agent environment
refused for retry and approval with `allow_agent_approval = true`, human lands past a broken target
config).

## Not covered

No test queues a trusted job behind other queued jobs to observe "alone" directly (the claim logic is
exercised by the single-member trains above).

## Review correction (lead)

`approve` reduced the client's environment to the push allowlist (`allowlisted`)
before calling `trust_refusal`, so the agent-detection variables were gone and an
agent's CLI could approve a train built from its own branch's `.sluice.toml`
whenever `[queue] allow_agent_approval = true`. The check now receives the whole
client environment (`client_env`); only the allowlist is still stored for the
push. `a_human_can_land_a_fix_of_the_configuration_the_target_cannot_pass`
exercises exactly that case (`CLAUDECODE=1`, `allow_agent_approval = true`).
