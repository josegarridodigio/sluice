---
title: "ADR-0013: Compatibility promise from 0.3.0"
tags: [sluice, adr, alpha, compatibility]
---

# ADR-0013: Compatibility promise from 0.3.0

Status: accepted (2026-10-22). Design: [[sluice/design/compatibility.md]].
Plan: [[sluice/plan/alpha-plan.md]] task H3.

## Context

Formats were declared frozen at version 1 but nothing proved it, several
readers in released binaries are strict (markers with unknown fields are
deleted by 0.1.0/0.2.0, wire enums have no fallback, the daemon requires an
exact database schema version and has no migrations), and alpha users need
to know what may change.

## Decision

- The promise of section 1 of the design applies from 0.3.0: every
  interface keeps reading what released versions wrote; changes are
  additive only; a breaking change needs a new interface version, a
  migration path and one release of deprecation warnings. Whether old
  binaries read new data is documented per interface as a fact.
- Gaps fixed in 0.3.0 (design section 4): G4a (markers stop rejecting
  unknown fields; new marker fields stay forbidden until one release after
  that), G5 (`Unknown` fallbacks on the four wire enums), G1 (new gate
  options use new keys, never new types), G3, G6, G8, G10–G14 (fixtures,
  snapshots and tests), G9 (a database migration framework: numbered,
  transactional migrations on open, a `sluice.db.bak-<version>` copy before
  migrating, a test from a real 0.2.0 database; new tables and nullable
  columns are preferred so 0.2.0 binaries keep opening the file).
- Golden files from the 0.1.0 and 0.2.0 release binaries live under
  `crates/*/tests/fixtures/compat/` and are never edited to make a test
  pass; 0.3.0 adds its own directory at release time.
- `docs/compatibility.md` is the user-facing version of this promise.

## Consequences

- Downgrades are supported only as far as the facts allow: binaries older
  than 0.3.0 refuse a database whose schema version changed; the backup
  copy and a warning from `sluice update` cover that case.
- Every format change now costs a fixture and a test, by design.
