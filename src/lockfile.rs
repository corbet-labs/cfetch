//! Advisory lockfile over flock(2): a PERMANENT lock file, LOCK_EX|LOCK_NB in
//! a bounded 50ms-step retry, the open file handle as the guard. The kernel
//! releases the lock when the holder's last fd closes — including on crash —
//! so there is no staleness concept and no steal path. The file is never
//! unlinked: remove-and-recreate is exactly the double-steal race (two
//! processes each "holding" a lock on a different inode behind the same
//! path), and an unlink after a steal orphans the current holder the same
//! way.
//!
//! Callers must tolerate `None` — on the hook path, running unlocked beats
//! stalling the agent; on the scan path, `None` means another process is
//! already doing the work.

use std::path::Path;
use std::time::Duration;

/// Held lock. The open file descriptor IS the lock: dropping it closes the
/// fd, which releases the flock. The lock file itself stays on disk forever.
pub struct Lock {
    _file: std::fs::File,
}

/// Acquires `path` exclusively, retrying in 50ms steps for at most
/// `max_wait_ms`. `stale_secs` is kept for signature compatibility only:
/// flock needs no staleness heuristic — a dead holder's lock dies with its
/// process.
pub fn acquire(path: &Path, max_wait_ms: u64, _stale_secs: u64) -> Option<Lock> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    let attempts = (max_wait_ms / 50).max(1);
    for attempt in 0..attempts {
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return Some(Lock { _file: file });
        }
        if attempt + 1 < attempts {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_while_held_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let l1 = acquire(&p, 100, 5).unwrap();
        assert!(acquire(&p, 100, 5).is_none(), "second acquire must fail while held");
        drop(l1);
        assert!(p.exists(), "the lock file is permanent — never unlinked");
        assert!(acquire(&p, 100, 5).is_some(), "release must be immediate on drop");
    }

    #[test]
    fn an_old_looking_lock_is_never_stolen() {
        // The old implementation stole locks by mtime; a HELD lock must now
        // stay held no matter how old the file looks — the mtime-steal was
        // the double-steal race.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let held = acquire(&p, 100, 1).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        drop(f);
        assert!(acquire(&p, 200, 1).is_none(), "no steal path may exist");
        drop(held);
        assert!(acquire(&p, 100, 1).is_some(), "an UNHELD old file is simply acquirable");
    }

    #[test]
    fn reacquire_keeps_the_same_inode() {
        // Proves lock-file permanence: no remove-and-recreate cycle, so every
        // contender always locks the SAME inode (the precondition flock needs
        // for mutual exclusion).
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let l1 = acquire(&p, 100, 5).unwrap();
        let ino1 = std::fs::metadata(&p).unwrap().ino();
        drop(l1);
        let _l2 = acquire(&p, 100, 5).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino1);
    }
}
