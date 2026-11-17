//! `artzain.toml` — the declarative desired state. A set of `[[app]]`
//! entries (each ≈ a Kubernetes Deployment: a prebuilt binary run at some
//! replica count), optional `[[check]]` external dependencies verified before
//! anything starts, and `[defaults]` env merged into every app.
//!
//! The manifest is the single source of truth. `artzain up` watches this file
//! and rolls the running apps toward whatever it says — editing and saving the
//! file IS the apply.

use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Environment keys artzain always carries into every child, inherited from
/// the parent process if present. They are not hashed as part of the spec,
/// so parent-env drift does not roll the app. Missing keys are omitted except
/// `PATH`, which falls back to a safe default so spawned binaries can still
/// exec helpers.
pub const BASE_ENV_KEYS: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL",
];

pub const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Cluster label, purely cosmetic (shown in status). Defaults to `local`.
    pub project: Option<String>,

    /// Values interpolated into the rest of the file as `${name}` before
    /// parsing. Override per machine with `ARTZAIN_VAR_<NAME>` env vars.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,

    #[serde(default)]
    pub defaults: Defaults,

    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,

    #[serde(default, rename = "app")]
    pub apps: Vec<App>,

    /// Directory the manifest was loaded from; relative paths (binaries,
    /// working dirs) resolve against it. Not part of the TOML.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Env merged into every app (app-level entries win).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra env keys inherited from the parent process in addition to the
    /// documented base-key set. Default empty — manifest env is intentional.
    #[serde(default)]
    pub inherit_env: Vec<String>,
    /// Base delay before restarting a crashed instance (doubles each
    /// consecutive crash, capped at `restart_backoff_max_ms`).
    #[serde(default = "default_backoff_ms")]
    pub restart_backoff_ms: u64,
    #[serde(default = "default_backoff_max_ms")]
    pub restart_backoff_max_ms: u64,
    /// An instance that stays up this long resets its crash counter, so a
    /// long-lived process that dies once doesn't inherit an old backoff.
    #[serde(default = "default_stable_secs")]
    pub stable_after_secs: u64,
    /// Maximum extra instances allowed above `replicas` during a rolling
    /// update. `0` means replace one-for-one (maxSurge=0); `1` starts the new
    /// instance on a temporary port before stopping the old one.
    #[serde(default)]
    pub max_surge: u32,
    /// Maximum size of a single log file before it is rotated. Default 10 MiB.
    #[serde(default = "default_log_max_bytes")]
    pub log_max_bytes: u64,
    /// Number of log files to keep (current + backups). Default 3.
    #[serde(default = "default_log_keep")]
    pub log_keep: u32,
}

fn default_backoff_ms() -> u64 {
    500
}
fn default_backoff_max_ms() -> u64 {
    30_000
}
fn default_stable_secs() -> u64 {
    10
}
fn default_log_max_bytes() -> u64 {
    10 * 1024 * 1024
}
fn default_log_keep() -> u32 {
    3
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            inherit_env: Vec::new(),
            restart_backoff_ms: default_backoff_ms(),
            restart_backoff_max_ms: default_backoff_max_ms(),
            stable_after_secs: default_stable_secs(),
            max_surge: 0,
            log_max_bytes: default_log_max_bytes(),
            log_keep: default_log_keep(),
        }
    }
}

/// External infrastructure artzain verifies but does not manage (Postgres,
/// Redis, another service). Checked once before apps start; an app may
/// `depends_on` a check to defer its own start until the dep answers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    /// `host:port` to probe with a TCP connect.
    pub tcp: Option<String>,
    /// `host:port` + path answered with a 2xx/3xx, e.g. `127.0.0.1:5432` is
    /// TCP-only; use `tcp` for that. `http` is `host:port/path`.
    pub http: Option<String>,
    /// Shown when the check fails, e.g. `brew services start postgresql@17`.
    pub hint: Option<String>,
}

/// Resource limits applied to the child before exec. These are advisory on
/// some platforms; artzain applies them fail-closed before exec.
#[derive(Debug, Clone, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    pub open_files: Option<u64>,
    /// Maximum virtual memory in MiB (`RLIMIT_AS`).
    pub memory_mb: Option<u64>,
}

/// One managed workload: a prebuilt binary run at `replicas` count. Replica
/// `i` binds `port + i` (injected as `PORT`); readiness/liveness are probed
/// over HTTP on that port. ≈ a Deployment.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub name: String,
    /// Path to the already-built binary (resolved against the manifest dir).
    /// artzain runs it; it does not build. No git, no Docker.
    pub bin: PathBuf,
    /// Extra argv passed to the binary.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory (resolved against the manifest dir). Defaults to the
    /// manifest dir.
    pub dir: Option<PathBuf>,
    /// How many instances to keep running.
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    /// Base TCP port. Replica `i` gets `port + i`, injected as `PORT` env.
    pub port: u16,
    /// Env for this app (merged over `[defaults].env`; both merged over the
    /// inherited process env). `PORT`/`ARTZAIN_REPLICA` are set per instance.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Checks or other apps that must be ready before this app starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// HTTP readiness path. When it answers 2xx/3xx the instance is `Ready`
    /// and counts toward `replicas`. sutegi apps expose `/__ready`.
    #[serde(default = "default_ready_path")]
    pub ready_path: String,
    /// HTTP liveness path. Consecutive failures restart the instance. sutegi
    /// apps expose `/__health`. Empty string disables liveness probing.
    #[serde(default = "default_live_path")]
    pub live_path: String,
    /// Seconds to reach first-ready before the instance is declared failed and
    /// backed off.
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout: u64,
    /// Seconds between liveness probes.
    #[serde(default = "default_live_period")]
    pub live_period: u64,
    /// Consecutive liveness failures that trigger a restart.
    #[serde(default = "default_live_failures")]
    pub live_failures: u32,
    /// Run the app as this user name (resolved to uid at load). On non-Unix
    /// platforms this is a load error.
    pub user: Option<String>,
    /// Run the app as this group name (resolved to gid at load). If omitted
    /// and `user` is set, the user's primary group is used.
    pub group: Option<String>,
    /// Optional per-app resource limits.
    pub limits: Option<Limits>,
    /// Resolved uid for `user`, filled at load time.
    #[serde(skip)]
    pub uid: Option<u32>,
    /// Resolved gid for `group` (or user's primary group), filled at load time.
    #[serde(skip)]
    pub gid: Option<u32>,
}

fn default_replicas() -> u32 {
    1
}
fn default_ready_path() -> String {
    "/__ready".to_string()
}
fn default_live_path() -> String {
    "/__health".to_string()
}
fn default_ready_timeout() -> u64 {
    60
}
fn default_live_period() -> u64 {
    5
}
fn default_live_failures() -> u32 {
    3
}

/// Longest duration any timer in the manifest may ask for. Bigger values
/// would overflow `Instant` arithmetic and panic the reconciler.
const MAX_DURATION_SECS: u64 = 365 * 24 * 60 * 60;
const MAX_NAME_LEN: usize = 64;

/// App and check names become file names (`.artzain/logs/<app>-<n>.log`) and
/// terminal output, so they are restricted to a plain identifier.
fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A probe path is written verbatim into an HTTP request line.
fn valid_probe_path(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn valid_addr(addr: &str) -> bool {
    !addr.is_empty() && !addr.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading manifest {}: {e}", path.display()))?;
        Self::parse(&raw, path)
    }

    pub fn parse(raw: &str, path: &Path) -> anyhow::Result<Self> {
        let raw = interpolate_vars(raw, path)?;
        let mut manifest: Manifest = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing manifest {}: {e}", path.display()))?;
        manifest.base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn project(&self) -> &str {
        self.project.as_deref().unwrap_or("local")
    }

    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }

    pub fn app(&self, name: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.name == name)
    }

    fn validate(&mut self) -> anyhow::Result<()> {
        let mut errors = Vec::new();
        let mut names = HashSet::new();
        let secs = |field: &str, v: u64, errors: &mut Vec<String>| {
            if v > MAX_DURATION_SECS {
                errors.push(format!("{field} = {v} exceeds {MAX_DURATION_SECS}"));
            }
        };
        let d = &self.defaults;
        secs(
            "defaults.stable_after_secs",
            d.stable_after_secs,
            &mut errors,
        );
        secs(
            "defaults.restart_backoff_ms",
            d.restart_backoff_ms / 1000,
            &mut errors,
        );
        secs(
            "defaults.restart_backoff_max_ms",
            d.restart_backoff_max_ms / 1000,
            &mut errors,
        );
        for a in &self.apps {
            if !valid_name(&a.name) {
                errors.push(format!(
                    "app name `{}` must be 1-{MAX_NAME_LEN} of [A-Za-z0-9._-], starting alphanumeric",
                    a.name.escape_debug()
                ));
            }
            secs(
                &format!("app `{}` ready_timeout", a.name),
                a.ready_timeout,
                &mut errors,
            );
            secs(
                &format!("app `{}` live_period", a.name),
                a.live_period,
                &mut errors,
            );
            for (field, path) in [("ready_path", &a.ready_path), ("live_path", &a.live_path)] {
                if !path.is_empty() && !valid_probe_path(path) {
                    errors.push(format!(
                        "app `{}`: {field} must start with `/` and contain no whitespace",
                        a.name
                    ));
                }
            }
            if a.group.is_some() && a.user.is_none() {
                errors.push(format!(
                    "app `{}`: `group` without `user` would still run as artzain's user",
                    a.name
                ));
            }
            if let Some(mb) = a.limits.as_ref().and_then(|l| l.memory_mb)
                && mb.checked_mul(1024 * 1024).is_none()
            {
                errors.push(format!(
                    "app `{}`: limits.memory_mb = {mb} overflows",
                    a.name
                ));
            }
            if !names.insert(a.name.as_str()) {
                errors.push(format!("duplicate app name `{}`", a.name));
            }
            if a.replicas == 0 {
                errors.push(format!("app `{}` has replicas = 0", a.name));
            }
            // Guard the port window: replica i binds port + i, which must not
            // overflow u16.
            if a.replicas > 1 && (a.port as u32 + a.replicas - 1) > u16::MAX as u32 {
                errors.push(format!(
                    "app `{}`: port {} + {} replicas overflows the port range",
                    a.name, a.port, a.replicas
                ));
            }
        }
        for c in &self.checks {
            if !valid_name(&c.name) {
                errors.push(format!(
                    "check name `{}` must be 1-{MAX_NAME_LEN} of [A-Za-z0-9._-], starting alphanumeric",
                    c.name.escape_debug()
                ));
            }
            if c.tcp.as_deref().is_some_and(|a| !valid_addr(a))
                || c.http.as_deref().is_some_and(|a| !valid_addr(a))
            {
                errors.push(format!("check `{}`: address contains whitespace", c.name));
            }
            if !names.insert(c.name.as_str()) {
                errors.push(format!("check `{}` collides with another name", c.name));
            }
            if c.tcp.is_none() && c.http.is_none() {
                errors.push(format!("check `{}` needs `tcp` or `http`", c.name));
            }
        }

        // Port collisions across apps (each app owns [port, port+replicas)).
        let mut claimed: BTreeMap<u16, String> = BTreeMap::new();
        for a in &self.apps {
            for i in 0..a.replicas {
                let p = a.port + i as u16;
                if let Some(owner) = claimed.insert(p, a.name.clone()) {
                    errors.push(format!(
                        "port {p} claimed by both `{owner}` and `{}`",
                        a.name
                    ));
                }
            }
        }

        for a in &self.apps {
            for dep in &a.depends_on {
                if !names.contains(dep.as_str()) {
                    errors.push(format!(
                        "app `{}` depends on unknown `{dep}` (not an app or check)",
                        a.name
                    ));
                }
            }
        }

        // Resolve user/group names to uid/gid and store them for spawn.
        for a in &mut self.apps {
            let mut uid = None;
            let mut gid = None;
            if let Some(user) = &a.user {
                match crate::sys::lookup_user(user) {
                    Some((u, primary_gid)) => {
                        uid = Some(u);
                        gid = Some(primary_gid);
                    }
                    None => errors.push(format!("app `{}`: unknown user `{user}`", a.name)),
                }
            }
            if let Some(group) = &a.group {
                match crate::sys::lookup_group(group) {
                    Some(g) => gid = Some(g),
                    None => errors.push(format!("app `{}`: unknown group `{group}`", a.name)),
                }
            }
            a.uid = uid;
            a.gid = gid;
        }

        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("invalid manifest:\n  - {}", errors.join("\n  - "))
        }
    }

    /// Apps in dependency order (checks are verified up front, not returned).
    /// Fails on cycles. This is start order; teardown is the reverse.
    pub fn apps_ordered(&self) -> anyhow::Result<Vec<&App>> {
        let by_name: BTreeMap<&str, &App> =
            self.apps.iter().map(|a| (a.name.as_str(), a)).collect();
        let mut ordered = Vec::new();
        let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = visiting, 2 = done

        fn visit<'a>(
            name: &'a str,
            by_name: &BTreeMap<&'a str, &'a App>,
            state: &mut BTreeMap<&'a str, u8>,
            ordered: &mut Vec<&'a App>,
        ) -> anyhow::Result<()> {
            match state.get(name) {
                Some(2) => return Ok(()),
                Some(1) => anyhow::bail!("dependency cycle through app `{name}`"),
                _ => {}
            }
            let Some(app) = by_name.get(name) else {
                return Ok(()); // a check — verified elsewhere
            };
            state.insert(name, 1);
            for dep in &app.depends_on {
                visit(dep, by_name, state, ordered)?;
            }
            state.insert(name, 2);
            ordered.push(app);
            Ok(())
        }

        for a in &self.apps {
            visit(a.name.as_str(), &by_name, &mut state, &mut ordered)?;
        }
        Ok(ordered)
    }
}

/// Substitute `${name}` for every entry of the manifest's `[vars]` table (an
/// `ARTZAIN_VAR_<NAME>` env var overrides the file value). `$$` escapes a
/// literal `$`, so values like `$$HOME` or `$${foo}` are preserved. Unknown
/// `${...}` references are errors — typos should not silently parse.
///
/// Substitution is textual, so a value that could close a TOML string (a
/// quote, a backslash, a newline) is rejected: otherwise an
/// `ARTZAIN_VAR_*` value could inject whole `[[app]]` tables.
fn interpolate_vars(raw: &str, path: &Path) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct VarsOnly {
        #[serde(default)]
        vars: BTreeMap<String, String>,
    }
    let vars = toml::from_str::<VarsOnly>(raw)
        .map(|v| v.vars)
        .unwrap_or_default();
    let resolve = |name: &str| -> Option<String> {
        std::env::var(format!("ARTZAIN_VAR_{}", name.to_uppercase()))
            .ok()
            .or_else(|| vars.get(name).cloned())
    };
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            // Escaped literal '$'.
            out.push('$');
            rest = stripped;
            continue;
        }
        if !after.starts_with('{') {
            // Bare '$' that is not an escape or interpolation — keep it.
            out.push('$');
            rest = after;
            continue;
        }
        // '${...}' interpolation.
        let after = &after[1..];
        let Some(end) = after.find('}') else {
            anyhow::bail!("{}: unterminated ${{...}} reference", path.display());
        };
        let name = &after[..end];
        match resolve(name) {
            Some(value)
                if value
                    .chars()
                    .any(|c| matches!(c, '"' | '\'' | '\\') || c.is_control()) =>
            {
                anyhow::bail!(
                    "{}: variable ${{{name}}} contains a quote, backslash or control character",
                    path.display()
                )
            }
            Some(value) => out.push_str(&value),
            None => anyhow::bail!(
                "{}: unknown variable ${{{name}}} — define it under [vars] or set ARTZAIN_VAR_{}",
                path.display(),
                name.to_uppercase()
            ),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> anyhow::Result<Manifest> {
        Manifest::parse(src, Path::new("artzain.toml"))
    }

    #[test]
    fn minimal_app_defaults() {
        let m = parse(
            r#"
[[app]]
name = "todo"
bin = "./todo"
port = 8080
"#,
        )
        .unwrap();
        let a = &m.apps[0];
        assert_eq!(a.replicas, 1);
        assert_eq!(a.ready_path, "/__ready");
        assert_eq!(a.live_path, "/__health");
        assert_eq!(m.project(), "local");
    }

    #[test]
    fn rejects_zero_replicas() {
        let err = parse(
            r#"
[[app]]
name = "x"
bin = "./x"
port = 8080
replicas = 0
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("replicas = 0"));
    }

    #[test]
    fn detects_port_collision_across_replicas() {
        // app `a` owns 8080..=8081, app `b` claims 8081 too.
        let err = parse(
            r#"
[[app]]
name = "a"
bin = "./a"
port = 8080
replicas = 2

[[app]]
name = "b"
bin = "./b"
port = 8081
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("port 8081 claimed by both"));
    }

    #[test]
    fn orders_apps_by_dependency() {
        let m = parse(
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
"#,
        )
        .unwrap();
        let order: Vec<_> = m
            .apps_ordered()
            .unwrap()
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(order, vec!["data", "gateway"]);
    }

    #[test]
    fn detects_cycles() {
        let m = parse(
            r#"
[[app]]
name = "a"
bin = "./a"
port = 8080
depends_on = ["b"]

[[app]]
name = "b"
bin = "./b"
port = 9000
depends_on = ["a"]
"#,
        )
        .unwrap();
        assert!(m.apps_ordered().is_err());
    }

    #[test]
    fn depends_on_a_check_is_valid() {
        let m = parse(
            r#"
[[check]]
name = "postgres"
tcp = "127.0.0.1:5432"

[[app]]
name = "data"
bin = "./data"
port = 8080
depends_on = ["postgres"]
"#,
        )
        .unwrap();
        // The check is not an app, so it's not in the start order, but the
        // dependency validates.
        assert_eq!(m.apps_ordered().unwrap().len(), 1);
    }

    #[test]
    fn var_interpolation_escapes_dollar_dollar() {
        let m = parse(
            r#"
[vars]
root = "/srv/app"

[[app]]
name = "data"
bin = "$${root}/data"
port = 8080
"#,
        )
        .unwrap();
        // $$ becomes a literal $, so the bin is "${root}/data", not "/srv/app/data".
        assert_eq!(m.apps[0].bin, PathBuf::from("${root}/data"));

        // Mixed interpolation and escaping.
        let m2 = parse(
            r#"
[vars]
root = "/srv/app"

[[app]]
name = "data"
bin = "${root}/data-$$HOME"
port = 8080
"#,
        )
        .unwrap();
        assert_eq!(m2.apps[0].bin, PathBuf::from("/srv/app/data-$HOME"));
    }

    #[test]
    fn var_values_cannot_break_out_of_a_string() {
        let err = parse(
            r#"
[vars]
root = "x\"\n[[app]]\nname = \"evil\""

[[app]]
name = "data"
bin = "${root}/data"
port = 8080
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("quote, backslash or control"));
    }

    #[test]
    fn rejects_names_that_escape_the_log_dir() {
        for name in ["../../etc/cron.d/x", "a/b", ".hidden", "", "x\\u001b[2J"] {
            let src = format!("[[app]]\nname = \"{name}\"\nbin = \"./x\"\nport = 8080\n");
            let err = parse(&src).unwrap_err();
            assert!(err.to_string().contains("app name"), "{name}: {err}");
        }
    }

    #[test]
    fn rejects_probe_paths_that_inject_headers() {
        let err = parse(
            r#"
[[app]]
name = "x"
bin = "./x"
port = 8080
ready_path = "/r HTTP/1.1\r\nX-Evil: 1"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ready_path"));
    }

    #[test]
    fn rejects_timers_that_would_overflow() {
        let err = parse(
            r#"
[defaults]
restart_backoff_max_ms = 9223372036854775807

[[app]]
name = "x"
bin = "./x"
port = 8080
ready_timeout = 9223372036854775807
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("restart_backoff_max_ms"), "{msg}");
        assert!(msg.contains("ready_timeout"), "{msg}");
    }

    #[test]
    fn rejects_memory_limit_overflow_and_group_without_user() {
        let err = parse(
            r#"
[[app]]
name = "x"
bin = "./x"
port = 8080
group = "nogroup"
limits = { memory_mb = 9223372036854775807 }
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("memory_mb"), "{msg}");
        assert!(msg.contains("without `user`"), "{msg}");
    }

    #[test]
    fn bare_dollar_is_preserved() {
        let m = parse(
            r#"
[[app]]
name = "x"
bin = "./$x"
port = 8080
"#,
        )
        .unwrap();
        assert_eq!(m.apps[0].bin, PathBuf::from("./$x"));
    }
}
