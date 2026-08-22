//! Advisory lockfile with the SAME contract on every platform: a PERMANENT
//! lock file, a non-blocking exclusive claim in a bounded 50ms-step retry, the
//! open file handle as the guard. The kernel releases the claim when the
//! holder's last handle closes — including on crash — so there is no
//! staleness concept and no steal path. The file is never unlinked:
//! remove-and-recreate is exactly the double-steal race (two processes each
//! "holding" a lock on a different inode behind the same path), and an unlink
//! after a steal orphans the current holder the same way.
//!
//! - unix: `flock(2)` with `LOCK_EX|LOCK_NB` on an already-open file.
//! - Windows: the OPEN ITSELF is the claim — `dwShareMode = 0`
//!   (`FILE_SHARE_NONE`), so a second opener gets `ERROR_SHARING_VIOLATION`
//!   while any handle is outstanding. This is chosen over `LockFileEx`
//!   deliberately: it needs no `windows-sys` dependency for a single call,
//!   and it yields the identical guarantees — object-manager enforced, kernel
//!   released on last-handle close (crash included), no unlink, and the same
//!   refusal when ONE process opens the same path twice, which is what
//!   `flock` does for two open file descriptions too.
//!
//! Callers must tolerate `None` — on the hook path, running unlocked beats
//! stalling the agent; on the scan path, `None` means another process is
//! already doing the work.

use std::path::Path;
use std::time::Duration;

/// Held lock. The open file handle IS the lock: dropping it closes the
/// handle, which releases the claim. The lock file itself stays on disk
/// forever.
pub struct Lock {
    _file: std::fs::File,
}

/// Acquires `path` exclusively, retrying in 50ms steps for at most
/// `max_wait_ms`. `stale_secs` is kept for signature compatibility only:
/// neither backend needs a staleness heuristic — a dead holder's lock dies
/// with its process.
#[cfg(unix)]
pub fn acquire(path: &Path, max_wait_ms: u64, _stale_secs: u64) -> Option<Lock> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    let started = std::time::Instant::now();
    let max_wait = Duration::from_millis(max_wait_ms);
    loop {
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return Some(Lock { _file: file });
        }
        let elapsed = started.elapsed();
        if elapsed >= max_wait {
            return None;
        }
        std::thread::sleep((max_wait - elapsed).min(Duration::from_millis(50)));
    }
}

/// Windows counterpart of the unix `acquire`, with the identical contract:
/// same signature, same bounded retry, same lock-or-skip `None`.
///
/// Contention and failure are distinguished the same way the unix path
/// distinguishes them: a sharing violation means somebody holds the lock and
/// is retried; anything else (bad path, denied) is not contention and
/// returns immediately, exactly as a failed `open` does on unix.
#[cfg(windows)]
pub fn acquire(path: &Path, max_wait_ms: u64, _stale_secs: u64) -> Option<Lock> {
    use std::os::windows::fs::OpenOptionsExt as _;
    /// The Win32 error a `FILE_SHARE_NONE` open returns while another handle
    /// is outstanding.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    /// Its byte-range sibling, returned by some filesystem filters.
    const ERROR_LOCK_VIOLATION: i32 = 33;

    let started = std::time::Instant::now();
    let max_wait = Duration::from_millis(max_wait_ms);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => return Some(Lock { _file: file }),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                ) => {}
            Err(_) => return None,
        }
        let elapsed = started.elapsed();
        if elapsed >= max_wait {
            return None;
        }
        std::thread::sleep((max_wait - elapsed).min(Duration::from_millis(50)));
    }
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
        // the double-steal race. The mtime is aged BEFORE anyone holds the
        // file: a held file cannot be reopened at all on Windows, which is
        // the same guarantee stated one layer lower.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        std::fs::write(&p, "").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        drop(f);
        let held = acquire(&p, 100, 1).unwrap();
        assert!(acquire(&p, 200, 1).is_none(), "no steal path may exist");
        drop(held);
        assert!(acquire(&p, 100, 1).is_some(), "an UNHELD old file is simply acquirable");
    }

    #[test]
    fn reacquire_keeps_the_same_file() {
        // Proves lock-file permanence: no remove-and-recreate cycle, so every
        // contender always claims the SAME file (the precondition mutual
        // exclusion needs on either backend).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let l1 = acquire(&p, 100, 5).unwrap();
        drop(l1);
        let id1 = file_identity(&p);
        let l2 = acquire(&p, 100, 5).unwrap();
        drop(l2);
        assert_eq!(file_identity(&p), id1);
    }

    #[cfg(unix)]
    fn file_identity(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(p).unwrap().ino()
    }

    /// Windows std exposes no stable inode; the creation timestamp changes on
    /// a remove-and-recreate cycle and answers the same question here.
    #[cfg(windows)]
    fn file_identity(p: &Path) -> u64 {
        use std::os::windows::fs::MetadataExt as _;
        std::fs::metadata(p).unwrap().creation_time()
    }
}
