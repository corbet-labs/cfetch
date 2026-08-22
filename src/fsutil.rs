//! Small, security-sensitive filesystem primitives shared by installers and
//! mutable state writers.

use anyhow::Context as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(
        ".{name}.cfetch-tmp.{}.{}",
        std::process::id(),
        TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            let mode = existing.as_ref().map_or(0o600, |p| p.mode() & 0o7777);
            options.mode(mode);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        // `OpenOptionsExt::mode` is still filtered through the process umask.
        // An atomic rewrite must preserve an existing 0660/0640 file exactly,
        // not silently narrow it to the daemon's current umask.
        if let Some(permissions) = existing.clone() {
            std::fs::set_permissions(&tmp, permissions)
                .with_context(|| format!("set permissions on {}", tmp.display()))?;
        }
        file.write_all(content.as_ref())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        drop(file);

        replace_file(&tmp, &target)
            .with_context(|| format!("replace {} with {}", target.display(), tmp.display()))?;
        sync_parent(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn replace_file(tmp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, target)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let tmp: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // `std::fs::rename` does not replace an existing file on Windows. The
    // installer is an upsert, so use the platform's atomic replacement API.
    let ok = unsafe {
        MoveFileExW(
            tmp.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
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

    #[cfg(unix)]
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
