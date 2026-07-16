//! systemd unit generator for `artzain up`.
//!
//! Outputs a unit file that runs artzain as an unprivileged service account,
//! captures stdout/stderr to journald, and applies systemd-level hardening.
//! The installed file is named `artzain@.service` so it can be enabled as a
//! template instance (e.g. `systemctl enable artzain@/srv/app/artzain.toml`); the
//! generator-produced unit uses the concrete manifest path given on the command
//! line.

use std::fmt::Write;
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

/// Generate the unit file contents.
pub fn generate_unit(options: UnitOptions) -> String {
    let manifest = options.manifest.display();
    let binary = options.binary.display();
    let timeout = options.stop_grace_secs;

    let mut unit = String::new();
    writeln!(&mut unit, "[Unit]").unwrap();
    writeln!(&mut unit, "Description=artzain fleet for {}", manifest).unwrap();
    writeln!(&mut unit, "After=network.target").unwrap();
    writeln!(&mut unit).unwrap();

    writeln!(&mut unit, "[Service]").unwrap();
    writeln!(&mut unit, "Type=simple").unwrap();
    writeln!(&mut unit, "User={}", options.user).unwrap();
    writeln!(&mut unit, "Group={}", options.group).unwrap();
    writeln!(&mut unit, "ExecStart={} -f {} up", binary, manifest).unwrap();
    writeln!(&mut unit, "Restart=on-failure").unwrap();
    writeln!(&mut unit, "RestartSec=5").unwrap();
    writeln!(&mut unit, "KillSignal=SIGTERM").unwrap();
    writeln!(&mut unit, "TimeoutStopSec={}", timeout).unwrap();
    writeln!(&mut unit, "NoNewPrivileges=true").unwrap();
    writeln!(&mut unit, "ProtectSystem=strict").unwrap();
    writeln!(&mut unit, "ProtectHome=true").unwrap();
    writeln!(&mut unit, "PrivateTmp=true").unwrap();
    if let Some(mem) = options.memory_max {
        writeln!(&mut unit, "MemoryMax={}", mem).unwrap();
    }
    writeln!(&mut unit).unwrap();

    writeln!(&mut unit, "[Install]").unwrap();
    writeln!(&mut unit, "WantedBy=multi-user.target").unwrap();

    unit
}

/// Install the generated unit to `/etc/systemd/system/artzain@.service`.
pub fn install_unit(content: &str, path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            anyhow::bail!("{} is a directory", path.display());
        }
    }
    std::fs::write(path, content)?;
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
        let unit = generate_unit(opts);
        assert!(unit.contains("User=artzain"));
        assert!(unit.contains("Group=artzain"));
        assert!(unit.contains("ExecStart=/usr/local/bin/artzain -f /srv/app/artzain.toml up"));
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
        let unit = generate_unit(opts);
        assert!(!unit.contains("MemoryMax"));
    }
}
