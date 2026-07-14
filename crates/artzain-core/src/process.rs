//! Low-level process control: spawn a single app instance in its own process
//! group, stream its logs with a colored `[app/replica]` prefix, and signal /
//! reap it. Lifted in spirit from laino's supervisor, narrowed to one instance
//! — the reconciler owns the fleet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

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
}

/// A live child plus the bookkeeping needed to signal and label it.
pub struct Handle {
    pub child: Child,
    pub pgid: i32,
}

/// Spawn the instance, inheriting the parent env and layering `env` on top,
/// in its own process group so we can signal the whole tree at teardown.
/// Logs are streamed on background tasks with a colored prefix.
pub fn spawn(s: &Spawn) -> anyhow::Result<Handle> {
    let color = color_for(s.app);
    let tag = tag(s.app, s.replica, color);

    let mut cmd = Command::new(s.bin);
    cmd.args(s.args)
        .current_dir(s.cwd)
        .kill_on_drop(true)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in s.env {
        cmd.env(k, v);
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
fn signal_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        unsafe { libc::kill(-pgid, signal) };
    }
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
) {
    if let Some(out) = stdout {
        let tag = tag.to_string();
        let log_path = log_path.clone();
        tokio::spawn(async move {
            let mut file = open_log_file(log_path.as_deref()).await;
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("{tag} {line}");
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(line.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                }
            }
        });
    }
    if let Some(err) = stderr {
        let tag = tag.to_string();
        tokio::spawn(async move {
            let mut file = open_log_file(log_path.as_deref()).await;
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{tag} {line}");
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(line.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                }
            }
        });
    }
}

async fn open_log_file(path: Option<&Path>) -> Option<tokio::fs::File> {
    let path = path?;
    if let Some(dir) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            tracing::warn!(dir = %dir.display(), error = %e, "could not create log directory");
            return None;
        }
    }
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not open log file");
            None
        }
    }
}
