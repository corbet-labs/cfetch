//! Small, security-sensitive filesystem primitives shared by installers and
//! mutable state writers.

use anyhow::Context as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Atomically replaces `path` without replacing a symlink that the user or a
/// declarative manager owns. Existing permissions are retained; a new file is
/// private by default because agent configs may contain literal credentials.
pub fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> anyhow::Result<()> {
    let target = resolved_write_target(path)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let existing = match std::fs::metadata(&target) {
        Ok(m) => Some(m.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("stat {}", target.display())),
    };
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}.cfetch-tmp."))
        .tempfile_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    // NamedTempFile is private by default. An atomic rewrite must preserve an
    // existing 0660/0640 file exactly, not narrow it to the process umask.
    if let Some(permissions) = existing {
        tmp.as_file()
            .set_permissions(permissions)
            .with_context(|| format!("set permissions on {}", tmp.path().display()))?;
    }
    tmp.write_all(content.as_ref())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("sync {}", tmp.path().display()))?;

    // tempfile supplies the platform-specific atomic replacement behind a
    // safe API, including replacing an existing file on Windows.
    tmp.persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", target.display()))?;
    sync_parent(parent);
    Ok(())
}

fn resolved_write_target(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("resolve symlink {}", path.display())),
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) {
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn preserves_symlink_and_target_mode() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("managed-config");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();

        atomic_write(&link, "new").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn new_files_are_private_and_regular_modes_survive() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new.json");
        atomic_write(&new, "secret").unwrap();
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let existing = dir.path().join("existing.json");
        std::fs::write(&existing, "old").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o660)).unwrap();
        atomic_write(&existing, "new").unwrap();
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o660
        );
    }
}
