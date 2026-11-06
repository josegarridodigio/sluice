---
title: "Goal: real memory containment of gate jobs and environment inputs of the cache key"
tags: [sluice, plan, goal, memory, containment, cache-key]
---

# Goal: real memory containment of gate jobs and environment inputs of the cache key

Make Sluice contain gate memory for real and key its cache on the declared
environment. Ship both as one verified, committed branch enqueued in the Sluice
merge queue. Work autonomously until every "Done when" item holds. Stop only
where "Ask first" says so.

## Start

1. Read `CLAUDE.md`, [[sluice/plan/master-plan.md]], ADR-0017, ADR-0019,
   ADR-0022, ADR-0026 and ADR-0027, [[sluice/design/cache-key.md]] and
   [[sluice/research/memory-freeze-2026-11-01.md]]. Search the KB with
   `kb_search_documents` for "containment", "ceiling", "outside_growth" and
   "key_inputs".
2. The local `main` may be behind `origin`. Run `git pull --ff-only`, then
   create the branch `feat/memory-containment-0028` from it.

## Evidence already gathered (2026-11-05 incident on a workstation; do not re-derive it)

- The desktop shell hung and stayed hung: only the mouse moved, every key
  combination was dead, and the machine was rebooted with SysRq. The shell's
  last normal log line came at the end of a memory peak: 21 GiB used, swap from
  3.6 to 5.2 GiB, swap-out up to 1,700 pages/s, load 32. After that, one core
  spun for an hour while the machine sat idle.
- The peak came from a frontend `test` gate (about 15 GiB) started "alone beyond
  its estimate" (learned 17.7 to 21.2 GiB, 12.9 to 16.4 GiB free), Playwright
  jobs of about 8 GiB, and builds sandboxed outside Sluice. There was no OOM and
  no ceiling kill.
- Confirmed with a probe: Playwright's `chrome-headless-shell` moves its browser
  process into `app-org.chromium.Chromium-<pid>.scope`, out of the job leaf. Its
  memory is therefore missing from `gate_memory`, outside the job's `memory.max`,
  and out of reach of `cgroup.kill`.
- The ADR-0027 ceiling leaves swap unlimited, so a job held at its `memory.max`
  swaps instead of dying.
- Sluice keeps no per-job PSI, swap or escape data, so the incident cannot be
  reconstructed.
- The other team machines on 0.10.x show 0 OOM and 0 memory kills since the
  ceiling shipped.

## Work, in this order

### A. Telemetry (land it first; it is what the next incident will be judged on)

- Each second the daemon records, per running job: `memory.pressure`
  (some/full avg10), `memory.swap.current`, `memory.stat` anon/file, and the
  processes that left the job leaf. It also records the pressure of the user's
  session slice, read only.
- Log one line when a job crosses a threshold. Store the per-job peaks (new
  table, migration, schema bump).
- Expose them in `status`, in the MCP `status` tool and in `sluice cache status`.
  *Amended while implementing:* `sluice cache status` shows the cache state of the
  gates for a tree, not runs, so the telemetry goes to `status` (CLI and MCP, one
  shared text), the daemon log and `job_memory_pressure`; nothing is added to
  `cache status`.

### B. ADR-0028 "Gate jobs do not degrade the desktop"

1. Research before designing:
   - Chromium's source: when and how it moves itself to a systemd scope, and
     any switch, environment variable or cgroup condition that prevents it. Also
     Firefox and WebKit under Playwright.
   - How other tools handle processes that escape a cgroup (systemd
     `Delegate=`, `cgroup.subtree_control`, BuildKit, Nix, Bazel sandboxes).
   - Swap control per cgroup (`memory.swap.max`, `memory.zswap.max`) and how it
     interacts with `memory.max`.

   Search in English, Chinese, Russian, Spanish, French, Portuguese, Japanese,
   German and Korean, in their own sources. Cite the original source of each
   fact and say when a fact appears in one language only.
2. Decide in the ADR, with evidence:
   - (a) how Sluice keeps or reclaims escaped processes, so their memory counts
     and they die with the job;
   - (b) whether "starts alone beyond its estimate" stays, and if so its bound;
   - (c) swap limits for job leaves;
   - (d) an interactivity target: session-slice PSI that admission and the
     ceiling react to.

   Use no flag workarounds in user projects and make no changes to units the
   user owns.
3. Implement, with unit tests and an integration test that uses a synthetic
   escaping child (a tiny process that moves itself to a scope the test
   creates), never real memory pressure.

### C. ADR-0029 "Environment inputs of the cache key"

- `key_env`: named variables from the sanitised gate environment go into the
  key.
- `sluice init` fills `key_env` and an installed-state `key_file` per detected
  stack.
- `sluice doctor` warns when `paths` leave out lockfiles, toolchain files or
  test config of the detected stack.
- Keys of configurations that do not use the new options stay byte-identical,
  proven by golden tests.
- Update [[sluice/design/cache-key.md]], which is a public contract.

## Rules (they are not optional)

- Follow `CLAUDE.md` and the user's global instructions. Everything is in
  English. No personal data in code, KB or commits: no home paths, host names,
  IPs, user names or client project names.
- Verify only through the Sluice `verify` MCP tool or `sluice run`. Never run
  cargo test, clippy or fmt through the shell. Run the full gates once per
  finished part, not after every edit.
- Spike safety: no memory-pressure experiments on the user's machine, no
  touching processes or units the probe did not start, no sub-agents running
  spikes. Probes must be bounded (seconds, under 1 GiB, inside their own
  transient scope) and clean up after themselves.
- No patches and no "temporary" tweaks on any machine. Only product changes.
- Every part gets a KB note under `kb/sluice/changes/` (files, symbols, why, how
  verified). Decisions go into ADRs. CHANGELOG entries go under the existing
  empty Unreleased headings.
- Commits follow the existing style, with no AI attribution of any kind.
- Sub-agents use model sonnet. Give each one a narrow, self-contained brief.

## Ask first (stop and ask the user; do not do these)

- push, PR, release, tag, or approving a merge train;
- installing or deploying to any machine, or changing system configuration
  (sshd, sysrq, swap);
- any experiment that would create memory pressure;
- changing an accepted ADR's decision in a way that alters behaviour users
  already rely on (amend it with evidence, then report).

## Done when

- [ ] ADR-0028 and ADR-0029 are written with their research notes and evidence;
      the master plan is updated.
- [ ] Telemetry, containment and `key_env` are implemented with tests,
      including the synthetic-escape integration test and the golden key tests.
- [ ] `verify` passes on the committed branch for fmt, clippy and test.
- [ ] KB change notes, `docs/configuration.md`, README sections and the
      CHANGELOG are updated.
- [ ] The branch is enqueued with `queue_enqueue`, and its job and train ids are
      reported.
- [ ] A final report in Spanish to the user covers what changed, the evidence
      behind each decision, what is still unproven about the desktop hang, and
      the exact steps that need their approval (release, deploy to the team
      machines).
