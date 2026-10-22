//! IPC protocol types between the Sluice daemon and its clients.
//!
//! Messages are newline-delimited JSON over a Unix socket. The protocol is
//! versioned through [`PROTOCOL_VERSION`], exchanged in the hello handshake.
//! Message types arrive in Phase 2.

/// Version of the daemon <-> client protocol.
pub const PROTOCOL_VERSION: u32 = 1;
