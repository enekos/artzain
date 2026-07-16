# Plan 001 — Production hardening (the how)

Implements `spec.md`. Ordered work is in `tasks.md`. Read `../constitution.md`
first — the choices below are constrained by it.

## Sequencing rationale

**M0 (test harness) ships before any hardening.** Every later milestone mutates
the spawn path, the reconcile tick, or startup. Without the safety net from
FR-R4 those changes are unverifiable except by hand — the exact gap this spec
exists to close. So the net comes first; hardening lands against a green suite.

```
M0 Test net ──▶ M1 Security ──▶ M2 Reliability ──▶ M3 Operations ──▶ M4 CI/release
   (FR-R4)        (FR-S*)         (FR-R1,R3)          (FR-O*)          (NFR4)
                                  FR-R2 proven by M0 harness
```

Each milestone is a gate: full suite green + `clippy -D warnings` + `fmt` before
the next starts.

---

## M0 — Test harness & behavior lock (FR-R4, FR-R2)

**New crate `crates/artzain-fixture`** (a bin, workspace-internal, not
released): a tiny, dependency-light HTTP server that is *fault-controllable* via
env/args so tests can script failure:
- serves `/__ready`, `/__health`, `/__metrics` like a sutegi app;
- `FIXTURE_READY_AFTER_MS` — delay before readiness flips true (tests startup +
  `ready_timeout`);
- `FIXTURE_LIVE_FAIL_AFTER_MS` — start returning 500 on `/__health` after N ms
  (tests liveness restart, FR-R2);
- `FIXTURE_CRASH_AFTER_MS` / `FIXTURE_EXIT_CODE` — self-exit (tests crash
  backoff + counter reset);
- `FIXTURE_IGNORE_SIGTERM` — ignore SIGTERM (tests SIGKILL escalation + grace);
- writes its resolved `PORT`/`ARTZAIN_REPLICA` so tests assert per-instance env.

**Integration tests** live in `crates/artzain-core/tests/`. To make the loop
testable, `reconcile::up` grows a test seam: extract the tick body so a test can
construct a `Reconciler`, call `reconcile().await` N times, and assert on
`snapshot()` — without real signals or a 500ms wall-clock wait. Options:
- expose `Reconciler` + `reconcile()` as `pub(crate)` and add
  `#[cfg(test)]`-gated constructors, **or**
- add a `pub async fn run_until<F: Fn(&ClusterState) -> bool>(…, steps, tick)`
  driver used by both `up` and tests.
Prefer the second — one driver, no divergence between tested and shipped loops
(Constitution III: the tested loop *is* the shipped loop).

Time control: `reconcile()` already reads `Instant::now()` internally. For
deterministic backoff/timeout tests, thread an injectable clock
(`trait Clock { fn now(&self) -> Instant }`, real impl in prod, a manual impl in
tests) **or** drive with short real durations via manifest-tunable
`restart_backoff_ms`/`ready_timeout` set to tens of ms. Start with the latter
(no production change); introduce the clock trait only if flakiness demands it.

Tests to write (one per FR-R4 bullet): readiness gating; crash→backoff→recovery
+ reset-after-stable; liveness restart; roll maxUnavailable=1; roll maxSurge=1
temp port; scale up; scale down highest-index-first; app-dep gating;
`[[check]]` fail→hold→recover; teardown reverse order; hot-reload +
half-written-file rejection; (orphan reclamation test lands with M2/FR-R1).

CI: add a `test` step that runs the suite (already present) — ensure fixtures
build on the runner.

---

## M1 — Security hardening (FR-S1..S6)

All child-launch changes are localized to `process.rs::spawn` via a `pre_exec`
closure (`std::os::unix::process::CommandExt::pre_exec`) plus env construction
in `reconcile::instance_env`.

- **FR-S2 env isolation.** In `spawn`, call `cmd.env_clear()` then set a
  constructed map. `instance_env` becomes the *whole* env: a small base
  (`PATH`, `HOME`, `LANG`/`LC_*` if present) + `[defaults].env` + app `env` +
  injected `PORT`/`ARTZAIN_APP`/`ARTZAIN_REPLICA`. New optional manifest keys:
  `[defaults].inherit_env = ["KEY", ...]` (allowlist of artzain's own env to
  forward; default empty). Base-key list is documented and overridable.
- **FR-S3 priv-drop.** New optional `App.user: Option<String>`,
  `App.group: Option<String>`. Resolve name→uid/gid at load (via
  `getpwnam`/`getgrnam` through libc, or parse `/etc/passwd` to avoid a dep —
  libc is already a dep, use it). In `pre_exec`: `setgid`, `setgroups([])`,
  `setuid` **in that order**; any failure returns an `io::Error` so the spawn
  fails closed (FR-S3). Guard behind `#[cfg(unix)]`; on non-unix a declared
  `user` is a load error (NFR5 honesty).
- **FR-S4 file perms.** Create `.artzain/` with mode `0700` and write
  `state.json`/`lock`/log files `0600` (set via `OpenOptions.mode()` on unix; a
  post-write `set_permissions` fallback elsewhere). Centralize the dir-create so
  `state.rs`, `lock.rs`, `process.rs` all go through one helper.
- **FR-S5 secrets.** Audit: `snapshot()` already excludes env (state.rs:40-51 —
  keep). Add a test asserting a secret env value never appears in `state.json`
  text nor in captured tracing output. No `tracing` call logs env today; add a
  test guard so it stays that way.
- **FR-S6 rlimits.** Optional `[[app]].limits = { open_files = N, memory_mb = N }`.
  Apply in `pre_exec` via `libc::setrlimit(RLIMIT_NOFILE / RLIMIT_AS)`. Absent =
  no change. Document that `memory_mb` maps to address space (RLIMIT_AS), a soft
  guard, and that systemd `MemoryMax=` (M3) is the stronger knob.

`pre_exec` safety note: the closure runs post-fork/pre-exec — only
async-signal-safe calls (the libc set*id/setrlimit calls qualify; no allocation,
no logging). Document this in `process.rs`.

---

## M2 — Reliability hardening (FR-R1, FR-R3)

- **FR-R1 orphan reclamation.** Extend `state.json` to record, per running
  instance, its **process-group id** (`pgid`, already on `process::Handle`) in
  addition to `pid`. Bump a `state_version` field for forward-safety. On `up`
  startup, *before* acquiring the fresh lock's reconcile:
  1. read any existing `state.json` + `lock`;
  2. if the owner pid is dead (reuse `lock::is_pid_alive`), treat every recorded
     instance as an orphan: `kill(-pgid, SIGTERM)`, wait a short grace, then
     `kill(-pgid, SIGKILL)`;
  3. delete the stale state/lock, then proceed to normal reconcile.
  This closes S4 (kill -9 of `up` leaves children holding ports). Signalling by
  recorded pgid — not pid — avoids hitting a reused pid (fail-closed, Const. VI:
  skip any pgid whose leader pid is alive but *not* one of ours if
  distinguishable; otherwise only reclaim when the owner is confirmed dead).
- **FR-R3 bounded logs.** In `process.rs::stream_logs`/`open_log_file`, enforce
  a per-file cap. Simplest: size-checked rotation — when the active log passes
  `log_max_bytes` (new `[defaults]`, default e.g. 10 MB), rename to `.1` and
  reopen (keep `log_keep` old files, default 3). Alternative (less code): a
  fixed-size ring by truncating on threshold; rotation is friendlier for
  `logs --follow`, prefer it. The size check piggybacks on the existing
  per-line write loop.
- **FR-R2** is already covered by the M0 fixture (`FIXTURE_LIVE_FAIL_AFTER_MS`);
  M2 just adds the explicit test if not written in M0.

---

## M3 — Server operations (FR-O1..O3)

- **FR-O1 systemd.** New subcommand `artzain systemd [--user NAME] [--manifest
  PATH]` prints a unit file to stdout (and `--install` writes it to
  `/etc/systemd/system/artzain@.service` when run with privilege). Unit encodes:
  `User=`/`Group=` (FR-S1), `ExecStart=artzain -f <manifest> up`,
  `Restart=on-failure`, `RestartSec`, `KillSignal=SIGTERM` +
  `TimeoutStopSec` ≥ artzain's `STOP_GRACE`, journald (default stdout), plus
  systemd-level hardening (`NoNewPrivileges=`, `ProtectSystem=strict`,
  `ProtectHome=`, `PrivateTmp=`, optional `MemoryMax=`). A template unit is also
  checked in at `packaging/artzain@.service`.
- **FR-O2 docs.** `docs/deployment.md`: service account creation, binary +
  manifest placement, `artzain systemd --install` + `enable`, a reverse-proxy
  upstream example for **Caddy, nginx, and Apache** fronting `port..port+N`
  replicas, the upgrade procedure (drop binary → edit manifest → artzain rolls),
  and a recovery runbook (stale lock, orphan cleanup, wedged app).
- **FR-O3 diagnosis.** `status --json` (serialize `ClusterState` — already
  `Serialize`) and a `--check`/exit-code mode: exit non-zero when any app has
  `ready < desired` (external health check). `logs --tail N` (default a sane N,
  e.g. 200) and `logs --follow` (tail + inotify-free poll of the rotated files).
  `logs` must never `read_to_string` an unbounded file (current main.rs:165
  does — replace with a bounded tail reader).

---

## M4 — CI, release integrity (NFR4, stretch FR)

- Integration tests already run via `cargo test` in CI — confirm the fixture bin
  is built there.
- Add `cargo audit` (or `cargo deny check advisories bans`) as a CI job. Cheap,
  catches vulnerable deps (partial supply-chain value without adopting the full
  tier).
- **Stretch (not required for v1.0):** optional `App.sha256` verified against the
  binary before first spawn; on mismatch, fail closed. Pairs with the checksum
  the release workflow already emits. Keep behind an opt-in field so default
  behavior is unchanged (Const. VI, NFR3).
- sakela deploy: verify the release `.sha256` when fetching the artzain binary
  (consumer-side change in the sakela repo, tracked there; note it here for
  traceability).

---

## Data model changes
- `state.json`: add `state_version: u32`, and `pgid: Option<i32>` per
  `InstanceStatus`. Reader tolerates missing fields (serde default) → NFR3.
- `Manifest`: new optional fields — `Defaults.inherit_env`,
  `Defaults.log_max_bytes`, `Defaults.log_keep`, `App.user`, `App.group`,
  `App.limits`, (stretch) `App.sha256`. All `#[serde(default)]`, `deny_unknown_fields`
  still holds (they're now known). Extend `validate()` for each.

## Test strategy summary
- Unit tests (existing, keep): pure logic — backoff, spec_hash, select_apps,
  interpolation, validation, status parsing.
- Integration tests (new, M0): real fixture process through the real loop driver.
- CI: fmt + clippy + unit + integration + cargo-audit.

## Risks / watch-items
- `pre_exec` misuse (non-async-signal-safe calls) → subtle deadlocks. Mitigate:
  keep the closure to libc set*id/setrlimit only; document; test priv-drop with
  a fixture that reports its uid.
- Orphan reclamation racing a live pid-reuse → could signal an innocent process.
  Mitigate: only reclaim when the recorded owner pid is confirmed dead, signal
  by recorded pgid, and treat any uncertainty as "don't kill" (Const. VI).
- Log rotation vs the two async log-writer tasks (stdout+stderr share a file):
  serialize the size-check/rotate through one writer or a shared guarded handle.
