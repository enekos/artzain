//! The control loop. Given a manifest of desired state, it drives the actual
//! set of running processes toward it, forever: keeps `replicas` instances of
//! each app alive on distinct ports, restarts crashed instances with
//! exponential backoff, drives readiness/liveness probes, rolls apps when the
//! manifest changes on disk, and tears everything down gracefully on a signal.
//!
//! It is a single sequential loop — no shared state, no locks. That is a
//! deliberate reliability choice: the whole cluster's state transitions happen
//! in one place, one tick at a time, so the behavior is easy to reason about
//! and reproduce.

use crate::lock::Lock;
use crate::manifest::{App, Manifest, BASE_ENV_KEYS, DEFAULT_PATH};
use crate::paths::ensure_private_dir;
use crate::probe;
use crate::process::{self, is_group_alive, is_pid_alive, signal_group, Spawn};
use crate::state::{AppStatus, ClusterState, InstanceStatus, Phase};

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const TICK: Duration = Duration::from_millis(500);
const READY_PERIOD: Duration = Duration::from_secs(1);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const CHECK_VERIFY_PERIOD: Duration = Duration::from_secs(10);
/// Grace between SIGTERM and SIGKILL when stopping an instance.
pub const STOP_GRACE: Duration = Duration::from_secs(10);
/// Grace for orphan reclamation on startup: shorter than STOP_GRACE because
/// the previous owner is already dead and we want to claim the ports quickly.
const ORPHAN_RECLAIM_GRACE: Duration = Duration::from_secs(2);
/// Rolling-update budget: at most this many of an app's replicas may be
/// unavailable at once while replacing stale instances (k8s maxUnavailable).
const MAX_UNAVAILABLE: usize = 1;

#[derive(Debug, Default, Clone)]
pub struct UpOptions {
    /// Apps to run (their transitive app dependencies are pulled in). `None`
    /// runs every app in the manifest.
    pub only: Option<Vec<String>>,
    /// Reload and roll when the manifest file changes on disk. On by default;
    /// editing and saving the manifest is the "apply".
    pub watch: bool,
}

/// Run the reconcile loop until `predicate` returns true or `max_steps` ticks
/// have elapsed. Uses `tick` as the wall-clock sleep between ticks. If
/// `watch_manifest` is true, `maybe_reload()` is called each tick.
///
/// This is the single driver used by both `up` and integration tests so the
/// tested loop is the shipped loop.
///
/// Returns `true` if the predicate was satisfied.
pub async fn run_until(
    r: &mut Reconciler,
    predicate: impl Fn(&ClusterState) -> bool,
    max_steps: usize,
    tick: Duration,
    watch_manifest: bool,
) -> bool {
    for step in 0..max_steps {
        if watch_manifest {
            r.maybe_reload();
        }
        r.reconcile().await;
        r.write_state();
        if predicate(&r.snapshot()) {
            return true;
        }
        // Sleep between ticks, but not after the last step. With max_steps set
        // to usize::MAX by `up`, this is effectively an infinite loop.
        if step + 1 < max_steps {
            tokio::time::sleep(tick).await;
        }
    }
    false
}

/// Reclaim orphaned child process groups left behind when a previous `up`
/// died without running teardown (kill -9, OOM, reboot). If the recorded
/// owner pid is dead, SIGTERM every recorded pgid, wait `STOP_GRACE`, then
/// SIGKILL stragglers, and remove the stale state/lock files so the new
/// `up` starts from a clean manifest.
pub async fn reclaim_orphans(base_dir: &Path) -> anyhow::Result<()> {
    let state_path = base_dir.join(".artzain").join("state.json");
    let raw = match std::fs::read_to_string(&state_path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("reading stale state.json: {e}")),
    };

    let state: ClusterState = match ClusterState::from_json(&raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not parse stale state.json; removing and continuing");
            let _ = std::fs::remove_file(&state_path);
            return Ok(());
        }
    };

    if is_pid_alive(state.owner_pid as i32) {
        // Owner is still alive; this is unexpected because we hold the lock,
        // but leave the state alone and let the lock check surface the issue.
        return Ok(());
    }

    tracing::warn!(
        owner_pid = state.owner_pid,
        "prior owner is dead — reclaiming orphans"
    );

    for app in &state.apps {
        for inst in &app.instances {
            if let Some(pgid) = inst.pgid {
                signal_group(pgid, libc::SIGTERM);
            }
        }
    }

    let deadline = Instant::now() + ORPHAN_RECLAIM_GRACE;
    loop {
        let all_done = state
            .apps
            .iter()
            .flat_map(|a| &a.instances)
            .all(|i| i.pgid.map(|pgid| !is_group_alive(pgid)).unwrap_or(true));
        if all_done {
            break;
        }
        if Instant::now() >= deadline {
            for app in &state.apps {
                for inst in &app.instances {
                    if let Some(pgid) = inst.pgid {
                        if is_group_alive(pgid) {
                            signal_group(pgid, libc::SIGKILL);
                        }
                    }
                }
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = std::fs::remove_file(&state_path);
    let lock_path = Lock::path(base_dir);
    let _ = std::fs::remove_file(&lock_path);
    Ok(())
}

pub async fn up(manifest_path: &Path, opts: UpOptions) -> anyhow::Result<()> {
    let manifest = Manifest::load(manifest_path)?;
    let selected = select_apps(&manifest, opts.only.as_deref())?;
    if selected.is_empty() {
        anyhow::bail!("no apps to run");
    }

    // Take the lock early so two `up` runs can't clobber the same state file.
    let _lock = Lock::acquire(&manifest.base_dir)?;

    // If a previous owner died without teardown, reclaim its orphaned children
    // before we try to bind the same ports.
    reclaim_orphans(&manifest.base_dir).await?;

    // Verify external dependencies once, up front — fail fast with the hint
    // rather than crash-looping an app against a Postgres that isn't up.
    let checks_ok = verify_checks(&manifest, &selected).await?;

    let mut r = Reconciler::new(manifest, manifest_path.to_path_buf(), selected, checks_ok);
    r.write_state();
    tracing::info!(
        apps = r.selected.len(),
        "artzain up — reconciling; Ctrl-C to stop"
    );

    loop {
        tokio::select! {
            _ = crate::shutdown_signal() => break,
            _ = run_until(&mut r, |_| false, usize::MAX, TICK, opts.watch) => {}
        }
    }

    tracing::info!("shutting down — stopping all instances");
    r.teardown().await;
    let _ = std::fs::remove_file(&r.state_path);
    Ok(())
}

/// One managed process slot: desired `(app, replica)` plus everything about
/// the process currently (not) filling it.
struct Instance {
    app: String,
    replica: u32,
    port: u16,
    /// Hash of the spec this instance was launched with; a mismatch against
    /// the current desired spec marks it stale (→ rolling replace).
    spec_hash: u64,
    handle: Option<process::Handle>,
    phase: Phase,
    restarts: u32,
    started_at: Option<Instant>,
    backoff_until: Option<Instant>,
    ready_deadline: Option<Instant>,
    kill_deadline: Option<Instant>,
    /// On intentional stop: should the slot respawn (crash/restart) or be
    /// dropped (scale-down / rolling replace)?
    restart_after_stop: bool,
    consecutive_live_fail: u32,
    next_ready_probe: Instant,
    next_live_probe: Instant,
    ever_ready: bool,
}

impl Instance {
    fn pending(app: &str, replica: u32, port: u16, spec_hash: u64) -> Self {
        let now = Instant::now();
        Self {
            app: app.to_string(),
            replica,
            port,
            spec_hash,
            handle: None,
            phase: Phase::Pending,
            restarts: 0,
            started_at: None,
            backoff_until: None,
            ready_deadline: None,
            kill_deadline: None,
            restart_after_stop: false,
            consecutive_live_fail: 0,
            next_ready_probe: now,
            next_live_probe: now,
            ever_ready: false,
        }
    }

    fn is_running(&self) -> bool {
        self.handle.is_some()
    }
}

pub struct Reconciler {
    manifest: Manifest,
    manifest_path: PathBuf,
    /// The mtime of the manifest we're currently reconciling to.
    manifest_mtime: Option<SystemTime>,
    /// A newer mtime seen but not yet acted on — held one tick to debounce
    /// non-atomic writers (see `maybe_reload`).
    pending_mtime: Option<SystemTime>,
    selected: HashSet<String>,
    instances: Vec<Instance>,
    state_path: PathBuf,
    /// Current port assigned to each desired slot `(app, replica)`. Initially
    /// `app.port + replica`; updated during rolling updates when a new
    /// instance is launched on a temporary surge port.
    slot_ports: BTreeMap<(String, u32), u16>,
    /// Current liveness of every `[[check]]` in the manifest. Re-verified
    /// periodically while the cluster is running so apps can be gated on deps
    /// that come and go.
    checks_ok: BTreeMap<String, bool>,
    /// When to re-verify checks next.
    next_check_verify: Instant,
}

impl Reconciler {
    pub fn new(
        manifest: Manifest,
        manifest_path: PathBuf,
        selected: HashSet<String>,
        checks_ok: BTreeMap<String, bool>,
    ) -> Self {
        let state_path = manifest.base_dir.join(".artzain").join("state.json");
        if let Some(dir) = state_path.parent() {
            let _ = ensure_private_dir(dir);
        }
        let mtime = file_mtime(&manifest_path);
        Self {
            manifest,
            manifest_path,
            manifest_mtime: mtime,
            pending_mtime: None,
            selected,
            instances: Vec::new(),
            state_path,
            slot_ports: BTreeMap::new(),
            checks_ok,
            next_check_verify: Instant::now() + CHECK_VERIFY_PERIOD,
        }
    }

    /// Reload the manifest if it changed on disk — but only once the change
    /// has settled. Two guards keep a mid-write read from taking the fleet
    /// down: (1) *debounce* — a new mtime must hold steady for one tick before
    /// we act, so a truncate-then-write editor or shell `>` isn't read while
    /// half-written; (2) any parse error, or a manifest that reloads to zero
    /// apps (what an empty/partial file looks like), is rejected and the last
    /// good state is kept.
    pub fn maybe_reload(&mut self) {
        let current = file_mtime(&self.manifest_path);
        if current == self.manifest_mtime {
            self.pending_mtime = None;
            return;
        }
        // Wait for the mtime to be stable across one tick before acting.
        if self.pending_mtime != current {
            self.pending_mtime = current;
            return;
        }
        self.pending_mtime = None;
        self.manifest_mtime = current;

        let reloaded = Manifest::load(&self.manifest_path).and_then(|m| {
            if m.apps.is_empty() {
                anyhow::bail!("no apps (empty or partial file?)");
            }
            let selected = select_apps(&m, self.only_names().as_deref())?;
            if selected.is_empty() {
                anyhow::bail!("selection is empty");
            }
            Ok((m, selected))
        });
        match reloaded {
            Ok((m, selected)) => {
                tracing::info!("manifest changed — reconciling to new desired state");
                self.manifest = m;
                self.selected = selected;
            }
            Err(e) => {
                tracing::error!("manifest reload rejected ({e:#}) — keeping current state")
            }
        }
    }

    /// Reconstruct the `--only` list from the currently selected apps so a
    /// reload keeps the same selection semantics.
    fn only_names(&self) -> Option<Vec<String>> {
        // If every app is selected, treat as "no filter" so newly-added apps
        // are picked up; otherwise preserve the explicit selection.
        if self.selected.len() == self.manifest.apps.len() {
            None
        } else {
            Some(self.selected.iter().cloned().collect())
        }
    }

    /// Re-verify `[[check]]` entries periodically. If a check flips from ok to
    /// not-ok, dependents will be stopped on the next tick; when it recovers,
    /// `spawn_missing` will restart them.
    async fn reverify_checks(&mut self) {
        let now = Instant::now();
        if now < self.next_check_verify {
            return;
        }
        self.next_check_verify = now + CHECK_VERIFY_PERIOD;

        for check in &self.manifest.checks {
            let ok = match (&check.tcp, &check.http) {
                (Some(addr), _) => probe::tcp_once(addr).await,
                (None, Some(hostpath)) => {
                    let (addr, path) = split_host_path(hostpath);
                    probe::http_once(addr, path, Duration::from_secs(2)).await
                }
                (None, None) => continue,
            };
            let was = self.checks_ok.get(&check.name).copied().unwrap_or(true);
            if ok && !was {
                tracing::info!(check = %check.name, "recovered");
            } else if !ok && was {
                tracing::warn!(check = %check.name, "failed");
            }
            self.checks_ok.insert(check.name.clone(), ok);
        }
    }

    /// Stop running instances whose app depends on a check that is currently
    /// failing. They will be held in Pending/CrashLoopBackOff until the check
    /// recovers and `spawn_missing` restarts them.
    fn stop_check_blocked(&mut self) {
        for idx in 0..self.instances.len() {
            let (app_name, replica, blocked) = {
                let inst = &self.instances[idx];
                let blocked = self
                    .manifest
                    .app(&inst.app)
                    .map(|a| {
                        a.depends_on.iter().any(|d| {
                            // App deps are handled elsewhere; only stop for
                            // failing checks.
                            self.manifest.app(d).is_none()
                                && self.checks_ok.get(d).copied() == Some(false)
                        })
                    })
                    .unwrap_or(false);
                (inst.app.clone(), inst.replica, blocked)
            };
            if blocked {
                let inst = &mut self.instances[idx];
                if inst.is_running() && inst.phase != Phase::Terminating {
                    tracing::warn!(app = %app_name, replica, "dependency check failed — stopping instance");
                    begin_stop(inst, true);
                }
            }
        }
    }

    pub async fn reconcile(&mut self) {
        self.reverify_checks().await;
        self.reap();
        let desired = self.desired_slots();
        self.terminate_undesired(&desired);
        self.stop_check_blocked();
        self.roll_stale(&desired);
        let desired = self.desired_slots();
        self.spawn_missing(&desired);
        self.retire_stale_surplus(&desired);
        self.probe().await;
    }

    /// Harvest exited children and route each exit to the right outcome.
    fn reap(&mut self) {
        let now = Instant::now();
        let stable = Duration::from_secs(self.manifest.defaults.stable_after_secs);
        let base = self.manifest.defaults.restart_backoff_ms;
        let cap = self.manifest.defaults.restart_backoff_max_ms;
        let mut drop_slots: Vec<(String, u32)> = Vec::new();

        for inst in &mut self.instances {
            let Some(handle) = inst.handle.as_mut() else {
                continue;
            };
            match handle.child.try_wait() {
                Ok(Some(status)) => {
                    let uptime = inst
                        .started_at
                        .map(|t| now.duration_since(t))
                        .unwrap_or_default();
                    inst.handle = None;
                    inst.started_at = None;
                    inst.ever_ready = false;

                    let respawn = inst.phase != Phase::Terminating || inst.restart_after_stop;
                    if respawn {
                        if uptime >= stable {
                            inst.restarts = 0;
                        }
                        inst.restarts += 1;
                        let backoff = backoff_delay(base, cap, inst.restarts);
                        inst.backoff_until = Some(now + backoff);
                        inst.phase = Phase::CrashLoopBackOff;
                        inst.restart_after_stop = false;
                        tracing::warn!(
                            app = %inst.app, replica = inst.replica, %status,
                            restarts = inst.restarts, backoff_ms = backoff.as_millis() as u64,
                            "instance exited — backing off"
                        );
                    } else {
                        drop_slots.push((inst.app.clone(), inst.replica));
                    }
                }
                Ok(None) => {
                    // Still running. If we asked it to stop and it ignored
                    // SIGTERM past the grace window, escalate.
                    if inst.phase == Phase::Terminating {
                        if let Some(dl) = inst.kill_deadline {
                            if now >= dl {
                                process::kill(handle);
                                inst.kill_deadline = Some(now + Duration::from_secs(2));
                            }
                        }
                    }
                }
                Err(_) => {}
            }
        }
        self.instances
            .retain(|i| !drop_slots.contains(&(i.app.clone(), i.replica)));
    }

    /// Desired `(app, replica) -> (port, spec_hash)` for every selected app.
    fn desired_slots(&self) -> BTreeMap<(String, u32), (u16, u64)> {
        let mut out = BTreeMap::new();
        for app in &self.manifest.apps {
            if !self.selected.contains(&app.name) {
                continue;
            }
            for i in 0..app.replicas {
                let port = self
                    .slot_ports
                    .get(&(app.name.clone(), i))
                    .copied()
                    .unwrap_or_else(|| app.port + i as u16);
                let hash = spec_hash(&self.manifest, app, i, port);
                out.insert((app.name.clone(), i), (port, hash));
            }
        }
        out
    }

    /// Stop instances that are no longer desired (app removed or scaled down).
    /// Retires higher replica indices first so the surviving set stays
    /// contiguous (0, 1, …) rather than leaving a gap.
    fn terminate_undesired(&mut self, desired: &BTreeMap<(String, u32), (u16, u64)>) {
        // Collect indices first, then sort by descending replica index.
        let mut indices: Vec<usize> = self
            .instances
            .iter()
            .enumerate()
            .filter(|(_, i)| !desired.contains_key(&(i.app.clone(), i.replica)))
            .map(|(idx, _)| idx)
            .collect();
        indices.sort_by_key(|&idx| std::cmp::Reverse(self.instances[idx].replica));

        let mut remove = Vec::new();
        for idx in indices {
            let inst = &mut self.instances[idx];
            if inst.is_running() {
                if inst.phase != Phase::Terminating {
                    begin_stop(inst, false);
                }
            } else {
                remove.push((inst.app.clone(), inst.replica));
            }
        }
        self.instances
            .retain(|i| !remove.contains(&(i.app.clone(), i.replica)));
    }

    /// Rolling replace: for each app, retire stale instances. Stale non-Ready
    /// instances are stopped directly (they are already unavailable). When
    /// `max_surge == 0`, stale Ready instances are stopped one at a time while
    /// respecting the maxUnavailable budget. When `max_surge > 0`, a new
    /// instance is launched on a temporary surge port before the old one is
    /// stopped, giving a zero-downtime roll at the cost of a temporary port
    /// change.
    fn roll_stale(&mut self, desired: &BTreeMap<(String, u32), (u16, u64)>) {
        let app_names: Vec<String> = self
            .selected
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        for app in app_names {
            let app_cfg = self.manifest.app(&app);
            let desired_count = app_cfg.map(|a| a.replicas as usize).unwrap_or(0);
            let ready = self
                .instances
                .iter()
                .filter(|i| i.app == app && i.phase == Phase::Ready)
                .count();
            let unavailable = desired_count.saturating_sub(ready);
            let max_surge = self.manifest.defaults.max_surge as usize;
            let total = self.instances.iter().filter(|i| i.app == app).count();
            let surge_budget = desired_count + max_surge;

            // Any stale non-Ready instance can be rolled directly: it is
            // already unavailable, so replacing it cannot make things worse.
            let non_ready_target = self.instances.iter_mut().find(|i| {
                i.app == app
                    && i.phase != Phase::Ready
                    && i.phase != Phase::Terminating
                    && desired
                        .get(&(i.app.clone(), i.replica))
                        .map(|(_, h)| *h != i.spec_hash)
                        .unwrap_or(false)
            });
            if let Some(inst) = non_ready_target {
                tracing::info!(app = %inst.app, replica = inst.replica, phase = ?inst.phase, "rolling: retiring stale non-ready instance");
                begin_stop(inst, false);
                continue;
            }

            if max_surge > 0 {
                // Surge strategy: keep the old Ready instance running while we
                // start a new one on a temporary port. Once the new one is
                // Ready, retire_stale_surplus() will stop the old one.
                let stale_ready = self.instances.iter().find(|i| {
                    i.app == app
                        && i.phase == Phase::Ready
                        && desired
                            .get(&(i.app.clone(), i.replica))
                            .map(|(_, h)| *h != i.spec_hash)
                            .unwrap_or(false)
                });
                if let Some(stale) = stale_ready {
                    let slot_key = (stale.app.clone(), stale.replica);
                    let current_hash = desired.get(&slot_key).map(|(_, h)| *h).unwrap_or(0);
                    let has_new_for_slot = self.instances.iter().any(|i| {
                        i.app == stale.app
                            && i.replica == stale.replica
                            && i.spec_hash == current_hash
                    });
                    if !has_new_for_slot && total < surge_budget {
                        let surge_port = surge_port_for(&self.manifest, app_cfg.unwrap());
                        self.slot_ports.insert(slot_key.clone(), surge_port);
                        tracing::info!(
                            app = %stale.app,
                            replica = stale.replica,
                            old_port = stale.port,
                            new_port = surge_port,
                            "rolling: allocating surge port for stale instance"
                        );
                    }
                }
                continue;
            }

            if unavailable >= MAX_UNAVAILABLE {
                continue;
            }
            // Find one Ready, stale instance to retire.
            let target = self.instances.iter_mut().find(|i| {
                i.app == app
                    && i.phase == Phase::Ready
                    && desired
                        .get(&(i.app.clone(), i.replica))
                        .map(|(_, h)| *h != i.spec_hash)
                        .unwrap_or(false)
            });
            if let Some(inst) = target {
                tracing::info!(app = %inst.app, replica = inst.replica, "rolling: retiring stale ready instance");
                begin_stop(inst, false); // dropped on exit, recreated with new spec
            }
        }
    }

    /// When maxSurge is enabled, a slot may temporarily have both an old and a
    /// new instance. Once the new instance is Ready, stop the old one.
    fn retire_stale_surplus(&mut self, desired: &BTreeMap<(String, u32), (u16, u64)>) {
        if self.manifest.defaults.max_surge == 0 {
            return;
        }
        let mut to_stop = Vec::new();
        for ((app, replica), (_, current_hash)) in desired {
            let slot_instances: Vec<_> = self
                .instances
                .iter()
                .enumerate()
                .filter(|(_, i)| &i.app == app && i.replica == *replica)
                .collect();
            if slot_instances.len() <= 1 {
                continue;
            }
            let has_new_ready = slot_instances
                .iter()
                .any(|(_, i)| i.spec_hash == *current_hash && i.phase == Phase::Ready);
            if !has_new_ready {
                continue;
            }
            for (idx, i) in slot_instances {
                if i.spec_hash != *current_hash && i.phase != Phase::Terminating {
                    to_stop.push(idx);
                }
            }
        }
        for idx in to_stop {
            let inst = &mut self.instances[idx];
            tracing::info!(
                app = %inst.app,
                replica = inst.replica,
                port = inst.port,
                "rolling: retiring stale instance after surge replacement is ready"
            );
            begin_stop(inst, false);
        }
    }

    /// Create Pending slots for anything desired-but-missing, and spawn any
    /// Pending/backed-off instance whose backoff has elapsed and whose app
    /// dependencies are Ready.
    fn spawn_missing(&mut self, desired: &BTreeMap<(String, u32), (u16, u64)>) {
        for ((app, replica), (port, hash)) in desired {
            let has_current = self
                .instances
                .iter()
                .any(|i| &i.app == app && i.replica == *replica && i.spec_hash == *hash);
            if !has_current {
                self.instances
                    .push(Instance::pending(app, *replica, *port, *hash));
            }
        }

        let now = Instant::now();
        // Which apps currently have a Ready instance — for dependency gating.
        let ready_apps: HashSet<String> = self
            .instances
            .iter()
            .filter(|i| i.phase == Phase::Ready)
            .map(|i| i.app.clone())
            .collect();

        for idx in 0..self.instances.len() {
            let (app_name, replica, port, backoff_ok, deps_ok) = {
                let inst = &self.instances[idx];
                if inst.is_running() || inst.phase == Phase::Terminating {
                    continue;
                }
                let backoff_ok = inst.backoff_until.map(|t| now >= t).unwrap_or(true);
                let deps_ok = self
                    .manifest
                    .app(&inst.app)
                    .map(|a| {
                        a.depends_on.iter().all(|d| {
                            if self.manifest.app(d).is_some() {
                                // App deps must be Ready.
                                ready_apps.contains(d)
                            } else {
                                // Check deps must currently be ok.
                                self.checks_ok.get(d).copied().unwrap_or(true)
                            }
                        })
                    })
                    .unwrap_or(false);
                (
                    inst.app.clone(),
                    inst.replica,
                    inst.port,
                    backoff_ok,
                    deps_ok,
                )
            };
            if !backoff_ok || !deps_ok {
                continue;
            }
            self.spawn(idx, &app_name, replica, port);
        }
    }

    fn spawn(&mut self, idx: usize, app_name: &str, replica: u32, port: u16) {
        let Some(app) = self.manifest.app(app_name).cloned() else {
            return;
        };
        let cwd = self
            .manifest
            .resolve(app.dir.as_deref().unwrap_or(Path::new(".")));
        let bin = self.manifest.resolve(&app.bin);
        let env = instance_env(&self.manifest, &app, replica, port);

        if !process::port_listeners(port).is_empty() {
            tracing::warn!(app = %app_name, port, "port already in use — spawn may fail to bind");
        }

        let log_path = self
            .manifest
            .base_dir
            .join(".artzain")
            .join("logs")
            .join(format!("{}-{}.log", app_name, replica));
        let spawn = Spawn {
            app: app_name,
            replica,
            bin: &bin,
            args: &app.args,
            cwd: &cwd,
            env: &env,
            log_path: Some(log_path),
            log_max_bytes: self.manifest.defaults.log_max_bytes,
            log_keep: self.manifest.defaults.log_keep,
            uid: app.uid,
            gid: app.gid,
            limits: app.limits.clone(),
        };
        match process::spawn(&spawn) {
            Ok(handle) => {
                let now = Instant::now();
                // No readiness endpoint → Ready as soon as it's running (like
                // a bare worker with no HTTP server). Otherwise it starts in
                // Starting and a probe promotes it.
                let no_ready_probe = app.ready_path.is_empty();
                let inst = &mut self.instances[idx];
                inst.handle = Some(handle);
                inst.phase = if no_ready_probe {
                    Phase::Ready
                } else {
                    Phase::Starting
                };
                inst.started_at = Some(now);
                inst.backoff_until = None;
                inst.ready_deadline = Some(now + Duration::from_secs(app.ready_timeout));
                inst.consecutive_live_fail = 0;
                inst.ever_ready = no_ready_probe;
                inst.next_ready_probe = now;
                inst.next_live_probe = now + Duration::from_secs(app.live_period);
                inst.spec_hash = spec_hash(&self.manifest, &app, replica, port);
                tracing::info!(app = %app_name, replica, port, "starting");
            }
            Err(e) => {
                tracing::error!("{e:#}");
                let inst = &mut self.instances[idx];
                inst.restarts += 1;
                let backoff = backoff_delay(
                    self.manifest.defaults.restart_backoff_ms,
                    self.manifest.defaults.restart_backoff_max_ms,
                    inst.restarts,
                );
                inst.backoff_until = Some(Instant::now() + backoff);
                inst.phase = Phase::CrashLoopBackOff;
            }
        }
    }

    /// Drive readiness and liveness probes for running instances.
    async fn probe(&mut self) {
        let now = Instant::now();
        for idx in 0..self.instances.len() {
            let (running, phase, port, ready_due, live_due, app_name, ready_dl) = {
                let i = &self.instances[idx];
                (
                    i.is_running(),
                    i.phase,
                    i.port,
                    now >= i.next_ready_probe,
                    now >= i.next_live_probe,
                    i.app.clone(),
                    i.ready_deadline,
                )
            };
            if !running || phase == Phase::Terminating {
                continue;
            }
            let Some(app) = self.manifest.app(&app_name).cloned() else {
                continue;
            };
            let addr = format!("127.0.0.1:{port}");

            // Readiness — gates the Ready phase; never restarts on its own.
            // Skipped entirely when the app declares no readiness endpoint
            // (it was marked Ready at spawn).
            if ready_due && !app.ready_path.is_empty() {
                let ok = probe::http_once(&addr, &app.ready_path, PROBE_TIMEOUT).await;
                let inst = &mut self.instances[idx];
                inst.next_ready_probe = Instant::now() + READY_PERIOD;
                if ok {
                    if inst.phase != Phase::Ready {
                        tracing::info!(app = %app_name, replica = inst.replica, "ready");
                    }
                    inst.phase = Phase::Ready;
                    inst.ever_ready = true;
                } else if inst.phase == Phase::Ready {
                    inst.phase = Phase::Unready;
                } else if inst.phase == Phase::Starting
                    && ready_dl.map(|dl| Instant::now() >= dl).unwrap_or(false)
                {
                    tracing::warn!(app = %app_name, replica = inst.replica, "failed to become ready in time — restarting");
                    begin_stop(inst, true);
                    continue;
                }
            }

            // Liveness — only once the instance has been ready; consecutive
            // failures restart it.
            if live_due && !app.live_path.is_empty() {
                let inst_ever = self.instances[idx].ever_ready;
                if inst_ever {
                    let ok = probe::http_once(&addr, &app.live_path, PROBE_TIMEOUT).await;
                    let inst = &mut self.instances[idx];
                    inst.next_live_probe = Instant::now() + Duration::from_secs(app.live_period);
                    if ok {
                        inst.consecutive_live_fail = 0;
                    } else {
                        inst.consecutive_live_fail += 1;
                        if inst.consecutive_live_fail >= app.live_failures {
                            tracing::warn!(
                                app = %app_name, replica = inst.replica,
                                fails = inst.consecutive_live_fail, "liveness failed — restarting"
                            );
                            begin_stop(inst, true);
                        }
                    }
                }
            }
        }
    }

    /// Graceful teardown in reverse dependency order: SIGTERM every instance,
    /// wait for exits up to the grace window, then SIGKILL stragglers.
    pub async fn teardown(&mut self) {
        let order = self
            .manifest
            .apps_ordered()
            .map(|v| v.iter().map(|a| a.name.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        let rank = |app: &str| order.iter().position(|n| n == app).unwrap_or(usize::MAX);
        // Reverse start order: dependents die before their dependencies.
        self.instances
            .sort_by_key(|i| std::cmp::Reverse(rank(&i.app)));

        for inst in &mut self.instances {
            if let Some(h) = &inst.handle {
                process::terminate(h);
                inst.phase = Phase::Terminating;
            }
        }
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            let mut all_done = true;
            for inst in &mut self.instances {
                if let Some(h) = inst.handle.as_mut() {
                    match h.child.try_wait() {
                        Ok(Some(_)) => inst.handle = None,
                        _ => all_done = false,
                    }
                }
            }
            if all_done {
                break;
            }
            if Instant::now() >= deadline {
                for inst in &self.instances {
                    if let Some(h) = &inst.handle {
                        process::kill(h);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn snapshot(&self) -> ClusterState {
        let now = Instant::now();
        let mut apps: BTreeMap<String, Vec<InstanceStatus>> = BTreeMap::new();
        for inst in &self.instances {
            apps.entry(inst.app.clone())
                .or_default()
                .push(InstanceStatus {
                    replica: inst.replica,
                    port: inst.port,
                    phase: inst.phase,
                    pid: inst.handle.as_ref().and_then(|h| h.child.id()),
                    pgid: inst.handle.as_ref().map(|h| h.pgid),
                    restarts: inst.restarts,
                    uptime_secs: inst
                        .started_at
                        .map(|t| now.duration_since(t).as_secs())
                        .unwrap_or(0),
                });
        }
        let app_status = self
            .manifest
            .apps
            .iter()
            .filter(|a| self.selected.contains(&a.name))
            .map(|a| {
                let mut instances = apps.remove(&a.name).unwrap_or_default();
                instances.sort_by_key(|i| i.replica);
                let ready = instances.iter().filter(|i| i.phase == Phase::Ready).count() as u32;
                AppStatus {
                    name: a.name.clone(),
                    desired: a.replicas,
                    ready,
                    instances,
                }
            })
            .collect();
        ClusterState {
            state_version: 1,
            project: self.manifest.project().to_string(),
            owner_pid: std::process::id(),
            updated_at: now_unix(),
            apps: app_status,
        }
    }

    pub fn write_state(&self) {
        let state = self.snapshot();
        if let Err(e) =
            crate::paths::write_private_file(&self.state_path, state.to_json().as_bytes())
        {
            tracing::error!(
                path = %self.state_path.display(),
                error = %e,
                "could not write state file"
            );
        }
    }
}

/// Find a port outside the declared ranges of every app in the manifest, to
/// use as a temporary surge port during a rolling update. If every port above
/// the app's range is taken, this falls back to a likely-colliding port; the
/// bind failure will be handled like any other spawn failure.
fn surge_port_for(manifest: &Manifest, app: &App) -> u16 {
    let claimed: HashSet<u16> = manifest
        .apps
        .iter()
        .flat_map(|a| (0..a.replicas).map(move |i| a.port + i as u16))
        .collect();
    let base = app.port as u32 + app.replicas;
    let mut candidate = base;
    while candidate <= u16::MAX as u32 {
        let p = candidate as u16;
        if !claimed.contains(&p) {
            return p;
        }
        candidate += 1;
    }
    // Overflow fallback: use the base even if it collides.
    base as u16
}

/// SIGTERM an instance and mark it terminating. `respawn` decides the fate
/// once it exits: restart with backoff, or drop the slot.
fn begin_stop(inst: &mut Instance, respawn: bool) {
    if let Some(h) = &inst.handle {
        process::terminate(h);
    }
    inst.phase = Phase::Terminating;
    inst.restart_after_stop = respawn;
    inst.kill_deadline = Some(Instant::now() + STOP_GRACE);
}

fn backoff_delay(base_ms: u64, cap_ms: u64, restarts: u32) -> Duration {
    let shift = restarts.saturating_sub(1).min(20);
    let ms = base_ms.saturating_mul(1u64 << shift).min(cap_ms);
    Duration::from_millis(ms)
}

/// Env for one instance: inherited base/allowlist keys, then `[defaults].env`,
/// then the app's own env, then the per-instance `PORT`/`ARTZAIN_*` pointers.
/// Because `spawn` calls `env_clear()`, this map is the child's *entire* env.
fn instance_env(
    manifest: &Manifest,
    app: &App,
    replica: u32,
    port: u16,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    // Inherit base keys (not hashed — parent env drift must not roll the app).
    for key in BASE_ENV_KEYS {
        if key == &"PATH" {
            let path = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_PATH.to_string());
            env.insert(key.to_string(), path);
        } else if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    for key in &manifest.defaults.inherit_env {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    for (k, v) in &manifest.defaults.env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in &app.env {
        env.insert(k.clone(), v.clone());
    }
    env.insert("PORT".to_string(), port.to_string());
    env.insert("ARTZAIN_APP".to_string(), app.name.clone());
    env.insert("ARTZAIN_REPLICA".to_string(), replica.to_string());
    env
}

/// The configured env that should trigger a relaunch when it changes. The
/// inherited process env is intentionally excluded so PATH/HOME/etc. drift does
/// not restart apps.
fn configured_env(
    manifest: &Manifest,
    app: &App,
    replica: u32,
    port: u16,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (k, v) in &manifest.defaults.env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in &app.env {
        env.insert(k.clone(), v.clone());
    }
    env.insert("PORT".to_string(), port.to_string());
    env.insert("ARTZAIN_APP".to_string(), app.name.clone());
    env.insert("ARTZAIN_REPLICA".to_string(), replica.to_string());
    env
}

/// Hash the parts of an app spec whose change requires relaunching an
/// instance. `replicas` is intentionally excluded — scaling adds/removes
/// slots, it doesn't restart existing ones. Inherited env is excluded so
/// parent-env drift does not roll the app.
fn spec_hash(manifest: &Manifest, app: &App, replica: u32, port: u16) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    app.bin.hash(&mut h);
    app.args.hash(&mut h);
    app.dir.hash(&mut h);
    port.hash(&mut h);
    app.ready_path.hash(&mut h);
    app.live_path.hash(&mut h);
    app.live_period.hash(&mut h);
    app.live_failures.hash(&mut h);
    app.ready_timeout.hash(&mut h);
    for (k, v) in configured_env(manifest, app, replica, port) {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    h.finish()
}

/// Selected apps plus their transitive app dependencies. `None` selects all.
pub fn select_apps(
    manifest: &Manifest,
    only: Option<&[String]>,
) -> anyhow::Result<HashSet<String>> {
    let by_name: BTreeMap<&str, &App> =
        manifest.apps.iter().map(|a| (a.name.as_str(), a)).collect();
    let Some(only) = only else {
        return Ok(manifest.apps.iter().map(|a| a.name.clone()).collect());
    };
    let mut selected = HashSet::new();
    let mut queue: Vec<&str> = Vec::new();
    for name in only {
        if !by_name.contains_key(name.as_str()) {
            anyhow::bail!("--only: unknown app `{name}`");
        }
        queue.push(name.as_str());
    }
    while let Some(name) = queue.pop() {
        if !selected.insert(name.to_string()) {
            continue;
        }
        if let Some(app) = by_name.get(name) {
            for dep in &app.depends_on {
                if by_name.contains_key(dep.as_str()) {
                    queue.push(dep.as_str());
                }
            }
        }
    }
    Ok(selected)
}

/// Verify every `[[check]]` reachable from a selected app (and any check with
/// no dependents too — a bare check in the manifest is an assertion). Returns a
/// map of check name -> ok status. Fails with the hint if anything is down on
/// the first check.
async fn verify_checks(
    manifest: &Manifest,
    _selected: &HashSet<String>,
) -> anyhow::Result<BTreeMap<String, bool>> {
    let mut out = BTreeMap::new();
    let mut failures = Vec::new();
    for check in &manifest.checks {
        let ok = match (&check.tcp, &check.http) {
            (Some(addr), _) => probe::tcp_once(addr).await,
            (None, Some(hostpath)) => {
                let (addr, path) = split_host_path(hostpath);
                probe::http_once(addr, path, Duration::from_secs(2)).await
            }
            (None, None) => unreachable!("validated at load"),
        };
        out.insert(check.name.clone(), ok);
        if ok {
            tracing::info!(check = %check.name, "ok");
        } else {
            failures.push(match &check.hint {
                Some(hint) => format!("`{}` is not reachable — try: {hint}", check.name),
                None => format!("`{}` is not reachable", check.name),
            });
        }
    }
    if failures.is_empty() {
        Ok(out)
    } else {
        anyhow::bail!("dependency checks failed:\n  - {}", failures.join("\n  - "))
    }
}

fn split_host_path(s: &str) -> (&str, &str) {
    match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "/"),
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(500, 30_000, 1), Duration::from_millis(500));
        assert_eq!(backoff_delay(500, 30_000, 2), Duration::from_millis(1000));
        assert_eq!(backoff_delay(500, 30_000, 3), Duration::from_millis(2000));
        // Caps out.
        assert_eq!(
            backoff_delay(500, 30_000, 20),
            Duration::from_millis(30_000)
        );
    }

    #[test]
    fn spec_hash_changes_with_relevant_fields() {
        let m = Manifest::parse(
            r#"
[[app]]
name = "x"
bin = "./x"
port = 8080
"#,
            Path::new("artzain.toml"),
        )
        .unwrap();
        let a = &m.apps[0];
        let h1 = spec_hash(&m, a, 0, 8080);
        let h2 = spec_hash(&m, a, 0, 8081); // different port
        assert_ne!(h1, h2);

        let m2 = Manifest::parse(
            r#"
[[app]]
name = "x"
bin = "./x"
port = 8080
args = ["--flag"]
"#,
            Path::new("artzain.toml"),
        )
        .unwrap();
        // args changed -> stale
        assert_ne!(
            spec_hash(&m, a, 0, 8080),
            spec_hash(&m2, &m2.apps[0], 0, 8080)
        );
    }

    #[test]
    fn select_apps_pulls_in_dependencies() {
        let m = Manifest::parse(
            r#"
[[app]]
name = "gateway"
bin = "./gw"
port = 9000
depends_on = ["data"]

[[app]]
name = "data"
bin = "./data"
port = 8080

[[app]]
name = "unrelated"
bin = "./u"
port = 7000
"#,
            Path::new("artzain.toml"),
        )
        .unwrap();
        let sel = select_apps(&m, Some(&["gateway".to_string()])).unwrap();
        assert!(sel.contains("gateway"));
        assert!(sel.contains("data"));
        assert!(!sel.contains("unrelated"));
    }

    #[test]
    fn split_host_path_splits_on_first_slash() {
        assert_eq!(
            split_host_path("127.0.0.1:8080/health"),
            ("127.0.0.1:8080", "/health")
        );
        assert_eq!(split_host_path("127.0.0.1:8080"), ("127.0.0.1:8080", "/"));
    }

    #[test]
    fn surge_port_is_outside_declared_ranges() {
        let m = Manifest::parse(
            r#"
[[app]]
name = "a"
bin = "./a"
port = 8080
replicas = 2

[[app]]
name = "b"
bin = "./b"
port = 8082
"#,
            Path::new("artzain.toml"),
        )
        .unwrap();
        let a = m.app("a").unwrap();
        let surge = surge_port_for(&m, a);
        // 8080 and 8081 are claimed by `a`; 8082 by `b`. Surge must be 8083 or higher.
        assert!(surge >= 8083);
    }
}
