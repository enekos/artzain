//! Low-level process control: spawn a single app instance in its own process
//! group, stream its logs with a colored `[app/replica]` prefix, and signal /
//! reap it. Lifted in spirit from laino's supervisor, narrowed to one instance
//! — the reconciler owns the fleet.

use crate::manifest::Limits;
use crate::paths::{open_private_log_file, open_private_log_file_sync};
use crate::sys;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Longest log line kept; the rest of an over-long line is dropped so a child
/// that never prints a newline cannot grow artzain's memory without bound.
pub const MAX_LOG_LINE: usize = 16 * 1024;
const LOG_QUEUE: usize = 1024;

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

/// Replace terminal control characters (ANSI escapes, `\r`, backspace, …)
/// with `?`, keeping tabs. App output is attacker-influenced — a request path
/// an app logs ends up here — and must not drive the operator's terminal.
pub fn sanitize_line(line: &str) -> String {
    line.chars()
        .map(|c| if c.is_control() && c != '\t' { '?' } else { c })
        .collect()
}

/// Read one `\n`-terminated line, keeping at most `MAX_LOG_LINE` bytes of it.
/// `None` at end of stream or on a read error.
pub(crate) async fn read_bounded_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<String> {
    let mut line = Vec::new();
    let mut seen_any = false;
    loop {
        let buf = reader.fill_buf().await.ok()?;
        if buf.is_empty() {
            return seen_any.then(|| String::from_utf8_lossy(&line).into_owned());
        }
        seen_any = true;
        let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], i + 1),
            None => (buf, buf.len()),
        };
        let room = MAX_LOG_LINE.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let newline = done > chunk.len();
        reader.consume(done);
        if newline {
            return Some(String::from_utf8_lossy(&line).into_owned());
        }
    }
}

/// Everything needed to launch one instance. The reconciler builds this from
/// the manifest `App` plus the assigned replica index / port.
#[derive(Debug)]
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
#[derive(Debug)]
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

    sys::drop_privileges_before_exec(&mut cmd, s.uid, s.gid, s.limits.clone());

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "app `{}` replica {}: spawning {}: {e}",
            s.app,
            s.replica,
            s.bin.display()
        )
    })?;
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .unwrap_or(0);
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
    sys::signal_group(pgid, signal);
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
    let tx = log_path.map(|path| {
        // Serialize stdout+stderr writes through a single writer task so
        // rotation is atomic and the cap is respected across both streams.
        // Open the file synchronously here so callers can rely on it existing
        // once `spawn` returns. The queue is bounded: a slow disk applies
        // backpressure to the child's pipe instead of growing memory.
        let initial_file = match open_private_log_file_sync(&path) {
            Ok(f) => Some(tokio::fs::File::from_std(f)),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not open log file");
                None
            }
        };
        let (tx, rx) = mpsc::channel::<String>(LOG_QUEUE);
        tokio::spawn(log_writer(rx, path, initial_file, log_max_bytes, log_keep));
        tx
    });
    if let Some(out) = stdout {
        tokio::spawn(pump(
            BufReader::new(out),
            tag.to_string(),
            tx.clone(),
            false,
        ));
    }
    if let Some(err) = stderr {
        tokio::spawn(pump(BufReader::new(err), tag.to_string(), tx, true));
    }
}

async fn pump<R: AsyncBufRead + Unpin>(
    mut reader: R,
    tag: String,
    tx: Option<mpsc::Sender<String>>,
    to_stderr: bool,
) {
    while let Some(line) = read_bounded_line(&mut reader).await {
        let line = sanitize_line(&line);
        if to_stderr {
            eprintln!("{tag} {line}");
        } else {
            println!("{tag} {line}");
        }
        if let Some(tx) = &tx {
            let _ = tx.send(line).await;
        }
    }
}

async fn log_writer(
    mut rx: mpsc::Receiver<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8]) -> Vec<String> {
        let mut reader = BufReader::with_capacity(7, input);
        let mut out = Vec::new();
        while let Some(line) = read_bounded_line(&mut reader).await {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn splits_lines_across_buffer_refills() {
        assert_eq!(
            lines(b"hello world\nsecond\nlast").await,
            ["hello world", "second", "last"]
        );
        assert!(lines(b"").await.is_empty());
    }

    #[tokio::test]
    async fn caps_a_line_that_never_ends() {
        let mut input = vec![b'x'; MAX_LOG_LINE * 4];
        input.extend_from_slice(b"\nnext\n");
        let got = lines(&input).await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), MAX_LOG_LINE);
        assert_eq!(got[1], "next");
    }

    #[test]
    fn sanitize_neutralises_terminal_control() {
        assert_eq!(sanitize_line("a\x1b[2Jb\tc\r\x08"), "a?[2Jb\tc??");
        assert_eq!(sanitize_line("naïve ✓"), "naïve ✓");
    }
}
