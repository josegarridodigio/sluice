//! Tracing setup of the CLI (`sluice run`): stderr, filtered by `SLUICE_LOG`,
//! then `[debug] level` of the user config, then `info`.

use sluice_gate_core::config::resolve_log_level;
use sluice_gate_core::git::EnvMap;

/// Installs the stderr subscriber once; later calls are no-ops.
pub fn init_cli(env: &EnvMap, configured: &str) {
    let level = resolve_log_level(env, Some(configured));
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
