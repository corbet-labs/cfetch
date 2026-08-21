//! Best-effort advisory lockfile: O_EXCL create, bounded wait, stale-steal.
//! Callers must tolerate `None` — on the hook path, running unlocked beats
//! stalling the agent; on the scan path, `None` means another process is
//! already doing the work.

use std::io::Write as _;
use std::path::{Path, PathBuf};

pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn acquire(path: &Path, max_wait_ms: u64, stale_secs: u64) -> Option<Lock> {
    let attempts = (max_wait_ms / 50).max(1);
    for _ in 0..attempts {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return Some(Lock(path.to_path_buf()));
            }
            Err(_) => {
                let stale = std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|e| e.as_secs() >= stale_secs);
                if stale {
                    let _ = std::fs::remove_file(path);
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_release_and_stale_steal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let l1 = acquire(&p, 100, 5).unwrap();
        assert!(acquire(&p, 100, 5).is_none(), "second acquire must fail while held");
        drop(l1);
        assert!(!p.exists());
        // stale steal
        std::fs::write(&p, "1").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)).unwrap();
        drop(f);
        assert!(acquire(&p, 100, 5).is_some());
    }
}
