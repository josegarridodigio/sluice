---
title: "Containment: user checks on a second machine over SSH (E3, partial)"
tags: [sluice, changes, containment, e3, ssh]
---

# Containment: user checks on a second machine over SSH (E3, partial)

Plan: [[sluice/plan/containment-plan.md]] E3. Date: 2026-10-24. Run by the
lead over SSH on the user's second machine with the user's consent. Binary:
a release build of `main` at `179e230` copied with the harness and the plugin
to a private directory; private XDG dirs; nothing installed system-wide;
everything removed afterwards (no unit, file or state dir left).

Machine: Ubuntu 26.04.1, systemd 259, kernel 7.0.0-38, Intel i7-9750H
(6 cores, 12 threads), 31 GB RAM, 8 GB swap, `systemd-oomd` active, user
manager delegating `cpu memory pids`; a desktop session was logged in.

## Results

| Check | Result |
|---|---|
| `sluice doctor` from the SSH session | `containment: full`; `own-cgroup` warns that the shell is in a login scope (`session-N.scope`) and cannot move itself, the agent is attached through the user manager; `system-bus` reachable; `memory-policy` on at 30 % for 5 s derived from 50 %/20 s |
| s9 inside a user scope (like a graphical terminal) | 30/30. Escapes (script, absolute path, Python `subprocess`, background, `setsid nohup`) all in `tools`. 2x (24 loops): first round failed the `ui` bound with 14 of 1 200 wake-ups over 10 ms (1.2 %, control 2), the retry passed (ui p95 76 µs, 2 over 10 ms). 1x (12 loops): ui p95 74 µs, 0 over 10 ms. Gates vs agents: 70.3 % / 2.37x |
| Agent started from an SSH login scope | the hook attached the agent from `session-16.scope` into `s-<id>/ui` through the user manager (`contained=full`); a shell the agent started afterwards moved itself into `tools` (nice 0); one env line; hook 195–215 ms on the first run (it also created the units) |

## Findings and open items

- On both machines the first 2x round sometimes exceeds the 1 % stall
  budget for the `ui` probe more than for the control (here 14 vs 2). S6
  attributes the stalls to oversubscription; keep collecting data before
  tightening or loosening the bound.
- The first session hook without a running daemon takes about 200 ms because
  it creates the units, close to the 230 ms containment budget inside the
  300 ms watchdog. On a slow machine the first session could fail open
  (uncontained, explained by `doctor`). Candidate improvement: let the
  session hook start the daemon early and only create the session node, or
  raise the budget for the very first run.
- Still open for E3: the Bash sandbox with its default seccomp filter, the
  interactive TUI, IDE integrations, a machine with systemd < 257.
