//! artzain — a tiny, k8s-shaped orchestrator for prebuilt binaries (built for
//! [sutegi](https://github.com/enekos/sutegi) apps, useful for any process
//! that serves HTTP health/readiness).
//!
//! One declarative `artzain.toml` names a set of apps and how many replicas of
//! each to keep alive. `artzain up` runs a single reconcile loop that keeps
//! reality matching the file: it starts the processes, restarts them with
//! exponential backoff when they crash, drives HTTP readiness/liveness probes,
//! rolls apps when the file changes on disk, and tears everything down
//! gracefully on Ctrl-C. No git, no Docker, no daemon — a file is the whole
//! control plane.

pub mod manifest;
mod probe;
mod process;
mod reconcile;
mod state;

pub use manifest::Manifest;
pub use reconcile::{up, UpOptions};
pub use state::{AppStatus, ClusterState, InstanceStatus, Phase};

use std::path::Path;

/// Default manifest filename looked up in the current directory.
pub const DEFAULT_MANIFEST: &str = "artzain.toml";

/// Validate a manifest and print the startup plan without touching any
/// process — the `--dry-run` / `plan` surface.
pub fn plan(manifest_path: &Path, only: Option<&[String]>) -> anyhow::Result<()> {
    let manifest = Manifest::load(manifest_path)?;
    let selected = reconcile_select(&manifest, only)?;
    println!("project: {}", manifest.project());
    for check in &manifest.checks {
        let target = check
            .tcp
            .as_deref()
            .or(check.http.as_deref())
            .unwrap_or("?");
        println!("check:   {} ({target})", check.name);
    }
    for app in manifest.apps_ordered()? {
        if !selected.contains(&app.name) {
            continue;
        }
        let ports = if app.replicas == 1 {
            app.port.to_string()
        } else {
            format!("{}-{}", app.port, app.port + app.replicas as u16 - 1)
        };
        println!(
            "app:     {} x{} — {} (ports {ports})",
            app.name,
            app.replicas,
            manifest.resolve(&app.bin).display(),
        );
    }
    Ok(())
}

/// Read the state file the running `up` writes. Errors if no cluster is up.
pub fn read_status(manifest_path: &Path) -> anyhow::Result<ClusterState> {
    let base = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let state_path = base.join(".artzain").join("state.json");
    let raw = std::fs::read_to_string(&state_path).map_err(|_| {
        anyhow::anyhow!(
            "no running cluster (state file {} not found) — start one with `artzain up`",
            state_path.display()
        )
    })?;
    ClusterState::from_json(&raw)
}

// `select_apps` lives in the reconcile module; re-derive the same selection
// for `plan` without exposing the internal helper.
fn reconcile_select(
    manifest: &Manifest,
    only: Option<&[String]>,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let all: std::collections::HashSet<String> =
        manifest.apps.iter().map(|a| a.name.clone()).collect();
    let Some(only) = only else {
        return Ok(all);
    };
    let mut selected = std::collections::HashSet::new();
    let by_name: std::collections::BTreeMap<&str, &manifest::App> =
        manifest.apps.iter().map(|a| (a.name.as_str(), a)).collect();
    let mut queue: Vec<String> = Vec::new();
    for n in only {
        if !by_name.contains_key(n.as_str()) {
            anyhow::bail!("--only: unknown app `{n}`");
        }
        queue.push(n.clone());
    }
    while let Some(n) = queue.pop() {
        if !selected.insert(n.clone()) {
            continue;
        }
        if let Some(a) = by_name.get(n.as_str()) {
            for d in &a.depends_on {
                if by_name.contains_key(d.as_str()) {
                    queue.push(d.clone());
                }
            }
        }
    }
    Ok(selected)
}

/// Completes on any signal meaning "shut down": SIGINT (Ctrl-C), SIGTERM, or
/// SIGHUP. Catching all three matters: artzain owns the child process groups,
/// so if it dies without running teardown every child is orphaned and keeps
/// holding its port, and the next `up` can't bind.
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        let term = async {
            match term.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let hup = async {
            match hup.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
            _ = hup => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
