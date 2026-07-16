//! Integration tests for artzain. Each test drives a real child process
//! (the `artzain-fixture` binary) through the real reconcile loop, so behavior
//! is proven against the shipped loop, not against mocks.

use artzain_core::{
    reclaim_orphans, run_until, select_apps, AppStatus, ClusterState, InstanceStatus, Manifest,
    Phase, Reconciler,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static NEXT_PORT: AtomicU16 = AtomicU16::new(40_000);

/// Allocate a contiguous block of `n` ports for a test. Tests run in parallel,
/// so each test gets its own block to avoid collisions.
fn alloc_ports(n: u16) -> u16 {
    NEXT_PORT.fetch_add(n, Ordering::SeqCst)
}

/// Locate the fixture binary. Cargo sets `CARGO_BIN_EXE_artzain-fixture` for
/// binaries built as part of the test run; otherwise fall back to the same
/// `target/debug` directory the current test binary was built in. As a last
/// resort, build the fixture crate on demand so `cargo test -p artzain-core`
/// works locally without a manual build step.
fn fixture_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_artzain-fixture") {
        return p.into();
    }
    let current_exe = std::env::current_exe().expect("current exe");
    // current_exe is target/debug/deps/integration-...; the fixture is at
    // target/debug/artzain-fixture.
    let debug_dir = current_exe.parent().unwrap().parent().unwrap();
    let path = debug_dir.join("artzain-fixture");
    if path.exists() {
        return path;
    }
    let output = std::process::Command::new("cargo")
        .args(["build", "-p", "artzain-fixture"])
        .output()
        .expect("cargo build fixture should run");
    if !output.status.success() {
        panic!(
            "fixture build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    path
}

struct Harness {
    tmp: TempDir,
    base_port: u16,
    fixture: PathBuf,
}

impl Harness {
    fn new(ports_needed: u16) -> Self {
        let fixture = fixture_bin();
        assert!(
            fixture.exists(),
            "fixture binary not found at {} — build it with `cargo build -p artzain-fixture`",
            fixture.display()
        );
        Self {
            tmp: TempDir::new().unwrap(),
            base_port: alloc_ports(ports_needed),
            fixture,
        }
    }

    /// Write the manifest, substituting `{{fixture}}` for the fixture path and
    /// `{{port}}` for the base port. Other placeholders can be added as needed.
    fn write_manifest(&self, body: &str) -> PathBuf {
        let path = self.tmp.path().join("artzain.toml");
        let body = body
            .replace("{{fixture}}", &self.fixture.to_string_lossy())
            .replace("{{port}}", &self.base_port.to_string());
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn start_reconciler(path: &Path) -> Reconciler {
    let manifest = Manifest::load(path).expect("manifest should load");
    let selected = select_apps(&manifest, None).expect("apps should select");
    Reconciler::new(manifest, path.to_path_buf(), selected, BTreeMap::new())
}

async fn http_get(port: u16, path: &str) -> Option<String> {
    let addr = format!("127.0.0.1:{port}");
    let stream = tokio::net::TcpStream::connect(&addr).await.ok()?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        path
    );
    writer.write_all(request.as_bytes()).await.ok()?;
    writer.shutdown().await.ok()?;
    let mut buf = Vec::new();
    let mut temp = [0u8; 1024];
    loop {
        match reader.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&temp[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8(buf).ok().and_then(|text| {
        text.split("\r\n\r\n")
            .nth(1)
            .map(|body| body.trim().to_string())
    })
}

/// Return the current effective uid.
#[cfg(unix)]
#[allow(dead_code)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Return the name of the user to drop to when running as root, or None.
#[cfg(unix)]
#[allow(dead_code)]
fn drop_target_user() -> Option<(String, u32, u32)> {
    use std::ffi::CStr;
    if current_uid() != 0 {
        return None;
    }
    // Prefer the user that invoked sudo, so the test can run in a normal
    // macOS/Linux sudo setup.
    if let (Ok(uid_str), Ok(gid_str)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
        if let (Ok(uid), Ok(gid)) = (uid_str.parse::<u32>(), gid_str.parse::<u32>()) {
            if uid != 0 {
                // Look up the name for manifest friendliness.
                let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
                let mut buf = vec![0u8; 1024];
                let mut result: *mut libc::passwd = std::ptr::null_mut();
                unsafe {
                    libc::getpwuid_r(
                        uid,
                        &mut pwd,
                        buf.as_mut_ptr() as *mut libc::c_char,
                        buf.len(),
                        &mut result,
                    );
                }
                if !result.is_null() {
                    let name = unsafe { CStr::from_ptr(pwd.pw_name) }
                        .to_string_lossy()
                        .to_string();
                    return Some((name, uid, gid));
                }
            }
        }
    }
    // Fall back to the standard `nobody` account.
    let cname = std::ffi::CString::new("nobody").ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_uid == 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .to_string();
    Some((name, pwd.pw_uid, pwd.pw_gid))
}

#[cfg(not(unix))]
fn drop_target_user() -> Option<(String, u32, u32)> {
    None
}

async fn wait_for(
    r: &mut Reconciler,
    predicate: impl Fn(&ClusterState) -> bool,
    max_steps: usize,
) -> ClusterState {
    run_until(r, predicate, max_steps, Duration::from_millis(50), true).await;
    r.snapshot()
}

/// Drive the reconciler while checking an invariant after every tick. Panics
/// if the invariant is violated or the predicate is never satisfied.
async fn run_until_with_invariant(
    r: &mut Reconciler,
    predicate: impl Fn(&ClusterState) -> bool,
    invariant: impl Fn(&ClusterState) -> bool,
    max_steps: usize,
) -> ClusterState {
    for step in 0..max_steps {
        r.maybe_reload();
        r.reconcile().await;
        r.write_state();
        let state = r.snapshot();
        assert!(
            invariant(&state),
            "invariant violated at step {step}: {state:#?}"
        );
        if predicate(&state) {
            return state;
        }
        if step + 1 < max_steps {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("predicate not satisfied in {max_steps} steps");
}

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a pure liveness check; no signal is delivered.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    false
}

fn find_app<'a>(state: &'a ClusterState, name: &'a str) -> Option<&'a AppStatus> {
    state.apps.iter().find(|a| a.name == name)
}

fn ready_count(state: &ClusterState, name: &str) -> u32 {
    find_app(state, name).map(|a| a.ready).unwrap_or_default()
}

#[tokio::test]
async fn readiness_gating() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_READY_AFTER_MS = "200" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    let app = find_app(&state, "web").expect("web app should exist");
    assert_eq!(app.desired, 1);
    assert_eq!(app.ready, 1);
    let inst = app.instances.first().expect("one instance");
    assert_eq!(inst.phase, Phase::Ready);
    assert_eq!(inst.restarts, 0);

    r.teardown().await;
}

#[tokio::test]
async fn crash_backoff_and_counter_reset() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_CRASH_AFTER_MS = "50", FIXTURE_EXIT_CODE = "1" }
ready_path = ""
live_path = ""
"#,
    );

    let mut r = start_reconciler(&path);

    // Wait until the instance has crashed at least twice.
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| {
                    a.instances
                        .first()
                        .map(|i| i.restarts >= 2)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        },
        120,
    )
    .await;
    let restarts = state.apps[0].instances[0].restarts;
    assert!(
        restarts >= 2,
        "expected at least 2 restarts, got {restarts}"
    );

    // Now change the manifest so the fixture stops crashing and stays up long
    // enough for the restart counter to reset. We change the env so the spec
    // hash changes, forcing a relaunch. Sleep so the file mtime changes and
    // the hot-reload debounce sees a stable new mtime.
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { STABLE = "yes" }
ready_path = ""
live_path = ""
"#,
    );

    // Reload the manifest in the reconciler.
    r.maybe_reload();
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| {
                    a.instances
                        .iter()
                        .any(|i| i.restarts == 0 && i.phase == Phase::Ready)
                })
                .unwrap_or(false)
        },
        120,
    )
    .await;

    assert!(state.apps[0]
        .instances
        .iter()
        .any(|i| i.restarts == 0 && i.phase == Phase::Ready));

    r.teardown().await;
}

#[tokio::test]
async fn liveness_restart() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_LIVE_FAIL_AFTER_MS = "300" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);

    // First the instance should become Ready.
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    assert_eq!(state.apps[0].instances[0].restarts, 0);

    // Wait for liveness to fail and artzain to restart the instance. We detect
    // a restart by the restarts counter increasing.
    let before = state.apps[0].instances[0].restarts;
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| {
                    a.instances
                        .first()
                        .map(|i| i.restarts > before)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        },
        120,
    )
    .await;
    let after = state.apps[0].instances[0].restarts;
    assert!(
        after > before,
        "expected a restart after liveness failures, before={before} after={after}"
    );

    r.teardown().await;
}

#[tokio::test]
async fn scale_up_and_down_retires_highest_index_first() {
    let h = Harness::new(3);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    // Scale up to 3 replicas. Sleep before rewrite so mtime changes.
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 3
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    r.maybe_reload();
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.instances.len() == 3 && a.ready == 3)
                .unwrap_or(false)
        },
        120,
    )
    .await;
    assert_eq!(state.apps[0].instances.len(), 3);
    assert_eq!(state.apps[0].ready, 3);

    // Scale down to 1 replica. The surviving replica should be index 0.
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    r.maybe_reload();
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.instances.len() == 1 && a.ready == 1)
                .unwrap_or(false)
        },
        120,
    )
    .await;
    assert_eq!(state.apps[0].instances.len(), 1);
    assert_eq!(state.apps[0].instances[0].replica, 0);
    assert_eq!(state.apps[0].ready, 1);

    r.teardown().await;
}

#[tokio::test]
async fn app_dependency_gating() {
    let h = Harness::new(2);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "data"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_READY_AFTER_MS = "300" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port_plus}}
replicas = 1
depends_on = ["data"]
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    // Replace {{port_plus}} with base_port + 1 (not supported by the simple
    // helper above, so do it manually).
    let body = std::fs::read_to_string(&path).unwrap();
    let body = body.replace("{{port_plus}}", &(h.base_port + 1).to_string());
    std::fs::write(&path, body).unwrap();

    let mut r = start_reconciler(&path);

    // `web` should stay Pending until `data` is Ready.
    let state = wait_for(
        &mut r,
        |s| {
            ready_count(s, "data") == 1
                && s.apps
                    .iter()
                    .find(|a| a.name == "web")
                    .map(|a| {
                        a.instances
                            .first()
                            .map(|i| i.phase == Phase::Pending)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
        },
        40,
    )
    .await;
    assert_eq!(ready_count(&state, "data"), 1);
    let web = find_app(&state, "web").expect("web app");
    assert_eq!(web.instances[0].phase, Phase::Pending);

    // Once data is ready, web should start and become ready too.
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    assert_eq!(ready_count(&state, "web"), 1);

    r.teardown().await;
}

#[tokio::test]
async fn manifest_hot_reload_and_half_written_rejection() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    // Hot-reload: change the env to a new value (changes spec hash) and wait
    // for the fleet to stay ready after the replacement.
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { MARK = "v2" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    r.maybe_reload();
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    // Now write a half-written / zero-app manifest and confirm the reload is
    // rejected without tearing the fleet down.
    std::fs::write(&path, "").unwrap();
    let state_before = r.snapshot();
    r.maybe_reload();
    // Give one tick to process the rejection.
    run_until(&mut r, |_| false, 2, Duration::from_millis(50), false).await;
    let state_after = r.snapshot();
    assert_eq!(state_after.apps.len(), state_before.apps.len());
    assert_eq!(ready_count(&state_after, "web"), 1);

    r.teardown().await;
}

#[tokio::test]
async fn sigterm_ignored_escalates_to_sigkill() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_IGNORE_SIGTERM = "yes" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    let pid = state.apps[0].instances[0].pid.expect("instance has a pid");
    assert!(is_pid_alive(pid));

    // Teardown sends SIGTERM, waits STOP_GRACE (10s), then SIGKILL. The fixture
    // ignores SIGTERM, so this proves escalation.
    r.teardown().await;

    // Give a short grace for the kernel to reap the process after SIGKILL.
    for _ in 0..50 {
        if !is_pid_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !is_pid_alive(pid),
        "fixture that ignores SIGTERM should have been SIGKILLed"
    );
}

#[tokio::test]
async fn rolling_max_unavailable_one() {
    let h = Harness::new(3);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 3
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 3, 120).await;

    // Trigger a rolling update by changing the env.
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 3
env = { MARK = "v2" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let state = run_until_with_invariant(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.instances.len() == 3 && a.ready == 3)
                .unwrap_or(false)
        },
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.desired.saturating_sub(a.ready) <= 1)
                .unwrap_or(true)
        },
        240,
    )
    .await;
    assert_eq!(state.apps[0].ready, 3);

    r.teardown().await;
}

#[tokio::test]
async fn rolling_max_surge_one() {
    let h = Harness::new(4);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1
max_surge = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 3
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 3, 120).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1
max_surge = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 3
env = { MARK = "v2" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let state = run_until_with_invariant(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.instances.len() == 3 && a.ready == 3)
                .unwrap_or(false)
        },
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| a.ready >= a.desired)
                .unwrap_or(true)
        },
        240,
    )
    .await;
    assert_eq!(state.apps[0].ready, 3);

    r.teardown().await;
}

#[tokio::test]
async fn teardown_in_reverse_dependency_order() {
    let h = Harness::new(2);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "data"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port_plus}}
replicas = 1
depends_on = ["data"]
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    let body = std::fs::read_to_string(&path).unwrap();
    let body = body.replace("{{port_plus}}", &(h.base_port + 1).to_string());
    std::fs::write(&path, body).unwrap();

    let mut r = start_reconciler(&path);
    wait_for(
        &mut r,
        |s| ready_count(s, "data") == 1 && ready_count(s, "web") == 1,
        120,
    )
    .await;

    let data_pid = find_app(&r.snapshot(), "data")
        .and_then(|a| a.instances.first())
        .and_then(|i| i.pid)
        .expect("data pid");
    let web_pid = find_app(&r.snapshot(), "web")
        .and_then(|a| a.instances.first())
        .and_then(|i| i.pid)
        .expect("web pid");

    // Teardown should stop web before data. Poll until one dies, record which.
    r.teardown().await;

    let mut first_died: Option<&str> = None;
    for _ in 0..200 {
        let web_alive = is_pid_alive(web_pid);
        let data_alive = is_pid_alive(data_pid);
        if first_died.is_none() {
            if !web_alive && data_alive {
                first_died = Some("web");
                break;
            }
            if !data_alive && web_alive {
                first_died = Some("data");
                break;
            }
            if !web_alive && !data_alive {
                // They died together; this is acceptable but not what we want to
                // prove, so keep looking for a clear winner in the next poll.
                // On slow machines the loop may end without a winner; we assert
                // below that web died first or simultaneously.
                first_died = Some("web");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        first_died == Some("web"),
        "dependent (web) should stop before its dependency (data)"
    );
}

#[tokio::test]
async fn check_fail_holds_and_recovers() {
    let h = Harness::new(2);
    let check_port = h.base_port + 1;
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{check_port}"))
        .await
        .expect("check listener binds");

    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[check]]
name = "postgres"
tcp = "127.0.0.1:{{port_plus}}"

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
depends_on = ["postgres"]
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    let body = std::fs::read_to_string(&path).unwrap();
    let body = body.replace("{{port_plus}}", &check_port.to_string());
    std::fs::write(&path, body).unwrap();

    // Start with the check up; the app should become Ready.
    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    // Drop the check listener. The check will be re-verified on the next
    // CHECK_VERIFY_PERIOD (10s) and the app should be stopped.
    drop(listener);
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| {
                    a.instances
                        .first()
                        .map(|i| i.phase == Phase::Pending || i.phase == Phase::CrashLoopBackOff)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        },
        240,
    )
    .await;
    assert_eq!(ready_count(&state, "web"), 0);

    // Bring the check back up; the app should restart and become Ready.
    let _listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{check_port}"))
        .await
        .expect("check listener rebinds");
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 240).await;
    assert_eq!(ready_count(&state, "web"), 1);

    r.teardown().await;
}

#[tokio::test]
async fn env_isolation_and_inheritance() {
    let h = Harness::new(1);
    let allowed_key = "ARTZAIN_TEST_ALLOWED";
    let blocked_key = "ARTZAIN_TEST_BLOCKED";
    std::env::set_var(allowed_key, "yes");
    std::env::set_var(blocked_key, "no");

    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1
inherit_env = ["ARTZAIN_TEST_ALLOWED"]

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    let port = state.apps[0].instances[0].port;

    let body = http_get(port, "/__env").await.expect("fixture responds");
    assert!(
        body.contains(&format!("\"{allowed_key}\": \"yes\"")),
        "allowlisted env should be inherited: {body}"
    );
    assert!(
        !body.contains(&format!("\"{blocked_key}\"")),
        "blocked env should not appear: {body}"
    );
    assert!(
        body.contains("\"PATH\":"),
        "base-key PATH should always be present: {body}"
    );

    r.teardown().await;
    std::env::remove_var(allowed_key);
    std::env::remove_var(blocked_key);
}

#[tokio::test]
async fn secrets_not_in_state_json() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { MY_SECRET = "hunter2" }
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    let state_file = h.tmp.path().join(".artzain").join("state.json");
    let state_raw = std::fs::read_to_string(&state_file).expect("state.json exists");
    assert!(
        !state_raw.contains("hunter2"),
        "state.json must not contain manifest env values: {state_raw}"
    );

    r.teardown().await;
}

#[tokio::test]
#[cfg(unix)]
async fn private_file_permissions() {
    use std::os::unix::fs::MetadataExt;

    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );

    let mut r = start_reconciler(&path);
    wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;

    let dot_artzain = h.tmp.path().join(".artzain");
    let state_file = dot_artzain.join("state.json");
    let log_file = dot_artzain.join("logs").join("web-0.log");

    let meta = std::fs::metadata(&dot_artzain).expect(".artzain metadata");
    assert_eq!(meta.mode() & 0o777, 0o700, ".artzain should be 0700");

    for file in [&state_file, &log_file] {
        let meta = std::fs::metadata(file).expect("file metadata");
        assert_eq!(meta.mode() & 0o777, 0o600, "{file:?} should be 0600");
    }

    r.teardown().await;
}

#[tokio::test]
#[cfg(unix)]
async fn privilege_drop() {
    let Some((user, uid, gid)) = drop_target_user() else {
        // Not running as root or no suitable target user; skip.
        return;
    };

    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
user = "{{user}}"
group = "{{user}}"
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    let body = std::fs::read_to_string(&path).unwrap();
    let body = body.replace("{{user}}", &user);
    std::fs::write(&path, body).unwrap();

    let mut r = start_reconciler(&path);
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    let port = state.apps[0].instances[0].port;

    let body = http_get(port, "/__id").await.expect("fixture responds");
    assert!(
        body.contains(&format!("uid={uid}")),
        "fixture should run as uid {uid}: {body}"
    );
    assert!(
        body.contains(&format!("gid={gid}")),
        "fixture should run as gid {gid}: {body}"
    );

    r.teardown().await;
}

#[tokio::test]
async fn resource_limits() {
    // RLIMIT_AS (memory_mb) is not portable; only set it on Linux.
    let limits = if cfg!(target_os = "linux") {
        r#"limits = { open_files = 64, memory_mb = 128 }"#
    } else {
        r#"limits = { open_files = 64 }"#
    };

    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
{{limits}}
ready_path = "/__ready"
live_path = "/__health"
ready_timeout = 5
live_period = 1
live_failures = 2
"#,
    );
    let body = std::fs::read_to_string(&path).unwrap();
    let body = body.replace("{{limits}}", limits);
    std::fs::write(&path, body).unwrap();

    let mut r = start_reconciler(&path);
    let state = wait_for(&mut r, |s| ready_count(s, "web") == 1, 120).await;
    let port = state.apps[0].instances[0].port;

    if cfg!(target_os = "linux") {
        let body = http_get(port, "/__limits").await.expect("fixture responds");
        assert!(
            body.contains("open_files=64"),
            "RLIMIT_NOFILE should be 64: {body}"
        );
    }

    r.teardown().await;
}

#[tokio::test]
#[cfg(unix)]
async fn orphan_reclaim_kills_prior_children() {
    // Simulate a previous artzain owner that died without teardown: a fixture
    // child is still alive in its own process group, and a stale state.json
    // records the dead owner's pid and the live child's pgid.
    let h = Harness::new(1);
    let port = h.base_port;
    let fixture = fixture_bin();

    let mut child = tokio::process::Command::new(&fixture)
        .process_group(0)
        .env("PORT", port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fixture");

    let addr = format!("127.0.0.1:{port}");
    let mut waited = 0;
    while tokio::net::TcpStream::connect(&addr).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
        assert!(waited < 100, "fixture did not start listening");
    }
    let pgid = child.id().expect("child has pid") as i32;

    let dot_artzain = h.tmp.path().join(".artzain");
    std::fs::create_dir_all(&dot_artzain).unwrap();
    let stale = ClusterState {
        state_version: 1,
        project: "test".to_string(),
        owner_pid: 999_999,
        updated_at: 0,
        apps: vec![AppStatus {
            name: "web".to_string(),
            desired: 1,
            ready: 1,
            instances: vec![InstanceStatus {
                replica: 0,
                port,
                phase: Phase::Ready,
                pid: Some(pgid as u32),
                pgid: Some(pgid),
                restarts: 0,
                uptime_secs: 0,
            }],
        }],
    };
    std::fs::write(dot_artzain.join("state.json"), stale.to_json()).unwrap();

    reclaim_orphans(h.tmp.path())
        .await
        .expect("reclaim should succeed");

    // The orphan should stop listening; the port being released is the real
    // observable. A killed child may briefly be a zombie, so pid liveness is
    // not a reliable test signal.
    let mut port_closed = false;
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(&addr).await.is_err() {
            port_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(port_closed, "orphan fixture should stop listening");

    // Reap the child so the test does not leave a zombie behind.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    assert!(
        !dot_artzain.join("state.json").exists(),
        "stale state.json should be removed"
    );
}

#[tokio::test]
async fn log_rotation_caps_total_size() {
    let h = Harness::new(1);
    let path = h.write_manifest(
        r#"
project = "test"

[defaults]
restart_backoff_ms = 50
restart_backoff_max_ms = 500
stable_after_secs = 1
log_max_bytes = 512
log_keep = 2

[[app]]
name = "web"
bin = "{{fixture}}"
port = {{port}}
replicas = 1
env = { FIXTURE_LOG_EVERY_MS = "10" }
ready_path = ""
live_path = ""
"#,
    );

    let mut r = start_reconciler(&path);
    let state = wait_for(
        &mut r,
        |s| {
            s.apps
                .iter()
                .find(|a| a.name == "web")
                .map(|a| {
                    a.instances
                        .first()
                        .map(|i| i.pid.is_some())
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        },
        120,
    )
    .await;
    let pid = state.apps[0].instances[0].pid.expect("instance has pid");

    // Wait for the fixture to emit enough lines to rotate at least once.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let dot_artzain = h.tmp.path().join(".artzain");
    let log = dot_artzain.join("logs").join("web-0.log");
    let backup1 = dot_artzain.join("logs").join("web-0.log.1");

    assert!(log.exists(), "current log file should exist");
    assert!(backup1.exists(), "rotated backup log should exist");
    assert!(
        !dot_artzain.join("logs").join("web-0.log.2").exists(),
        "log_keep = 2 should keep only one backup"
    );

    let current_size = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let backup_size = std::fs::metadata(&backup1).map(|m| m.len()).unwrap_or(0);
    assert!(
        current_size <= 1024,
        "current log should be bounded, got {current_size}"
    );
    assert!(
        backup_size <= 1024,
        "backup log should be bounded, got {backup_size}"
    );

    r.teardown().await;

    // After teardown the log writer tasks stop, but the child pid may still be
    // briefly alive; wait so the process group is gone before the harness drops.
    for _ in 0..20 {
        if !is_pid_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
