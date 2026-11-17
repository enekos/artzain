//! The ownership lock for a running `up`. It keeps two `artzain up`
//! invocations from fighting over the same ports and state file, and it lets
//! `down` prove the target pid is the live owner before sending SIGTERM.
//!
//! The lock is a `flock(2)` held on `.artzain/lock` for the life of `up`. The
//! kernel drops it when the process dies, so a crashed owner never leaves a
//! stale lock, and a recycled pid can never pass for the owner: only a live
//! holder of the lock is believed.

use crate::paths::{ensure_private_dir, open_private_rw};
use crate::sys;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "lock";
const MAX_LOCK_BYTES: u64 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Owner {
    pid: u32,
    started_at: u64,
}

/// A held lock. Dropping it releases the lock and removes the file.
#[derive(Debug)]
pub struct Lock {
    /// PID of the `artzain up` process that owns this cluster.
    pub pid: u32,
    /// Unix epoch seconds when the lock was acquired.
    pub started_at: u64,
    path: PathBuf,
    file: File,
}

impl Lock {
    pub fn path(base_dir: &Path) -> PathBuf {
        base_dir.join(".artzain").join(LOCK_FILE)
    }

    /// Take the lock, or fail if a live process holds it.
    pub fn acquire(base_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(base_dir);
        if let Some(dir) = path.parent() {
            ensure_private_dir(dir)?;
        }
        loop {
            let mut file = open_private_rw(&path)
                .map_err(|e| anyhow::anyhow!("opening lock file {}: {e}", path.display()))?;
            if !sys::try_flock(&file, true)? {
                let holder = read_owner(&mut file)
                    .map(|o| o.pid.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                anyhow::bail!(
                    "another artzain process is already running for this manifest (pid {holder})"
                );
            }
            if !same_file(&file, &path) {
                continue;
            }
            let owner = Owner {
                pid: std::process::id(),
                started_at: now_unix(),
            };
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(serde_json::to_string(&owner)?.as_bytes())?;
            file.sync_all()?;
            return Ok(Self {
                pid: owner.pid,
                started_at: owner.started_at,
                path,
                file,
            });
        }
    }

    /// The pid of the live owner, or `None` if nobody holds the lock.
    pub fn live_owner(base_dir: &Path) -> anyhow::Result<Option<u32>> {
        let path = Self::path(base_dir);
        let mut file = match crate::paths::open_private_read(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("opening lock file {}: {e}", path.display()),
        };
        if sys::try_flock(&file, false)? {
            return Ok(None);
        }
        Ok(read_owner(&mut file).map(|o| o.pid))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if same_file(&self.file, &self.path) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_owner(file: &mut File) -> Option<Owner> {
    let mut raw = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(MAX_LOCK_BYTES).read_to_string(&mut raw).ok()?;
    serde_json::from_str(&raw).ok()
}

/// After locking, the path may have been unlinked and recreated by a racing
/// owner's drop; only a lock on the inode the path names right now counts.
fn same_file(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Send SIGTERM to the live owner of the cluster rooted at `base_dir`. Only
/// a process that currently holds the lock is signalled.
pub fn signal_owner(base_dir: &Path) -> anyhow::Result<u32> {
    let Some(pid) = Lock::live_owner(base_dir)? else {
        anyhow::bail!(
            "no running cluster holds {}",
            Lock::path(base_dir).display()
        );
    };
    let pid_i32 = i32::try_from(pid).map_err(|_| anyhow::anyhow!("invalid owner pid {pid}"))?;
    sys::signal_pid(pid_i32, libc::SIGTERM)
        .map_err(|e| anyhow::anyhow!("could not signal cluster owner pid {pid}: {e}"))?;
    Ok(pid)
}

/// SIGTERM `pid` without proving it owns the cluster — `down --force`. Still
/// refuses pid 0 and values that `kill(2)` would read as a process group.
pub fn force_signal(pid: u32) -> anyhow::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| anyhow::anyhow!("invalid owner pid {pid}"))?;
    sys::signal_pid(pid, libc::SIGTERM)
        .map_err(|e| anyhow::anyhow!("could not signal pid {pid}: {e}"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_the_first_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lock::acquire(tmp.path()).expect("first acquire should succeed");
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(Lock::live_owner(tmp.path()).unwrap(), Some(lock.pid));

        let err = Lock::acquire(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already running"), "{msg}");
        assert!(msg.contains(&format!("pid {}", lock.pid)), "{msg}");
    }

    #[test]
    fn drop_releases_and_removes_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lock::acquire(tmp.path()).unwrap();
        let path = Lock::path(tmp.path());
        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());
        assert_eq!(Lock::live_owner(tmp.path()).unwrap(), None);
        Lock::acquire(tmp.path()).expect("re-acquire after drop");
    }

    #[test]
    fn a_stale_lock_file_names_no_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Lock::path(tmp.path());
        ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"pid":1,"started_at":0}"#).unwrap();

        assert_eq!(Lock::live_owner(tmp.path()).unwrap(), None);
        assert!(signal_owner(tmp.path()).is_err());
        Lock::acquire(tmp.path()).expect("stale lock file is taken over");
    }
}
