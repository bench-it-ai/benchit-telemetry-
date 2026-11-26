//! Default file locations.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Data directory, in order of preference:
///
/// 1. `$BENCHIT_TELEMETRY_DIR`;
/// 2. the legacy `~/telemetry` **if it already holds a `metrics.db`** and the standard location
///    does not (so machines deployed with the first layout keep their history without any flags);
/// 3. `$XDG_STATE_HOME/benchit-telemetry`, i.e. `~/.local/state/benchit-telemetry` (the default
///    for new installs).
pub fn default_dir() -> PathBuf {
    resolve_dir(
        std::env::var_os("BENCHIT_TELEMETRY_DIR"),
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Pure form of [`default_dir`] (for tests).
pub fn resolve_dir(explicit: Option<OsString>, xdg_state: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    if let Some(d) = explicit.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = home.unwrap_or_else(|| PathBuf::from("."));
    let standard = match xdg_state.filter(|d| !d.is_empty()) {
        Some(x) => PathBuf::from(x).join("benchit-telemetry"),
        None => home.join(".local/state/benchit-telemetry"),
    };
    let legacy = home.join("telemetry");
    if legacy.join(DB_FILE).exists() && !standard.join(DB_FILE).exists() {
        return legacy;
    }
    standard
}

/// Directory holding the fast-mode lease files of running `run` commands.
pub fn lease_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(FAST_LEASE_DIR)
}

pub const DB_FILE: &str = "metrics.db";
/// Native snapshot JSON written every tick.
pub const CURRENT_FILE: &str = "current.json";
/// Flat panel JSON written every tick (desktop widget feed).
pub const PANEL_FILE: &str = "latest.json";
/// Legacy fast-mode switch: while it holds a future unix time, the daemon stores every tick.
pub const FAST_FILE: &str = "fast-until";
/// Fast-mode leases, one file per `run` process (`<pid>` containing an expiry unix time).
pub const FAST_LEASE_DIR: &str = "fast-until.d";
/// Held with `flock` by the running daemon so two cannot share a directory.
pub const LOCK_FILE: &str = "daemon.lock";
/// Directory under the data dir for panel/CLI history exports.
pub const EXPORTS_DIR: &str = "exports";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dir_wins() {
        let d = resolve_dir(Some("/x/y".into()), Some("/state".into()), Some("/home/u".into()));
        assert_eq!(d, PathBuf::from("/x/y"));
    }

    #[test]
    fn new_installs_use_the_xdg_state_dir() {
        let h = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_dir(None, None, Some(h.path().to_path_buf())),
            h.path().join(".local/state/benchit-telemetry")
        );
        assert_eq!(
            resolve_dir(None, Some("/state".into()), Some(h.path().to_path_buf())),
            PathBuf::from("/state/benchit-telemetry")
        );
    }

    #[test]
    fn existing_legacy_layout_keeps_working() {
        let h = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(h.path().join("telemetry")).unwrap();
        // a bare ~/telemetry directory is not enough ...
        assert_eq!(
            resolve_dir(None, None, Some(h.path().to_path_buf())),
            h.path().join(".local/state/benchit-telemetry")
        );
        // ... it must hold a database
        std::fs::write(h.path().join("telemetry/metrics.db"), b"").unwrap();
        assert_eq!(resolve_dir(None, None, Some(h.path().to_path_buf())), h.path().join("telemetry"));
        // once the standard location has data too, it wins
        let std_dir = h.path().join(".local/state/benchit-telemetry");
        std::fs::create_dir_all(&std_dir).unwrap();
        std::fs::write(std_dir.join("metrics.db"), b"").unwrap();
        assert_eq!(resolve_dir(None, None, Some(h.path().to_path_buf())), std_dir);
    }
}
