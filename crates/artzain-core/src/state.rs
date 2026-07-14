//! The observable cluster state. The reconciler writes a snapshot to
//! `<manifest_dir>/.artzain/state.json` on every tick; `artzain status` reads
//! it. This is the whole "control plane" — a file, not a daemon socket. It
//! makes state inspectable and survives the `up` process being backgrounded,
//! without adding a listener or an API surface to secure.

use serde::{Deserialize, Serialize};

/// A single instance's lifecycle phase — deliberately k8s-shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// Desired but not yet spawned (waiting on a dependency or a spawn slot).
    Pending,
    /// Spawned, awaiting its first readiness pass.
    Starting,
    /// Readiness probe passing — counts toward `replicas`.
    Ready,
    /// Running, but the readiness probe is currently failing.
    Unready,
    /// Crashed; waiting out the restart backoff before respawning.
    CrashLoopBackOff,
    /// Being shut down (teardown, scale-down, or rolling replace).
    Terminating,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Pending => "Pending",
            Phase::Starting => "Starting",
            Phase::Ready => "Ready",
            Phase::Unready => "Unready",
            Phase::CrashLoopBackOff => "CrashLoopBackOff",
            Phase::Terminating => "Terminating",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub replica: u32,
    pub port: u16,
    pub phase: Phase,
    /// OS pid while running, else `None`.
    pub pid: Option<u32>,
    /// How many times this slot has been (re)started since it last stabilised.
    pub restarts: u32,
    /// Seconds the current process has been up (0 when not running).
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub name: String,
    pub desired: u32,
    pub ready: u32,
    pub instances: Vec<InstanceStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterState {
    pub project: String,
    /// pid of the `artzain up` process that owns this fleet.
    pub owner_pid: u32,
    /// Unix seconds of the last reconcile tick.
    pub updated_at: u64,
    pub apps: Vec<AppStatus>,
}

impl ClusterState {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}
