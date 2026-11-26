//! Default file locations.
use std::path::PathBuf;

/// Data directory: `$BENCHIT_TELEMETRY_DIR`, else `$XDG_STATE_HOME/benchit-telemetry`,
/// else `~/.local/state/benchit-telemetry`.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BENCHIT_TELEMETRY_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("benchit-telemetry");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/state/benchit-telemetry")
}

pub const DB_FILE: &str = "metrics.db";
/// Native snapshot JSON written every tick.
pub const CURRENT_FILE: &str = "current.json";
/// Flat panel JSON written every tick (desktop widget feed).
pub const PANEL_FILE: &str = "latest.json";
/// While this file holds a future unix time, the daemon stores every tick.
pub const FAST_FILE: &str = "fast-until";
