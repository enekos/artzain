//! systemd unit generator for `artzain up`.
//!
//! Outputs a unit file that runs artzain as an unprivileged service account,
//! captures stdout/stderr to journald, and applies systemd-level hardening.
//! The installed file is named `artzain@.service` so it can be enabled as a
//! template instance (e.g. `systemctl enable artzain@/srv/app/artzain.toml`); the
//! generator-produced unit uses the concrete manifest path given on the command
//! line.

use std::fmt::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Options for the generated unit.
pub struct UnitOptions<'a> {
    pub user: &'a str,
    pub group: &'a str,
    pub manifest: &'a Path,
    pub binary: &'a Path,
    pub memory_max: Option<&'a str>,
    pub stop_grace_secs: u64,
}

/// A value that is safe to put on a unit-file line: no control characters,
/// so no newline can start a directive of its own.
fn plain(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be non-empty and contain no control characters");
    }
    Ok(())
}

/// A user or group name: what `useradd` accepts.
fn account(field: &str, value: &str) -> anyhow::Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 32
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        anyhow::bail!(
            "{field} `{}` is not a valid account name",
            value.escape_debug()
        );
    }
    Ok(())
}

/// Quote one `ExecStart=` argument. systemd expands `%` specifiers and splits
/// on whitespace, so `%` is doubled and the word is double-quoted with `"`
/// and `\\` escaped.
fn exec_arg(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '%' => out.push_str("%%"),
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Generate the unit file contents.
pub fn generate_unit(options: UnitOptions) -> anyhow::Result<String> {
    account("--user", options.user)?;
    account("--group", options.group)?;
    let manifest = options
        .manifest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--manifest must be UTF-8"))?;
    let binary = options
        .binary
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--binary must be UTF-8"))?;
    plain("--manifest", manifest)?;
    plain("--binary", binary)?;
    if !options.binary.is_absolute() {
        anyhow::bail!("--binary must be an absolute path");
    }
    if let Some(mem) = options.memory_max {
        let ok = !mem.is_empty()
            && mem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '%');
        if !ok {
            anyhow::bail!("--memory-max `{}` is not a size", mem.escape_debug());
        }
    }
    let timeout = options.stop_grace_secs;
    let description = manifest.replace('%', "%%");

    let mut unit = String::new();
    writeln!(&mut unit, "[Unit]")?;
    writeln!(&mut unit, "Description=artzain fleet for {description}")?;
    writeln!(&mut unit, "After=network.target")?;
    writeln!(&mut unit)?;

    writeln!(&mut unit, "[Service]")?;
    writeln!(&mut unit, "Type=simple")?;
    writeln!(&mut unit, "User={}", options.user)?;
    writeln!(&mut unit, "Group={}", options.group)?;
    writeln!(
        &mut unit,
        "ExecStart={} -f {} up",
        exec_arg(binary),
        exec_arg(manifest)
    )?;
    writeln!(&mut unit, "Restart=on-failure")?;
    writeln!(&mut unit, "RestartSec=5")?;
    writeln!(&mut unit, "KillSignal=SIGTERM")?;
    writeln!(&mut unit, "TimeoutStopSec={timeout}")?;
    writeln!(&mut unit, "NoNewPrivileges=true")?;
    writeln!(&mut unit, "ProtectSystem=strict")?;
    writeln!(&mut unit, "ProtectHome=true")?;
    writeln!(&mut unit, "PrivateTmp=true")?;
    if let Some(mem) = options.memory_max {
        writeln!(&mut unit, "MemoryMax={mem}")?;
    }
    writeln!(&mut unit)?;

    writeln!(&mut unit, "[Install]")?;
    writeln!(&mut unit, "WantedBy=multi-user.target")?;

    Ok(unit)
}

/// Install the generated unit to `/etc/systemd/system/artzain@.service`.
pub fn install_unit(content: &str, path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            anyhow::bail!("{} is a directory", path.display());
        }
    }
    let tmp = path.with_extension("service.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)?;
    std::io::Write::write_all(&mut file, content.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    println!("wrote {}", path.display());
    println!("run: systemctl daemon-reload");
    println!("run: systemctl enable --now artzain@<instance>",);
    Ok(())
}

/// Default binary path assumed in generated units.
pub fn default_binary() -> PathBuf {
    PathBuf::from("/usr/local/bin/artzain")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_unit_contains_required_directives() {
        let opts = UnitOptions {
            user: "artzain",
            group: "artzain",
            manifest: Path::new("/srv/app/artzain.toml"),
            binary: Path::new("/usr/local/bin/artzain"),
            memory_max: Some("512M"),
            stop_grace_secs: 10,
        };
        let unit = generate_unit(opts).unwrap();
        assert!(unit.contains("User=artzain"));
        assert!(unit.contains("Group=artzain"));
        assert!(
            unit.contains(r#"ExecStart="/usr/local/bin/artzain" -f "/srv/app/artzain.toml" up"#)
        );
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("KillSignal=SIGTERM"));
        assert!(unit.contains("TimeoutStopSec=10"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("ProtectHome=true"));
        assert!(unit.contains("PrivateTmp=true"));
        assert!(unit.contains("MemoryMax=512M"));
    }

    #[test]
    fn generated_unit_omits_memory_max_when_none() {
        let opts = UnitOptions {
            user: "artzain",
            group: "artzain",
            manifest: Path::new("/srv/app/artzain.toml"),
            binary: Path::new("/usr/local/bin/artzain"),
            memory_max: None,
            stop_grace_secs: 10,
        };
        let unit = generate_unit(opts).unwrap();
        assert!(!unit.contains("MemoryMax"));
    }

    fn opts<'a>(user: &'a str, manifest: &'a Path, memory_max: Option<&'a str>) -> UnitOptions<'a> {
        UnitOptions {
            user,
            group: "artzain",
            manifest,
            binary: Path::new("/usr/local/bin/artzain"),
            memory_max,
            stop_grace_secs: 10,
        }
    }

    #[test]
    fn rejects_newline_injection() {
        let ok = Path::new("/srv/app/artzain.toml");
        assert!(generate_unit(opts("artzain\nExecStartPre=/bin/sh", ok, None)).is_err());
        assert!(generate_unit(opts("artzain", ok, Some("1G\nUser=root"))).is_err());
        let evil = Path::new("/srv/a.toml\nExecStartPre=/tmp/x");
        assert!(generate_unit(opts("artzain", evil, None)).is_err());
    }

    #[test]
    fn quotes_spaces_and_escapes_specifiers() {
        let unit = generate_unit(opts("artzain", Path::new("/srv/my app/%h.toml"), None)).unwrap();
        assert!(unit.contains(r#"-f "/srv/my app/%%h.toml" up"#), "{unit}");
        assert!(unit.contains("Description=artzain fleet for /srv/my app/%%h.toml"));
    }
}
