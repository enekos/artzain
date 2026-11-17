//! artzain CLI. Verbs mirror the mental model, not a daemon protocol:
//!
//!   artzain up      run the reconcile loop (foreground; Ctrl-C stops)
//!   artzain plan    validate the manifest and print what `up` would do
//!   artzain status  print the state of a running cluster
//!   artzain logs    print persisted logs from a running cluster
//!   artzain down    stop a running cluster

mod logs;
mod systemd;

use std::path::PathBuf;

use artzain_core::{DEFAULT_MANIFEST, STOP_GRACE, UpOptions, lock, plan, read_status, up};
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
    Status {
        /// Output the raw state as JSON.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when any app is not fully ready (ready < desired).
        #[arg(long)]
        check: bool,
    },
    /// Print persisted logs from the running cluster.
    Logs {
        /// Only logs for this app (default: all apps).
        app: Option<String>,
        /// Number of trailing lines to print per log file (default: 200).
        #[arg(short = 'n', long, default_value = "200")]
        tail: usize,
        /// Keep tailing as the log files grow.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Generate a systemd unit file for `artzain up`.
    Systemd {
        /// Service account user to run as (default: artzain).
        #[arg(long)]
        user: Option<String>,
        /// Service account group (default: same as user).
        #[arg(long)]
        group: Option<String>,
        /// Path to the manifest in the unit (default: the -f file).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Path to the artzain binary in the unit (default: /usr/local/bin/artzain).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Optional systemd MemoryMax= value (e.g. "512M", "2G").
        #[arg(long)]
        memory_max: Option<String>,
        /// Write the unit to /etc/systemd/system/artzain@.service instead of stdout.
        #[arg(long)]
        install: bool,
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
        Command::Status { json, check } => print_status(&cli.file, json, check),
        Command::Logs { app, tail, follow } => print_logs(&cli.file, app.as_deref(), tail, follow),
        Command::Systemd {
            user,
            group,
            manifest,
            binary,
            memory_max,
            install,
        } => generate_systemd(
            &cli.file,
            user.as_deref(),
            group.as_deref(),
            manifest.as_deref(),
            binary.as_deref(),
            memory_max.as_deref(),
            install,
        ),
        Command::Down { force } => down(&cli.file, force),
    }
}

fn run_async<F: std::future::Future<Output = anyhow::Result<()>>>(fut: F) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

fn print_status(file: &std::path::Path, json: bool, check: bool) -> anyhow::Result<()> {
    let state = read_status(file)?;

    if json {
        println!("{}", state.to_json());
    } else {
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
    }

    if check && state.apps.iter().any(|app| app.ready < app.desired) {
        std::process::exit(1);
    }

    Ok(())
}

fn generate_systemd(
    file: &std::path::Path,
    user: Option<&str>,
    group: Option<&str>,
    manifest: Option<&std::path::Path>,
    binary: Option<&std::path::Path>,
    memory_max: Option<&str>,
    install: bool,
) -> anyhow::Result<()> {
    let manifest = manifest.unwrap_or(file);
    let user = user.unwrap_or("artzain");
    let group = group.unwrap_or(user);
    let binary = binary
        .map(PathBuf::from)
        .unwrap_or_else(systemd::default_binary);

    let unit = systemd::generate_unit(systemd::UnitOptions {
        user,
        group,
        manifest,
        binary: &binary,
        memory_max,
        stop_grace_secs: STOP_GRACE.as_secs(),
    })?;

    if install {
        let path = std::path::Path::new("/etc/systemd/system/artzain@.service");
        systemd::install_unit(&unit, path)
    } else {
        print!("{}", unit);
        Ok(())
    }
}

fn print_logs(
    file: &std::path::Path,
    app: Option<&str>,
    tail: usize,
    follow: bool,
) -> anyhow::Result<()> {
    let base_dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_dir = base_dir.join(".artzain").join("logs");
    if !log_dir.exists() {
        anyhow::bail!("no log directory found — is a cluster running?");
    }

    let entries = logs::log_files(&log_dir, app)?;
    if entries.is_empty() {
        println!("no log files found");
        return Ok(());
    }

    for path in &entries {
        println!(
            "--- {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        if let Err(e) = logs::tail(path, tail) {
            eprintln!("could not tail {}: {e}", path.display());
        }
    }

    if follow {
        logs::follow(&entries, std::time::Duration::from_millis(500))?;
    }
    Ok(())
}

fn down(file: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let base_dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Only the process that holds the lock is signalled: a pid read from a
    // file could have been recycled by an unrelated process.
    let pid = match lock::signal_owner(&base_dir) {
        Ok(pid) => pid,
        Err(e) if force => {
            let pid = read_status(file)?.owner_pid;
            eprintln!("{e:#}; --force: signalling recorded owner pid {pid}");
            lock::force_signal(pid)?;
            pid
        }
        Err(e) => anyhow::bail!("{e:#} (use --force to signal the recorded owner pid anyway)"),
    };
    println!("sent SIGTERM to cluster owner pid {pid}");
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
