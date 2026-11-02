//! Private, crash-safe file handling for the host's data directory.
//!
//! On Unix, files are created 0600 and directories 0700. On Windows, files inherit the ACL of the
//! data directory; operators must place it in a location only the service account can read.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const LOCK_FILE: &str = "host.lock";

/// Creates the data directory if needed and restricts it to the current user.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Replaces `path` with `bytes` so readers observe either the old or the new file, never a mix.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = temp_path(path);
    let result = write_then_rename(&temp, path, dir, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_then_rename(temp: &Path, path: &Path, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_private_new(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    sync_dir(dir)
}

/// Tightens permissions on an existing secret file to owner read/write only.
pub fn restrict_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Exclusive lock on the data directory, held for the life of the process.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another iroh-host process is already using {0}")]
    InUse(PathBuf),
    #[error("could not lock {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

impl DataDirLock {
    pub fn acquire(dir: &Path) -> Result<Self, LockError> {
        let path = dir.join(LOCK_FILE);
        let file = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) => return Err(LockError::Io { path, source }),
        };
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(LockError::InUse(dir.to_path_buf())),
            Err(TryLockError::Error(source)) => Err(LockError::Io { path, source }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_private_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    path.with_file_name(format!(".{name}.{}.{unique}.tmp", std::process::id()))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn create() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "iroh-host-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TempDir;
    use super::*;

    #[test]
    fn atomic_write_replaces_contents_and_leaves_no_temp_files() {
        let dir = TempDir::create();
        let path = dir.0.join("state.json");
        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let entries: Vec<_> = fs::read_dir(&dir.0).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::create();
        let data = dir.0.join("data");
        ensure_private_dir(&data).unwrap();
        let path = data.join("identity.key");
        write_private_atomic(&path, b"x").unwrap();
        assert_eq!(
            fs::metadata(&data).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        restrict_permissions(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn second_lock_on_same_dir_is_refused() {
        let dir = TempDir::create();
        let first = DataDirLock::acquire(&dir.0).unwrap();
        assert!(matches!(
            DataDirLock::acquire(&dir.0),
            Err(LockError::InUse(_))
        ));
        drop(first);
        assert!(DataDirLock::acquire(&dir.0).is_ok());
    }
}
