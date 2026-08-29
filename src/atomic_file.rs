use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub(crate) enum DurableAtomicWriteError {
    DestinationUnchanged(anyhow::Error),
    DurabilityUncertain(anyhow::Error),
}

impl DurableAtomicWriteError {
    pub(crate) fn into_parts(self) -> (anyhow::Error, bool) {
        match self {
            Self::DestinationUnchanged(error) => (error, false),
            Self::DurabilityUncertain(error) => (error, true),
        }
    }
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Durably replaces one file without exposing a truncated or partially written destination.
///
/// The temporary file lives beside the destination so the final rename is atomic. Its contents
/// are flushed before the rename; on Unix the containing directory is then flushed when the
/// filesystem supports it. The function returns an error only while the destination is unchanged.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent_directory = replace_atomic(path, contents)?;
    sync_parent_directory_after_commit(parent_directory);
    Ok(())
}

/// Durably replaces one file and reports whether a failure happened before or after rename.
///
/// A post-rename directory-sync failure means the new file is visible but its survival across a
/// host crash is uncertain. Callers coordinating other durable state must keep their fail-closed
/// guard in place rather than treating that outcome as an unchanged destination.
pub(crate) fn write_atomic_durable(
    path: &Path,
    contents: &[u8],
) -> std::result::Result<(), DurableAtomicWriteError> {
    let parent_directory =
        replace_atomic(path, contents).map_err(DurableAtomicWriteError::DestinationUnchanged)?;
    if let Some(parent_directory) = parent_directory {
        parent_directory.sync_all().map_err(|error| {
            DurableAtomicWriteError::DurabilityUncertain(anyhow::Error::from(error).context(
                format!(
                    "{} was replaced but its directory could not be durably flushed",
                    path.display()
                ),
            ))
        })?;
    }
    Ok(())
}

fn replace_atomic(path: &Path, contents: &[u8]) -> Result<Option<File>> {
    let destination = resolve_destination(path)?;
    let temp_path = temp_path_for(&destination)?;
    let result = (|| {
        // Open the directory before changing the destination so every reportable failure remains
        // a pre-commit failure. Once rename succeeds, callers must treat the write as committed.
        let parent_directory = open_parent_directory(&destination)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        preserve_permissions(&file, &temp_path, &destination)?;
        file.write_all(contents)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush {}", temp_path.display()))?;
        drop(file);
        atomic_replace(&temp_path, &destination).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                destination.display(),
                temp_path.display()
            )
        })?;
        Ok(parent_directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn resolve_destination(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .with_context(|| format!("failed to resolve symbolic link {}", path.display())),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect destination {}", path.display()))
        }
    }
}

fn temp_path_for(path: &Path) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}.{counter}",
        std::process::id(),
        now
    )))
}

#[cfg(unix)]
fn preserve_permissions(file: &File, temp_path: &Path, path: &Path) -> Result<()> {
    let mode = match fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode(),
        Err(error) if error.kind() == ErrorKind::NotFound => 0o600,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read metadata for {}", path.display()));
        }
    };
    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", temp_path.display()))
}

#[cfg(not(unix))]
fn preserve_permissions(file: &File, temp_path: &Path, path: &Path) -> Result<()> {
    if path.exists() {
        let permissions = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?
            .permissions();
        file.set_permissions(permissions)
            .with_context(|| format!("failed to set permissions on {}", temp_path.display()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(temp_path: &Path, path: &Path) -> Result<()> {
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(temp_path: &Path, path: &Path) -> Result<()> {
    use std::io;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            lp_existing_file_name: *const u16,
            lp_new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let old_path = temp_path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let new_path = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            old_path.as_ptr(),
            new_path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn open_parent_directory(path: &Path) -> Result<Option<File>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .map(Some)
        .with_context(|| format!("failed to open directory {}", parent.display()))
}

#[cfg(not(unix))]
fn open_parent_directory(_path: &Path) -> Result<Option<File>> {
    Ok(None)
}

fn sync_parent_directory_after_commit(parent_directory: Option<File>) {
    if let Some(parent_directory) = parent_directory {
        // The destination has already been atomically replaced. A directory-sync failure cannot
        // be returned as an ordinary write failure without causing callers to perform an invalid
        // rollback against content that is already committed. Filesystems that support directory
        // fsync still get the stronger crash-durability guarantee.
        let _ = parent_directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn atomic_write_replaces_complete_contents() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sing-box-tui-atomic-{suffix}.json"));
        fs::write(&path, b"old").expect("seed file writes");

        write_atomic(&path, b"new\n").expect("atomic write succeeds");

        assert_eq!(fs::read(&path).expect("file reads"), b"new\n");
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_symbolic_link_and_updates_its_target() {
        use std::os::unix::fs::symlink;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let target = std::env::temp_dir().join(format!("sing-box-tui-target-{suffix}.json"));
        let link = std::env::temp_dir().join(format!("sing-box-tui-link-{suffix}.json"));
        fs::write(&target, b"old").expect("target writes");
        symlink(target.file_name().expect("target has file name"), &link).expect("symlink creates");

        write_atomic(&link, b"new\n").expect("atomic symlink write succeeds");

        assert!(
            fs::symlink_metadata(&link)
                .expect("link metadata reads")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).expect("target reads"), b"new\n");
        let _ = fs::remove_file(link);
        let _ = fs::remove_file(target);
    }
}
