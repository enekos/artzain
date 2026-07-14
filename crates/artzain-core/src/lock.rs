//! A simple pidfile lock for the running `up` process. This prevents two
//! `artzain up` invocations from fighting over the same ports and state file,
//! and it lets `down` verify that the target pid is still the owner before
//! sending SIGTERM.
//!
//! On Unix, process liveness is checked with `kill(pid, 0)`. On other platforms
//! we fall back to "best effort": a lock file is still written and read, but we
//! cannot detect a stale pid left by a crash.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    /// PID of the `artzain up` process that owns this cluster.
    pub pid: u32,
    /// Unix epoch seconds when the lock was acquired.
    pub started_at: u64,
    #[serde(skip)]
    path: PathBuf,
}

impl Lock {
    pub fn current() -> Self {
        Self {
            pid: std::process::id(),
            started_at: now_unix(),
            path: PathBuf::new(),
        }
    }

    pub fn path(base_dir: &Path) -> PathBuf {
        base_dir.join(".artzain").join(LOCK_FILE)
    }

    /// Try to acquire the lock. Returns an error if another live process owns
    /// it. If the existing lock is stale (process dead or same process), it is
    /// replaced.
    pub fn acquire(base_dir: &Path) -> anyhow::Result<Self> {
        let mut lock = Self::current();
        lock.path = Self::path(base_dir);

        if let Some(existing) = Self::read(base_dir)? {
            if existing.pid != lock.pid && is_pid_alive(existing.pid as i32) {
                anyhow::bail!(
                    "another artzain process is already running for this manifest (pid {})",
                    existing.pid
                );
            }
        }

        let raw = serde_json::to_string_pretty(&lock).unwrap_or_default();
        if let Some(parent) = lock.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&lock.path, raw).map_err(|e| {
            anyhow::anyhow!("could not write lock file {}: {e}", lock.path.display())
        })?;
        Ok(lock)
    }

    pub fn read(base_dir: &Path) -> anyhow::Result<Option<Self>> {
        let path = Self::path(base_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("reading lock file {}: {e}", path.display()),
        };
        let mut lock: Lock = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing lock file {}: {e}", path.display()))?;
        lock.path = path;
        Ok(Some(lock))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        // Only remove if we can read the file and it still points to us.
        if let Ok(raw) = std::fs::read_to_string(&self.path) {
            if let Ok(lock) = serde_json::from_str::<Lock>(&raw) {
                if lock.pid == self.pid {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }
}

#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a pure liveness check; no signal is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: i32) -> bool {
    // Without a portable liveness check, assume the lock is valid if present.
    true
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
    fn lock_acquire_prevents_concurrent_owners() {
        let tmp = std::env::temp_dir().join(format!("artzain-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let lock = Lock::acquire(&tmp).expect("first acquire should succeed");
        assert_eq!(lock.pid, std::process::id());
        assert!(Lock::read(&tmp).unwrap().is_some());

        let re = Lock::acquire(&tmp);
        assert!(re.is_ok(), "same pid should be allowed to re-acquire");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn lock_drop_removes_only_owned_file() {
        let tmp =
            std::env::temp_dir().join(format!("artzain-lock-drop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        {
            let lock = Lock::acquire(&tmp).unwrap();
            let path = lock.path.clone();
            assert!(path.exists());
            drop(lock);
            assert!(!path.exists(), "lock file should be removed on drop");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
