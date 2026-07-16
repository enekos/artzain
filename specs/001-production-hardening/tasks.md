# Tasks 001 — Production hardening

Ordered, checkable work for `spec.md` / `plan.md`. `[ ]` todo, `[~]` in
progress, `[x]` done. Each milestone ends at a **gate**: full test suite green +
`cargo clippy -- -D warnings` + `cargo fmt --check`. Do not start the next
milestone until the current gate passes. IDs are stable references for commits.

---

## M0 — Test harness & behavior lock  *(do first — Const. VII)*

- [x] **T0.1** Add `crates/artzain-fixture` bin: HTTP server serving
  `/__ready` `/__health` `/__metrics`, fault-controllable via env
  (`FIXTURE_READY_AFTER_MS`, `FIXTURE_LIVE_FAIL_AFTER_MS`, `FIXTURE_CRASH_AFTER_MS`,
  `FIXTURE_EXIT_CODE`, `FIXTURE_IGNORE_SIGTERM`). Zero-dep HTTP like `probe.rs`.
- [x] **T0.2** Add loop test seam: extract a `run_until(reconciler, predicate,
  max_steps, tick)` driver used by both `up` and tests; make `Reconciler` +
  `reconcile()` reachable from `tests/`. No behavior change to shipped loop.
- [x] **T0.3** Integration test: readiness gating (Starting→Ready only after
  `/__ready` passes; `ready` count reflects it).
- [x] **T0.4** Integration test: crash → exponential backoff → recovery, and
  restart counter resets after `stable_after_secs` uptime.
- [x] **T0.5** Integration test: liveness restart (`FIXTURE_LIVE_FAIL_AFTER_MS`
  → `live_failures` consecutive fails → restart). *(satisfies FR-R2)*
- [x] **T0.6** Integration test: SIGTERM-ignored child → SIGKILL escalation
  after `STOP_GRACE` (`FIXTURE_IGNORE_SIGTERM`).
- [x] **T0.7** Integration test: rolling update maxUnavailable=1 (edit spec →
  never more than 1 replica unavailable vs desired).
- [x] **T0.8** Integration test: rolling update maxSurge=1 (new instance on temp
  port becomes Ready before old one stops).
- [x] **T0.9** Integration test: scale up, and scale down retiring highest
  replica index first.
- [x] **T0.10** Integration test: app-dependency gating (dependent stays Pending
  until dependency Ready) + `[[check]]` fail → dependents held → recover.
- [x] **T0.11** Integration test: graceful teardown in reverse dependency order.
- [x] **T0.12** Integration test: manifest hot-reload picks up changes AND a
  half-written / zero-app file is rejected (fleet stays up).
- [x] **GATE M0** — suite green in CI; fixtures build on the runner.

## M1 — Security hardening

- [x] **T1.1** `instance_env` builds the *whole* env; `spawn` calls
  `env_clear()`. Add `[defaults].inherit_env` allowlist (default empty) + a
  documented base-key set. *(FR-S2)*
- [x] **T1.2** Add `App.user`/`App.group`; resolve name→uid/gid at load;
  `pre_exec` does setgid→setgroups([])→setuid, fail-closed on error; `#[cfg(unix)]`,
  non-unix declared-user = load error. *(FR-S3, NFR5)*
- [x] **T1.3** Test priv-drop with a fixture that reports its runtime uid/gid
  (skip/ignore unless test runner can drop, e.g. gated on running as root).
- [x] **T1.4** Centralized `.artzain/` creation at `0700`; `state.json`, `lock`,
  logs written `0600`. *(FR-S4)*
- [x] **T1.5** Test: no manifest env value appears in `state.json` or captured
  tracing output. *(FR-S5)*
- [x] **T1.6** Add `App.limits = { open_files, memory_mb }`; apply via
  `setrlimit` in `pre_exec`. Document async-signal-safety of the closure.
  *(FR-S6)*
- [x] **T1.7** Update `manifest.rs::validate` + README manifest reference +
  example for all new fields.
- [x] **GATE M1**.

## M2 — Reliability hardening

- [x] **T2.1** `state.json`: add `state_version` + per-instance `pgid`; reader
  tolerates missing (serde default). *(NFR3)*
- [x] **T2.2** Startup orphan reclamation: on `up`, if prior owner pid is dead,
  SIGTERM→grace→SIGKILL each recorded pgid, clear stale state/lock, then
  reconcile. Only when owner confirmed dead; signal by pgid. *(FR-R1, S4)*
- [x] **T2.3** Integration test: simulate a dead-owner state file with a live
  child pgid → `up` reclaims it and frees the port.
- [x] **T2.4** Bounded logs: `[defaults].log_max_bytes` (default 10MB) +
  `log_keep` (default 3); size-checked rotation in the log writer, serialized
  across stdout/stderr writers. *(FR-R3)*
- [x] **T2.5** Integration test: a crash-looping fixture does not grow log dir
  beyond the cap.
- [x] **GATE M2**.

## M3 — Server operations

- [x] **T3.1** `artzain systemd [--user] [--manifest] [--install]` generates the
  unit (User/Group, Restart=on-failure, KillSignal=SIGTERM,
  TimeoutStopSec≥STOP_GRACE, journald, NoNewPrivileges/ProtectSystem/ProtectHome/
  PrivateTmp, optional MemoryMax). *(FR-O1)*
- [x] **T3.2** Check in `packaging/artzain@.service` template.
- [x] **T3.3** `status --json` and `status --check` (non-zero exit when any app
  `ready < desired`). *(FR-O3)*
- [x] **T3.4** `logs --tail N` (default 200) + `logs --follow`; replace the
  unbounded `read_to_string` (main.rs:165) with a bounded tail reader. *(FR-O3)*
- [x] **T3.5** `docs/deployment.md`: service account, install, Caddy + nginx +
  Apache upstream examples for the replica port range, upgrade procedure,
  recovery runbook. *(FR-O2)*
- [x] **GATE M3**.

## M4 — CI & release integrity

- [ ] **T4.1** Confirm integration suite (incl. fixture bin) runs in CI.
- [ ] **T4.2** Add `cargo audit` (or `cargo deny`) CI job. *(NFR4)*
- [ ] **T4.3** *(stretch)* Optional `App.sha256`, verified before first spawn,
  fail-closed on mismatch.
- [ ] **T4.4** *(cross-repo, tracked in sakela)* Verify the release `.sha256`
  when fetching the artzain binary in the sakela deploy.
- [ ] **GATE M4** → tag **v1.0.0**; verify acceptance criteria 1–7 in `spec.md`.

---

## Definition of done (v1.0)
All GATE items pass and every acceptance criterion in `spec.md §7` is checked
off, on a fresh server following `docs/deployment.md`, with sakela's existing
manifest running unchanged.
