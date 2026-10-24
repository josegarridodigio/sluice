---
title: "Containment: review and reconciliation"
tags: [sluice, changes, containment, review]
---

# Containment: review and reconciliation

Plan: [[sluice/plan/containment-plan.md]] R1–R3 (2026-10-24). Decision:
[[sluice/design/adr/0017-agent-resource-containment.md]]; design:
[[sluice/design/containment.md]].

## What landed on `containment-integration`

C1 core module and config ([[sluice/changes/containment-core.md]]), C2 daemon
and job leaves ([[sluice/changes/containment-daemon.md]]), C3/C4 session hook
and shims ([[sluice/changes/containment-session.md]]), C5 status, doctor and
interventions ([[sluice/changes/containment-observability.md]]), E1 harness s9
([[sluice/changes/containment-proof.md]]: 30/30), E2 real sessions on haiku
([[sluice/changes/containment-e2e.md]]: cases k–n pass). Gates: `sluice run
fmt`, `clippy`, `test` pass on the merged tree.

## Review findings (R1) and their resolution

| Finding | Resolution |
|---|---|
| A test leaked the hook's `sleep 86400` stopgap holder inside a stopped test scope; E2 saw the same in a product path once (scope `failed`, holder alive) | C5: the stopgap is `sluice daemon holder --lifetime 600` and exits when its scope is not active or not adopted within 30 s; the hook re-checks the scope after creating it and kills its holder and fails open if not active; test guards kill leftovers only below their own test slice |
| `plugin/hooks/test-session-start.sh` ran the binary against the developer's real user manager | fixed: empty `XDG_RUNTIME_DIR`; the lead checked no real Sluice unit was created |
| Isolated 20–150 ms stalls of the agent UI stand-in under outside load (E1) | S6: oversubscription stalls hit any sleeping task (control too); layout kept; s9 judged by p95 and share over 10 ms |
| Only one place sends signals: the daemon retires the hook's stopgap holder by exact command line, only in its own `holder/` leaf | checked, safe |
| Cgroup removal uses `remove_dir` (empty only); PIDs moved are the agent and its descendants only | checked, safe |
| s8 expected the pre-0.4.1 hook text | fixed in E1 |
| Client names in code, docs, KB, commits | none |

## Open items (for E3 and before a release)

- Rerun s2/s6/s9 numbers on an 8-core machine.
- Bash sandbox with its default seccomp filter (needs the AppArmor profile on
  Ubuntu); the interactive TUI and IDE integrations (VS Code, JetBrains).
- Memory policy on systemd 255 (Ubuntu 24.04): accounting only by design;
  verify `doctor` and behaviour there. `user.oomd_avoid` ranking effect.
- systemd 252–258: `set-property --runtime` on the slice verified on 259 only.

Open discrepancies between code, KB, docs and CHANGELOG: none found.
