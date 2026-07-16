//! Low-level process control: spawn a single app instance in its own process
//! group, stream its logs with a colored `[app/replica]` prefix, and signal /
//! reap it. Lifted in spirit from laino's supervisor, narrowed to one instance
//! — the reconciler owns the fleet.

use crate::manifest::Limits;
use crate::paths::{open_private_log_file, open_private_log_file_sync};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const PALETTE: &[&str] = &["36", "32", "33", "35", "34", "96", "92", "93", "95", "94"];

/// A stable color per app name so all replicas of an app share a hue.
pub fn color_for(app: &str) -> &'static str {
    let sum: usize = app.bytes().map(|b| b as usize).sum();
    PALETTE[sum % PALETTE.len()]
}

fn tag(app: &str, replica: u32, color: &str) -> String {
    let label = format!("{app}/{replica}");
    format!("\x1b[{color}m[{label:<14}]\x1b[0m")
}

/// Everything needed to launch one instance. The reconciler builds this from
/// the manifest `App` plus the assigned replica index / port.
pub struct Spawn<'a> {
    pub app: &'a str,
    pub replica: u32,
    pub bin: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    /// If set, stdout and stderr are also appended to this file.
    pub log_path: Option<PathBuf>,
    /// Max size of a single log file before rotation. `0` disables rotation.
    pub log_max_bytes: u64,
    /// Number of log files to keep (current + backups). Must be >= 1.
    pub log_keep: u32,
    /// Resolved Unix uid to drop to before exec. On non-Unix this is ignored
    /// because the manifest fails to load when user/group is declared.
    pub uid: Option<u32>,
    /// Resolved Unix gid to drop to before exec.
    pub gid: Option<u32>,
    /// Optional resource limits to apply before exec.
    pub limits: Option<Limits>,
}

/// A live child plus the bookkeeping needed to signal and label it.
pub struct Handle {
    pub child: Child,
    pub pgid: i32,
}

/// Spawn the instance, with a fully constructed env (parent env is cleared
/// first), in its own process group so we can signal the whole tree at
/// teardown. On Unix, `pre_exec` drops privileges and applies resource limits
/// before the binary image is replaced. Logs are streamed on background tasks
/// with a colored prefix.
pub fn spawn(s: &Spawn) -> anyhow::Result<Handle> {
    let color = color_for(s.app);
    let tag = tag(s.app, s.replica, color);

    let mut cmd = Command::new(s.bin);
    cmd.args(s.args)
        .current_dir(s.cwd)
        .kill_on_drop(true)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (k, v) in s.env {
        cmd.env(k, v);
    }

    // Privilege drop and rlimits run in the child after fork and before exec.
    // Only async-signal-safe functions are used inside this closure.
    #[cfg(unix)]
    {
        let uid = s.uid;
        let gid = s.gid;
        let limits = s.limits.clone();
        unsafe {
            cmd.pre_exec(move || {
                if let Some(gid) = gid {
                    if libc::setgroups(0, std::ptr::null()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::setgid(gid as libc::gid_t) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some(uid) = uid {
                    if libc::setuid(uid as libc::uid_t) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some(ref limits) = limits {
                    if let Some(nofile) = limits.open_files {
                        let mut rl: libc::rlimit = std::mem::zeroed();
                        rl.rlim_cur = nofile;
                        rl.rlim_max = nofile;
                        if libc::setrlimit(libc::RLIMIT_NOFILE, &rl) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    if let Some(memory_mb) = limits.memory_mb {
                        let bytes = memory_mb * 1024 * 1024;
                        let mut rl: libc::rlimit = std::mem::zeroed();
                        rl.rlim_cur = bytes;
                        rl.rlim_max = bytes;
                        if libc::setrlimit(libc::RLIMIT_AS, &rl) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "app `{}` replica {}: spawning {}: {e}",
            s.app,
            s.replica,
            s.bin.display()
        )
    })?;
    let pgid = child.id().map(|pid| pid as i32).unwrap_or(0);
    stream_logs(
        &tag,
        child.stdout.take(),
        child.stderr.take(),
        s.log_path.clone(),
        s.log_max_bytes,
        s.log_keep,
    );
    Ok(Handle { child, pgid })
}

/// SIGTERM the instance's process group. Non-blocking: the reconciler polls
/// for exit and escalates via [`kill`] if the grace period lapses.
pub fn terminate(handle: &Handle) {
    signal_group(handle.pgid, libc::SIGTERM);
}

/// SIGKILL the instance's process group — the escalation when SIGTERM was
/// ignored past the grace period.
pub fn kill(handle: &Handle) {
    signal_group(handle.pgid, libc::SIGKILL);
}

/// Negative pid signals the whole process group (the binary plus any children
/// it forked), so nothing is orphaned to keep holding a port.
pub(crate) fn signal_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        unsafe { libc::kill(-pgid, signal) };
    }
}

/// Check whether a process is still alive. Returns `false` for invalid or
/// non-existent pids.
#[cfg(unix)]
pub(crate) fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

#[cfg(not(unix))]
pub(crate) fn is_pid_alive(_pid: i32) -> bool {
    false
}

/// Check whether a process group still exists. Returns `false` for invalid or
/// non-existent groups.
#[cfg(unix)]
pub(crate) fn is_group_alive(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(-pgid, 0) };
    if rc == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    // EPERM means the group exists but we lack permission to signal it; treat
    // as alive so we don't declare a still-running orphan "done".
    errno != libc::ESRCH
}

#[cfg(not(unix))]
pub(crate) fn is_group_alive(_pgid: i32) -> bool {
    false
}

/// PIDs listening on a TCP port, via `lsof`. Empty if none — or if `lsof` is
/// unavailable, in which case a collision surfaces later as the child's own
/// bind error rather than a false "port free".
pub fn port_listeners(port: u16) -> Vec<i32> {
    let Ok(out) = std::process::Command::new("lsof")
        .args(["-nP", "-t"])
        .arg(format!("-iTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect()
}

fn stream_logs(
    tag: &str,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    log_path: Option<PathBuf>,
    log_max_bytes: u64,
    log_keep: u32,
) {
    let Some(path) = log_path else {
        // No log file requested: print to console only.
        if let Some(out) = stdout {
            let tag = tag.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    println!("{tag} {line}");
                }
            });
        }
        if let Some(err) = stderr {
            let tag = tag.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("{tag} {line}");
                }
            });
        }
        return;
    };

    // Serialize stdout+stderr writes through a single writer task so rotation
    // is atomic and the cap is respected across both streams. Open the file
    // synchronously here so callers can rely on it existing once `spawn` returns.
    let initial_file = match open_private_log_file_sync(&path) {
        Ok(f) => Some(tokio::fs::File::from_std(f)),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not open log file");
            None
        }
    };
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let path_clone = path.clone();
    tokio::spawn(async move {
        log_writer(rx, path_clone, initial_file, log_max_bytes, log_keep).await;
    });

    if let Some(out) = stdout {
        let tag = tag.to_string();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("{tag} {line}");
                let _ = tx.send(line);
            }
        });
    }
    if let Some(err) = stderr {
        let tag = tag.to_string();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{tag} {line}");
                let _ = tx.send(line);
            }
        });
    }
}

async fn log_writer(
    mut rx: mpsc::UnboundedReceiver<String>,
    path: PathBuf,
    mut file: Option<tokio::fs::File>,
    max_bytes: u64,
    keep: u32,
) {
    if keep == 0 {
        return;
    }
    let mut size = if let Some(f) = &file {
        f.metadata().await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    while let Some(line) = rx.recv().await {
        let line_len = line.len() as u64 + 1; // + newline

        if max_bytes > 0 && size + line_len > max_bytes {
            file = None;
            if let Err(e) = rotate_log(&path, keep).await {
                tracing::warn!(path = %path.display(), error = %e, "could not rotate log file");
                continue;
            }
            match open_private_log_file(&path).await {
                Ok(f) => {
                    size = 0;
                    file = Some(f);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not open log file");
                    continue;
                }
            }
        }

        if let Some(f) = file.as_mut() {
            let _ = f.write_all(line.as_bytes()).await;
            let _ = f.write_all(b"\n").await;
            size += line_len;
        }
    }
}

/// Rotate existing log files: `path` -> `path.1`, `path.1` -> `path.2`, ...,
/// deleting `path.{keep-1}`.
async fn rotate_log(path: &Path, keep: u32) -> std::io::Result<()> {
    if keep <= 1 {
        // No backups; just truncate the current file.
        let _ = tokio::fs::remove_file(path).await;
        return Ok(());
    }

    // Remove the oldest backup.
    let oldest = path.with_extension(format!("log.{}", keep - 1));
    let _ = tokio::fs::remove_file(&oldest).await;

    // Shift younger backups up.
    for i in (1..keep - 1).rev() {
        let src = path.with_extension(format!("log.{}", i));
        let dst = path.with_extension(format!("log.{}", i + 1));
        if tokio::fs::metadata(&src).await.is_ok() {
            tokio::fs::rename(&src, &dst).await?;
        }
    }

    // Current -> .1
    let backup = path.with_extension("log.1");
    tokio::fs::rename(path, &backup).await?;
    Ok(())
}
