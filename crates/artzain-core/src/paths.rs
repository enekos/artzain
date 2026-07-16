//! Filesystem helpers for creating private directories and files under the
//! `.artzain` control-plane directory. Everything artzain writes is owned by
//! the service account and must not be readable by other users.

use std::io::Write;
use std::path::Path;

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

/// Create/truncate `path` with mode `0600` (owner-only) and write `contents`.
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    Ok(())
}

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
