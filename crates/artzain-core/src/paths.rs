//! Filesystem helpers for creating private directories and files under the
//! `.artzain` control-plane directory. Everything artzain writes is owned by
//! the service account and must not be readable by other users.
//!
//! artzain often runs as root while the manifest directory belongs to the app
//! account, so every directory is checked to be a real directory owned by us
//! (never a symlink someone swapped in) and every file is opened with
//! `O_NOFOLLOW`.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Create `dir` if missing and make sure it is a `0700` directory owned by the
/// effective user. Refuses symlinks and directories owned by anyone else.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(refused(dir, "is not a directory (symlink?)"));
    }
    if meta.uid() != crate::sys::euid() {
        return Err(refused(dir, "is owned by another user"));
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn refused(path: &Path, why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("refusing to use {}: {why}", path.display()),
    )
}

/// Atomically replace `path` with `contents`, mode `0600` (owner-only).
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let tmp = temp_sibling(path);
    let _ = std::fs::remove_file(&tmp);
    match write_then_sync(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => {
            sync_parent_dir(path);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn write_then_sync(tmp: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tmp)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("artzain"));
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn sync_parent_dir(path: &Path) {
    if let Some(dir) = path.parent()
        && let Ok(handle) = std::fs::File::open(dir)
    {
        let _ = handle.sync_all();
    }
}

/// Open a file for appending with `0600`, refusing to follow a symlink.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Open or create a file read-write with `0600` without truncating it,
/// refusing to follow a symlink.
pub fn open_private_rw(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Open an existing file read-only, refusing to follow a symlink.
pub fn open_private_read(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Async version of log-file opening: checks the parent dir and opens the file
/// with `0600`.
pub async fn open_private_log_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || open_private_log_file_sync(&path)).await??;
    Ok(tokio::fs::File::from_std(file))
}

/// Synchronous version of log-file opening for use before the async runtime
/// has a chance to schedule the writer task.
pub fn open_private_log_file_sync(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    open_private_append(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn replaces_contents_and_keeps_owner_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_private_file(&path, b"first").unwrap();
        write_private_file(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_private_file(&path, b"payload").unwrap();

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["state.json".to_string()]);
    }

    #[test]
    fn concurrent_reader_never_sees_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let quiet = vec![b'a'; 256 * 1024];
        let loud = vec![b'b'; 256 * 1024];
        write_private_file(&path, &quiet).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let partial_reads = Arc::new(AtomicUsize::new(0));

        let reader = {
            let path = path.clone();
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            let partial_reads = Arc::clone(&partial_reads);
            let len = quiet.len();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(seen) = std::fs::read(&path) {
                        reads.fetch_add(1, Ordering::Relaxed);
                        let whole = seen.len() == len
                            && (seen.iter().all(|&c| c == b'a') || seen.iter().all(|&c| c == b'b'));
                        if !whole {
                            partial_reads.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        };

        for tick in 0..200 {
            let payload = if tick % 2 == 0 { &loud } else { &quiet };
            write_private_file(&path, payload).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert!(reads.load(Ordering::Relaxed) > 0);
        assert_eq!(partial_reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn refuses_a_symlinked_control_dir() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let control = dir.path().join(".artzain");
        std::os::unix::fs::symlink(&elsewhere, &control).unwrap();

        let err = write_private_file(&control.join("state.json"), b"x").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!elsewhere.join("state.json").exists());
    }

    #[test]
    fn refuses_to_append_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"").unwrap();
        let link = dir.path().join("app-0.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(open_private_log_file_sync(&link).is_err());
    }

    #[test]
    fn tightens_a_loose_control_dir() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join(".artzain");
        std::fs::create_dir(&control).unwrap();
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o777)).unwrap();

        ensure_private_dir(&control).unwrap();
        let mode = std::fs::metadata(&control).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
