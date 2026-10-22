//! The Sluice daemon: one per user, listening on a Unix socket.
//!
//! Holds the budget scheduler, the in-flight map used for coalescing, the
//! gate executor (process groups, timeouts, nice/ionice, logs), PSI-based
//! admission and SQLite job history. Implemented from Phase 2 on.
