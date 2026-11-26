//! Fast-mode leases.
//!
//! `benchit-telemetry run` asks the daemon to store every tick while a benchmark runs. Each run
//! owns one lease file `<dir>/fast-until.d/<pid>` holding an expiry time a few seconds in the
//! future and refreshes it while alive, so a crashed or `kill -9`'d `run` costs at most one
//! lease length of fast mode, never days. The daemon takes the latest expiry over all leases,
//! ignoring expired ones and ones whose process is gone, and deletes those. The old single
//! `fast-until` file is still honoured.
use crate::paths;
use std::path::{Path, PathBuf};

/// How long one lease lasts without a refresh.
pub const LEASE_SECS: f64 = 30.0;

fn lease_path(dir: &Path, pid: u32) -> PathBuf {
    paths::lease_dir(dir).join(pid.to_string())
}

/// Create or refresh the lease for `pid`, expiring at unix time `expires`.
pub fn write(dir: &Path, pid: u32, expires: f64) -> std::io::Result<()> {
    let d = paths::lease_dir(dir);
    std::fs::create_dir_all(&d)?;
    let p = lease_path(dir, pid);
    let tmp = d.join(format!(".{pid}.tmp"));
    std::fs::write(&tmp, expires.to_string())?;
    std::fs::rename(tmp, p)
}

pub fn remove(dir: &Path, pid: u32) {
    let _ = std::fs::remove_file(lease_path(dir, pid));
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists and can be signalled.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// True if any live, unexpired lease (or the legacy `fast-until` file) is in force at `now`.
/// Expired and orphaned leases are removed as a side effect.
pub fn fast_active(dir: &Path, now: f64) -> bool {
    let mut active = std::fs::read_to_string(dir.join(paths::FAST_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .is_some_and(|t| t > now);
    if let Ok(rd) = std::fs::read_dir(paths::lease_dir(dir)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(pid) = name.parse::<u32>() else { continue };
            let expires = std::fs::read_to_string(e.path()).ok().and_then(|s| s.trim().parse::<f64>().ok());
            match expires {
                Some(t) if t > now && pid_alive(pid) => active = true,
                _ => {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_lease_activates_fast_mode_and_expires() {
        let d = tempfile::tempdir().unwrap();
        assert!(!fast_active(d.path(), 100.0));
        write(d.path(), std::process::id(), 130.0).unwrap();
        assert!(fast_active(d.path(), 100.0));
        assert!(!fast_active(d.path(), 131.0), "expired lease no longer counts");
        assert!(!lease_path(d.path(), std::process::id()).exists(), "and is cleaned up");
    }

    #[test]
    fn lease_of_a_dead_process_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        // a pid that cannot exist (above pid_max)
        write(d.path(), 4_000_000_000 % 4_194_304 + 4_194_000, 1e12).unwrap();
        let pid = 4_000_000_000u64 % 4_194_304 + 4_194_000;
        assert!(!pid_alive(pid as u32));
        assert!(!fast_active(d.path(), 100.0));
    }

    #[test]
    fn the_longest_live_lease_wins_and_remove_works() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), std::process::id(), 200.0).unwrap();
        assert!(fast_active(d.path(), 150.0));
        remove(d.path(), std::process::id());
        assert!(!fast_active(d.path(), 150.0));
    }

    #[test]
    fn legacy_fast_until_file_still_works() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(paths::FAST_FILE), "500").unwrap();
        assert!(fast_active(d.path(), 100.0));
        assert!(!fast_active(d.path(), 600.0));
    }
}
