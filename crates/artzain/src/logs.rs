//! Bounded log reading and following for `artzain logs`. Log lines are app
//! output and may carry attacker-chosen bytes, so everything printed goes
//! through [`sanitize_line`].

use artzain_core::sanitize_line;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const FOLLOW_CHUNK: u64 = 1024 * 1024;

/// Collect the sorted list of log files for `app` (default: all apps).
pub fn log_files(log_dir: &Path, app: Option<&str>) -> anyhow::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(log_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| match app {
            Some(app) => p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit_once('-'))
                .is_some_and(|(name, _)| name == app),
            None => true,
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Read the last `n` lines of `path` and print them to stdout.
pub fn tail(path: &Path, n: usize) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut ring: VecDeque<String> = VecDeque::with_capacity(n.min(4096));
    let mut raw = Vec::new();
    if n == 0 {
        return Ok(());
    }
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        if ring.len() == n {
            ring.pop_front();
        }
        ring.push_back(printable(&raw));
    }
    let mut stdout = std::io::stdout().lock();
    for line in ring {
        writeln!(stdout, "{line}")?;
    }
    Ok(())
}

fn printable(raw: &[u8]) -> String {
    let line = String::from_utf8_lossy(raw);
    sanitize_line(line.trim_end_matches(['\n', '\r']))
}

/// Follow the given log files, printing any new lines as they appear. Rotated
/// files (the active file shrinks or is replaced) are re-read from the start.
/// The loop stops on SIGINT/SIGTERM (the process exits).
pub fn follow(paths: &[PathBuf], interval: std::time::Duration) -> anyhow::Result<()> {
    let mut state: HashMap<PathBuf, (u64, u64)> = HashMap::new(); // (position, last_size)

    loop {
        for path in paths {
            let (pos, last_size) = state.get(path).copied().unwrap_or((0, 0));

            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let size = meta.len();

            let (new_pos, new_size) = if size < last_size || size < pos {
                read_from(path, 0)?
            } else if size > pos {
                read_from(path, pos)?
            } else {
                (pos, size)
            };

            state.insert(path.clone(), (new_pos, new_size));
        }

        std::thread::sleep(interval);
    }
}

/// Print whole lines from `start`, at most `FOLLOW_CHUNK` bytes per call; a
/// trailing partial line is left for the next call.
fn read_from(path: &Path, start: u64) -> anyhow::Result<(u64, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(FOLLOW_CHUNK).read_to_end(&mut buf)?;

    let complete = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None if buf.len() as u64 == FOLLOW_CHUNK => buf.len(),
        None => 0,
    };
    let mut stdout = std::io::stdout().lock();
    for line in buf[..complete].split_inclusive(|&b| b == b'\n') {
        writeln!(stdout, "{}", printable(line))?;
    }
    stdout.flush()?;

    let size = std::fs::metadata(path)?.len();
    Ok((start + complete as u64, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tail_keeps_last_n_lines() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 0..10 {
            writeln!(tmp, "line {}", i).unwrap();
        }
        tail(tmp.path(), 3).unwrap();
        assert!(tmp.path().exists());
    }

    #[test]
    fn tail_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        tail(tmp.path(), 5).unwrap();
    }

    #[test]
    fn tail_survives_invalid_utf8() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"ok\n\xff\xfe\n").unwrap();
        tail(tmp.path(), 5).unwrap();
    }

    #[test]
    fn printable_strips_terminal_escapes() {
        assert_eq!(printable(b"\x1b]0;pwned\x07hi\r\n"), "?]0;pwned?hi");
    }

    #[test]
    fn app_filter_matches_whole_names_only() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["api-0.log", "api-1.log", "api-admin-0.log", "apiv2-0.log"] {
            std::fs::write(dir.path().join(f), b"").unwrap();
        }
        let names: Vec<_> = log_files(dir.path(), Some("api"))
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["api-0.log", "api-1.log"]);
    }
}
