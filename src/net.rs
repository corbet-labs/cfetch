//! Network identity: this host's persistent iroh keypair.
//!
//! The secret key IS the host — its public half is the endpoint id other
//! hosts grant slices to. Two invariants follow, and both are load-bearing:
//!
//! 1. **It never goes in the brain tree.** Everything else cfetch derives is
//!    shared on purpose — vectors are content, not machine state. The identity
//!    key is the exact opposite: a tree shared over NFS would hand every host
//!    the same identity, so a grant to one machine would silently be a grant
//!    to all of them. It lives in the per-host state directory.
//! 2. **A key that cannot be read is an error, never a fresh key.** Silently
//!    regenerating would mint a new identity, and every grant anyone ever made
//!    to this host points at the old one — the machine would look like a
//!    stranger to its own peers, with nothing in the logs to say why.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Filename of the identity key inside the state directory.
const KEY_FILE: &str = "endpoint.key";

/// Raw ed25519 secret key length.
const KEY_LEN: usize = 32;

pub fn key_path(state_dir: &Path) -> PathBuf {
    state_dir.join(KEY_FILE)
}

/// Loads this host's identity, creating it on first use.
///
/// The file holds the 32 raw key bytes — not hex, not a wrapper format — so
/// there is nothing to version and nothing to misparse.
pub fn load_or_create(state_dir: &Path) -> anyhow::Result<iroh::SecretKey> {
    let path = key_path(state_dir);
    match read_key(&path) {
        Ok(sk) => return Ok(sk),
        Err(e) if e.downcast_ref::<std::io::Error>().is_none_or(|io| {
            io.kind() != std::io::ErrorKind::NotFound
        }) => return Err(e),
        Err(_) => {}
    }

    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;
    let lock_path = state_dir.join("endpoint.lock");
    let _lock = crate::lockfile::acquire(&lock_path, 2_000, 0)
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for {}", lock_path.display()))?;

    // The winner may have created the key while this process waited.
    match read_key(&path) {
        Ok(sk) => return Ok(sk),
        Err(e) if e.downcast_ref::<std::io::Error>().is_none_or(|io| {
            io.kind() != std::io::ErrorKind::NotFound
        }) => return Err(e),
        Err(_) => {}
    }

    let sk = iroh::SecretKey::generate();
    write_key(&path, &sk)?;
    // Always return the persisted winner, never an in-memory candidate.
    read_key(&path)
}

fn read_key(path: &Path) -> anyhow::Result<iroh::SecretKey> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        anyhow::ensure!(
            mode == 0o600,
            "{} has mode {mode:04o}, expected 0600; refusing to use an exposed identity key",
            path.display()
        );
    }
    let bytes: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "{} is {} bytes, not {KEY_LEN}: refusing to mint a new identity over it — \
             every grant made to this host names the OLD one. Move the file aside \
             deliberately if you mean to become a different host.",
            path.display(),
            bytes.len()
        )
    })?;
    Ok(iroh::SecretKey::from_bytes(&bytes))
}

/// Writes the key 0600, creating the parent directory. The mode is set before
/// the bytes land, so the key is never briefly world-readable.
fn write_key(path: &Path, sk: &iroh::SecretKey) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = match opts.open(path) {
        Ok(f) => f,
        Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
    };
    use std::io::Write as _;
    f.write_all(&sk.to_bytes()).with_context(|| format!("write {}", path.display()))?;
    f.sync_all().with_context(|| format!("sync {}", path.display()))?;
    Ok(())
}

/// This host's endpoint id — the public half, safe to print and to hand out.
pub fn endpoint_id(state_dir: &Path) -> anyhow::Result<iroh::EndpointId> {
    Ok(load_or_create(state_dir)?.public())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_created_once_and_then_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = endpoint_id(dir.path()).unwrap();
        let second = endpoint_id(dir.path()).unwrap();
        assert_eq!(first, second, "a host must not change identity between calls");
        assert!(key_path(dir.path()).exists());
    }

    #[test]
    fn concurrent_first_use_has_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::sync::Arc::new(dir.path().to_path_buf());
        let mut workers = Vec::new();
        for _ in 0..32 {
            let root = root.clone();
            workers.push(std::thread::spawn(move || endpoint_id(&root).unwrap()));
        }
        let ids: std::collections::HashSet<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 1, "every successful caller must return the persisted identity");
    }

    #[test]
    fn two_hosts_are_two_identities() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(endpoint_id(a.path()).unwrap(), endpoint_id(b.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        endpoint_id(dir.path()).unwrap();
        let mode = std::fs::metadata(key_path(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the identity key is a secret, got {:o}", mode & 0o777);
    }

    #[test]
    fn a_damaged_key_is_refused_rather_than_replaced() {
        // Regenerating would mint a new identity and quietly orphan every
        // grant that names the old one.
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());
        std::fs::write(&path, b"too short").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let e = endpoint_id(dir.path()).unwrap_err().to_string();
        assert!(e.contains("refusing to mint a new identity"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn an_exposed_existing_key_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());
        std::fs::write(&path, iroh::SecretKey::generate().to_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = endpoint_id(dir.path()).unwrap_err().to_string();
        assert!(e.contains("expected 0600"), "{e}");
    }

    #[test]
    fn the_identity_never_lands_in_the_brain_tree() {
        // The state dir is per-host; the brain tree is shared. One identity
        // per host is the entire point of granting a slice to a host.
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        endpoint_id(state.path()).unwrap();
        let stray: Vec<_> = walk(brain.path());
        assert!(stray.is_empty(), "identity leaked into the tree: {stray:?}");
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                out.push(e.path());
            }
        }
        out
    }
}
