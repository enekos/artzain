//! artzain CLI. Verbs mirror the mental model, not a daemon protocol:
//!
//!   artzain up      run the reconcile loop (foreground; Ctrl-C stops)
//!   artzain plan    validate the manifest and print what `up` would do
//!   artzain status  print the state of a running cluster
//!   artzain logs    print persisted logs from a running cluster
//!   artzain down    stop a running cluster

use std::path::PathBuf;

use artzain_core::{lock::Lock, plan, read_status, up, UpOptions, DEFAULT_MANIFEST};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "artzain",
    version,
    about = "A tiny k8s-shaped orchestrator for prebuilt binaries"
)]
struct Cli {
    /// Path to the manifest (default: ./artzain.toml).
    #[arg(short = 'f', long, global = true, default_value = DEFAULT_MANIFEST)]
    file: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reconcile the cluster to the manifest and supervise it (foreground).
    Up {
        /// Only these apps (their app dependencies are pulled in too).
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
        /// Don't hot-reload the manifest on file change.
        #[arg(long)]
        no_watch: bool,
    },
    /// Validate the manifest and print what `up` would do.
    Plan {
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
    },
    /// Print the state of the running cluster (reads the state file).
    Status,
    /// Print persisted logs from the running cluster.
    Logs {
        /// Only logs for this app (default: all apps).
        app: Option<String>,
    },
    /// Signal a running cluster to shut down.
    Down {
        /// Skip the lock-file safety check and signal the owner pid anyway.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Up { only, no_watch } => {
            let opts = UpOptions {
                only,
                watch: !no_watch,
            };
            run_async(up(&cli.file, opts))
        }
        Command::Plan { only } => plan(&cli.file, only.as_deref()),
        Command::Status => print_status(&cli.file),
        Command::Logs { app } => print_logs(&cli.file, app.as_deref()),
        Command::Down { force } => down(&cli.file, force),
    }
}

fn run_async<F: std::future::Future<Output = anyhow::Result<()>>>(fut: F) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

fn print_status(file: &std::path::Path) -> anyhow::Result<()> {
    let state = read_status(file)?;
    println!(
        "cluster {}  (pid {}, updated {}s ago)\n",
        state.project,
        state.owner_pid,
        now_unix().saturating_sub(state.updated_at),
    );
    println!(
        "{:<16} {:<8} {:<7} {:<6} {:<8} RESTARTS",
        "APP", "READY", "REPLICA", "PORT", "PHASE"
    );
    for app in &state.apps {
        for inst in &app.instances {
            println!(
                "{:<16} {:<8} {:<7} {:<6} {:<8} {} ({}s up)",
                app.name,
                format!("{}/{}", app.ready, app.desired),
                inst.replica,
                inst.port,
                inst.phase.as_str(),
                inst.restarts,
                inst.uptime_secs,
            );
        }
        if app.instances.is_empty() {
            println!(
                "{:<16} {:<8} (no instances)",
                app.name,
                format!("{}/{}", app.ready, app.desired)
            );
        }
    }
    Ok(())
}

fn print_logs(file: &std::path::Path, app: Option<&str>) -> anyhow::Result<()> {
    let base_dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_dir = base_dir.join(".artzain").join("logs");
    if !log_dir.exists() {
        anyhow::bail!("no log directory found — is a cluster running?");
    }

    let mut entries: Vec<_> = std::fs::read_dir(&log_dir)?
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

    if entries.is_empty() {
        println!("no log files found");
        return Ok(());
    }

    for path in entries {
        println!(
            "--- {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        match std::fs::read_to_string(&path) {
            Ok(text) => print!("{}", text),
            Err(e) => eprintln!("could not read {}: {e}", path.display()),
        }
    }
    Ok(())
}

fn down(file: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let state = read_status(file)?;
    let pid = state.owner_pid as i32;

    if !force {
        // Verify the lock file points to the same live pid before sending SIGTERM.
        // This avoids killing an unrelated process that reused a stale pid.
        let base_dir = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match Lock::read(&base_dir)? {
            Some(lock) if lock.pid == state.owner_pid => {
                if !is_pid_alive(pid) {
                    anyhow::bail!(
                        "cluster owner pid {pid} is not running; the cluster may already be down (use --force to override)"
                    );
                }
            }
            Some(_) => anyhow::bail!("lock file does not match state owner pid — refusing to signal an unrelated process (use --force to override)"),
            None => anyhow::bail!("no lock file found; the cluster may already be down (use --force to override)"),
        }
    }

    // SIGTERM the `up` process; its own teardown stops every child group.
    let rc = unsafe { libc_kill(pid, 15) };
    if rc != 0 {
        anyhow::bail!("could not signal cluster owner pid {pid} — is it still running?");
    }
    println!("sent SIGTERM to cluster owner pid {pid}");
    Ok(())
}

#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a pure liveness check.
    unsafe { libc_kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: i32) -> bool {
    true
}

// The binary crate avoids a direct libc dep for one call; declare the symbol.
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
