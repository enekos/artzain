//! artzain CLI. Verbs mirror the mental model, not a daemon protocol:
//!
//!   artzain up      run the reconcile loop (foreground; Ctrl-C stops)
//!   artzain plan    validate the manifest and print the startup plan
//!   artzain status  print the state of a running cluster
//!   artzain down    stop a running cluster

use std::path::PathBuf;

use artzain_core::{plan, read_status, up, UpOptions, DEFAULT_MANIFEST};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "artzain", version, about = "A tiny k8s-shaped orchestrator for prebuilt binaries")]
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
    /// Signal a running cluster to shut down.
    Down,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
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
        Command::Down => down(&cli.file),
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
    println!("{:<16} {:<8} {:<7} {:<6} {:<8} RESTARTS", "APP", "READY", "REPLICA", "PORT", "PHASE");
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
            println!("{:<16} {:<8} (no instances)", app.name, format!("{}/{}", app.ready, app.desired));
        }
    }
    Ok(())
}

fn down(file: &std::path::Path) -> anyhow::Result<()> {
    let state = read_status(file)?;
    let pid = state.owner_pid as i32;
    // SIGTERM the `up` process; its own teardown stops every child group.
    let rc = unsafe { libc_kill(pid, 15) };
    if rc != 0 {
        anyhow::bail!("could not signal cluster owner pid {pid} — is it still running?");
    }
    println!("sent SIGTERM to cluster owner pid {pid}");
    Ok(())
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
