//! Filesystem helpers for creating private directories and files under the
//! `.artzain` control-plane directory. Everything artzain writes is owned by
//! the service account and must not be readable by other users.

use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Create `dir` and set its permissions to `0700` (owner-only).
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically replace `path` with `contents`, mode `0600` (owner-only).
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let tmp = temp_sibling(path);
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
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(tmp)?;
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

#[cfg(unix)]
fn sync_parent_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) {}

/// Async version of log-file opening: creates parent dirs with `0700` and
/// opens the file with `0600`.
pub async fn open_private_log_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
        #[cfg(unix)]
        {
            let dir = dir.to_path_buf();
            tokio::task::spawn_blocking(move || {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            })
            .await??;
        }
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    opts.open(path).await
}

/// Synchronous version of log-file opening for use before the async runtime
/// has a chance to schedule the writer task.
pub fn open_private_log_file_sync(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn replaces_contents_and_keeps_owner_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_private_file(&path, b"first").unwrap();
        write_private_file(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
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
}
