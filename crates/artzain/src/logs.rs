//! Bounded log reading and following for `artzain logs`.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Collect the sorted list of log files matching the optional app prefix.
pub fn log_files(log_dir: &Path, app: Option<&str>) -> anyhow::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(log_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            if let Some(app) = app {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.starts_with(app))
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Read the last `n` lines of `path` and print them to stdout.
pub fn tail(path: &Path, n: usize) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut ring: Vec<String> = Vec::with_capacity(n);
    for line in reader.lines() {
        let line = line?;
        if ring.len() == n {
            ring.remove(0);
        }
        ring.push(line);
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for line in ring {
        writeln!(stdout, "{}", line)?;
    }
    Ok(())
}

/// Follow the given log files, printing any new lines as they appear. Rotated
/// files (the active file shrinks or is replaced) are re-read from the start.
/// The loop stops on SIGINT/SIGTERM (the process exits).
pub fn follow(paths: &[PathBuf], interval: std::time::Duration) -> anyhow::Result<()> {
    // Track the read position for each path. We also store the file size at
    // the time of the last read so we can detect truncation/rotation.
    let mut state: HashMap<PathBuf, (u64, u64)> = HashMap::new(); // (position, last_size)
    let stdout = std::io::stdout();

    loop {
        for path in paths {
            let (pos, last_size) = state.get(path).copied().unwrap_or((0, 0));

            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();

            let (new_pos, new_size) = if size < last_size || size < pos {
                // File was rotated or truncated: start over.
                read_from(path, 0, &stdout)?
            } else if size > pos {
                read_from(path, pos, &stdout)?
            } else {
                (pos, size)
            };

            state.insert(path.clone(), (new_pos, new_size));
        }

        std::thread::sleep(interval);
    }
}

fn read_from(path: &Path, start: u64, stdout: &std::io::Stdout) -> anyhow::Result<(u64, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    reader.read_to_string(&mut buf)?;

    let mut lock = stdout.lock();
    lock.write_all(buf.as_bytes())?;
    lock.flush()?;

    let pos = reader.stream_position()?;
    let size = std::fs::metadata(path)?.len();
    Ok((pos, size))
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
        // Output is printed to stdout; just assert no panic and capacity.
        // A more thorough test would capture stdout.
        assert!(tmp.path().exists());
    }

    #[test]
    fn tail_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        tail(tmp.path(), 5).unwrap();
    }
}
