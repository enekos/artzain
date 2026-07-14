# artzain

**A tiny, k8s-shaped orchestrator for prebuilt binaries.** One declarative
`artzain.toml` describes the apps you want running and how many replicas of
each; `artzain up` runs a single reconcile loop that keeps reality matching the
file — starting processes, restarting crashed ones with exponential backoff,
driving HTTP readiness/liveness probes, rolling apps when the file changes, and
tearing everything down gracefully on Ctrl-C.

*artzain* is Basque for **shepherd** — it keeps the flock of app instances at
the desired count. It's the local-first cousin of [laino](../laino) (the GCP
emulator), reusing laino's process-supervision approach and adding the control
loop that makes it k8s-like: replicas, restart policy, probes, rolling updates.

Built for [sutegi](../sutegi) apps — which already expose `/__ready`,
`/__health`, `/__metrics`, and drain gracefully on SIGTERM — but it supervises
any process that serves HTTP health, or none at all.

## Non-goals (on purpose)

- **No git.** You point it at an already-built binary. It never clones, pulls,
  builds, or watches a repo.
- **No Docker / containers.** Native OS processes in their own process groups.
- **No daemon, no cluster.** `artzain up` is a foreground process; a JSON state
  file is the whole control plane. `status` reads it, `down` signals it.

The goal is a small, predictable, reliable binary — a single sequential
reconcile loop with no shared state and no locks, so the whole system's
behavior happens in one place, one tick at a time.

## Quick start

```toml
# artzain.toml
project = "demo"

[[app]]
name = "web"
bin = "./target/release/web"   # a prebuilt binary
port = 8080
replicas = 3                   # runs on 8080, 8081, 8082
```

```console
$ artzain plan            # validate + show the startup plan
$ artzain up              # reconcile and supervise (Ctrl-C to stop)
$ artzain status          # in another shell
cluster demo  (pid 41233, updated 0s ago)

APP              READY    REPLICA PORT   PHASE    RESTARTS
web              3/3      0       8080   Ready    0 (12s up)
web              3/3      1       8081   Ready    0 (12s up)
web              3/3      2       8082   Ready    0 (12s up)

$ artzain down            # stop the running cluster
```

## Commands

| command | what it does |
|---|---|
| `artzain up [--only a,b] [--no-watch]` | Reconcile the manifest and supervise it (foreground). |
| `artzain plan [--only a,b]` | Validate the manifest and print what `up` would do. |
| `artzain status` | Print the state of a running cluster. |
| `artzain logs [app]` | Print persisted logs from the running cluster. |
| `artzain down [--force]` | Signal a running cluster to shut down. `--force` skips the lock-file safety check. |

`-f/--file` points at a manifest other than `./artzain.toml`.

## How it maps to Kubernetes

| Kubernetes | artzain |
|---|---|
| Deployment | `[[app]]` |
| Pod / replica | one process instance on `port + i` |
| readinessProbe | `ready_path` (HTTP 2xx/3xx) → the `Ready` phase, counts toward `replicas` |
| livenessProbe | `live_path`; `live_failures` consecutive fails → restart |
| restartPolicy: Always + CrashLoopBackOff | exponential backoff, reset after `stable_after_secs` uptime |
| RollingUpdate (maxUnavailable=1) | edit the manifest → stale instances replaced one at a time |
| RollingUpdate (maxSurge=1) | set `[defaults].max_surge = 1`; new instance starts on a temp port before the old one stops |
| `kubectl apply` | save the manifest file — `up` watches and reconciles |
| `kubectl get pods` | `artzain status` |
| `kubectl logs` | `artzain logs` |

### Reconcile loop, per tick (500ms)

1. **Reload** the manifest if it changed on disk (debounced one tick, and a
   parse error or a zero-app file is rejected — a half-written file never takes
   the fleet down).
2. **Re-verify** `[[check]]` entries every 10s; apps whose check deps fail are
   stopped and held until the check recovers.
3. **Reap** exited children; crashes get exponential backoff.
4. **Terminate** instances no longer desired (scale-down, app removed); higher
   replica indices retire first.
5. **Roll** stale instances (spec changed) one per app, respecting
   maxUnavailable; non-Ready stale instances are also rolled immediately. With
   `max_surge > 0`, new instances start on a temporary surge port before the
   old one is stopped.
6. **Spawn** missing/backed-off slots whose app dependencies are `Ready` and
   whose checks are still passing.
7. **Probe** readiness and liveness.
8. **Write** the state snapshot.

## Manifest reference

```toml
project = "demo"                       # cosmetic label

[vars]                                 # ${name} interpolation; ARTZAIN_VAR_<NAME> overrides
root = "/srv/app"                      # use $$ for a literal $, e.g. "$${name}" or "$$HOME"

[defaults]
env = { RUST_LOG = "info" }            # merged into every app (app env wins)
restart_backoff_ms = 500               # base crash backoff (doubles each crash)
restart_backoff_max_ms = 30000         # backoff cap
stable_after_secs = 10                 # uptime that resets the crash counter
max_surge = 0                          # 0 = replace one-for-one; 1 = zero-downtime on temp port

[[check]]                              # external dep, verified once before apps start
name = "postgres"
tcp = "127.0.0.1:5432"                 # or: http = "127.0.0.1:8000/health"
hint = "brew services start postgresql@17"

[[app]]
name = "web"
bin = "${root}/target/release/web"     # prebuilt binary (resolved vs manifest dir)
args = []                              # extra argv
dir = "."                              # working dir (default: manifest dir)
replicas = 3                           # instances; replica i binds port + i
port = 8080
env = { DATABASE_URL = "..." }         # PORT / ARTZAIN_APP / ARTZAIN_REPLICA also injected
depends_on = ["postgres"]              # checks or apps; app deps must be Ready first
ready_path = "/__ready"                # HTTP readiness ("" = Ready once spawned)
live_path = "/__health"                # HTTP liveness ("" = no liveness probing)
ready_timeout = 60                     # seconds to first-ready before a restart
live_period = 5                        # seconds between liveness probes
live_failures = 3                      # consecutive liveness fails that restart
```

## Layout

- `crates/artzain-core` — manifest, probes, process control, reconcile loop, state.
- `crates/artzain` — the CLI.

## Releases

Prebuilt Linux x86_64 binaries are attached to [GitHub releases](https://github.com/enekos/artzain/releases). The `Release` workflow (`.github/workflows/release.yml`) builds `artzain` on every `v*.*.*` tag and publishes `artzain-<version>-x86_64-unknown-linux-gnu.tar.gz`.

## License

MIT
