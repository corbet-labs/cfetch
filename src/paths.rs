//! Filesystem locations. The state dir is per-host LOCAL on purpose: the SQLite
//! index (Milestone 2) cannot live on NFS, and hook state must never land in the
//! shared brain tree.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// Per-host mutable state: heartbeat, ledger, daemon socket fallback.
pub fn state_dir() -> PathBuf {
    match std::env::var_os("CFETCH_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => home().join(".local/state/cfetch"),
    }
}

/// Daemon socket: runtime dir when available (tmpfs, cleaned on logout),
/// state dir otherwise.
pub fn socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let rt = PathBuf::from(rt);
        if rt.is_dir() {
            return rt.join("cfetch.sock");
        }
    }
    state_dir().join("daemon.sock")
}

pub fn config_path() -> PathBuf {
    match std::env::var_os("CFETCH_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => home().join(".config/cfetch/config.json"),
    }
}

/// The shared brain tree (source of truth, git-tracked markdown).
pub fn default_brain_root() -> PathBuf {
    match std::env::var_os("CFETCH_BRAIN") {
        Some(p) => PathBuf::from(p),
        None => home().join("agents"),
    }
}
