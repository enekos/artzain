# Spec 001 — Production hardening (artzain v1.0)

**Status:** proposed · **Created:** 2026-07-15 · **Owner:** Eneko
**Governs:** everything under `specs/001-production-hardening/`
**Constitution:** all requirements below are subordinate to `../constitution.md`

> This is the **what and why**, not the how. Implementation choices live in
> `plan.md`; the ordered work lives in `tasks.md`.

---

## 1. Problem

artzain v0.2 is built, works, and already runs [[sakela]] on
`root@es.gizapedia.org`. But its k8s-shaped behavior has been proven mostly by
hand ("verified live once"), it makes no security assumptions about the host,
and it has no story for surviving reboots, its own crash, or unbounded disk
growth. Before Eneko hosts *more* Rust projects on this server and relies on it
unattended, artzain must become **battle-tested** (behavior locked by tests),
**secure** (least-authority on a single-tenant host), and **operable** (a real
install/lifecycle story).

This spec defines "v1.0: something I can rely on to host my Rust projects."

## 2. Goals

- G1. Every existing k8s-shaped behavior is locked by an integration test that
  drives a real child process (Constitution VII).
- G2. artzain and the apps it runs hold the least authority that works on a
  single-tenant server: unprivileged, minimal env, tight file perms, no secret
  leakage (Constitution V).
- G3. artzain survives SSH disconnect, host reboot, and its own SIGKILL without
  leaving orphaned processes holding ports or an inconsistent fleet.
- G4. A running artzain cannot exhaust the host (bounded logs, fds, optional
  memory caps).
- G5. Installing and operating artzain on a Linux server is a documented,
  repeatable, few-command procedure.

## 3. Non-goals (this spec)

- Native detached daemon / `--detach`. **systemd wraps the foreground `up`**
  (Constitution IV). Decided 2026-07-15.
- Built-in ingress / load balancer / TLS. A reverse proxy (Caddy / nginx /
  Apache) fronts the `port+i` replicas and is configured separately. Decided
  2026-07-15.
- Multi-tenant isolation (per-app namespaces, seccomp/landlock). Out of threat
  model — single-tenant host Eneko controls. Decided 2026-07-15.
- Adopting *already-running* healthy children across an artzain restart. artzain
  reclaims (kills) orphans from a dead generation and starts fresh; it does not
  re-parent live processes. (Documented limitation; revisit if fleet-wide
  restart-on-upgrade becomes painful.)
- Distributed / multi-host clustering. artzain is one host, one manifest.

## 4. Users & scenarios

**Primary user:** Eneko, operating his own Linux server, single-tenant.

- **S1 — First install.** Fresh server. Eneko installs the artzain binary,
  creates a service account, drops an `artzain.toml`, and enables a systemd
  unit. After reboot the fleet comes back up on its own.
- **S2 — Deploy a new version.** A new binary lands (via the release pipeline).
  Eneko updates the binary + edits the manifest; artzain rolls the app with no
  (or bounded) downtime, no manual restart.
- **S3 — An app misbehaves.** A managed app crashes / hangs / leaks memory /
  fails its health check. artzain restarts it with backoff, or holds it, and
  the rest of the fleet is unaffected. The event is visible in `status`/logs.
- **S4 — artzain itself dies.** OOM killer or a bug SIGKILLs the `up` process.
  systemd restarts it; on restart artzain finds and cleans up orphaned children
  from the previous generation, frees their ports, and re-reconciles — no manual
  `kill` needed.
- **S5 — Long uptime.** artzain runs for weeks. Logs do not fill the disk; fds
  do not leak; memory is flat.
- **S6 — Diagnose remotely.** Over SSH, Eneko can see fleet health, per-instance
  phase/restarts, and tail an app's logs, fast, without a half-gigabyte dump.

## 5. Functional requirements

### Security (single-tenant hardening)
- **FR-S1 — Unprivileged service account.** artzain ships an install path that
  runs it as a dedicated non-root user with a locked-down home. Documented; the
  systemd unit enforces it.
- **FR-S2 — Environment isolation.** A child's environment is *constructed*, not
  inherited wholesale. Default: pass only an explicit base (PATH, HOME, LANG,
  the injected `PORT`/`ARTZAIN_*`) plus manifest-declared env. A manifest opt-in
  controls whether any of artzain's own environment is forwarded, and which keys.
- **FR-S3 — Per-app privilege drop (optional).** An `[[app]]` may declare
  `user`/`group`; artzain drops to that uid/gid for that child before exec. If
  declared and the drop cannot be performed, the spawn **fails closed** (the app
  does not run as the wrong user).
- **FR-S4 — Tight file permissions.** `.artzain/` and everything artzain writes
  in it (`state.json`, `lock`, `logs/`) are created `0700`/`0600` — not
  world-readable. Logs may contain app secrets; they must not leak to other
  users.
- **FR-S5 — No secrets in artzain's own outputs.** `state.json` never contains
  env values; artzain's tracing never logs env values or manifest secrets. (Hold
  the current behavior under test so it can't regress.)
- **FR-S6 — Optional per-app resource limits.** An `[[app]]` may declare limits
  (at minimum: max open files; ideally: address-space/memory) that artzain
  applies to the child before exec. Absent = inherit artzain's own limits.

### Reliability / battle-tested
- **FR-R1 — Orphan reclamation on startup.** On `up`, if a stale
  `state.json`/`lock` from a dead owner is found, artzain identifies the
  previous generation's child process groups, terminates them (SIGTERM→SIGKILL),
  frees their ports, and only then reconciles. Requires recording enough
  identity (process-group id, not just pid) to reclaim safely.
- **FR-R2 — Liveness restart, proven live.** A child whose liveness endpoint
  fails `live_failures` times in a row is restarted — exercised end-to-end by a
  fault-injection app, not only by unit logic.
- **FR-R3 — Bounded logs.** Per-instance log files are size-capped with rotation
  (or ring-buffer truncation). A crash-looping app cannot fill the disk.
- **FR-R4 — Behavior lock.** Integration tests cover, each driving a real
  process: readiness gating, crash→backoff→recovery + counter reset after
  `stable_after_secs`, liveness restart, rolling update (maxUnavailable=1),
  rolling update (maxSurge=1 on temp port), scale up, scale down (highest
  replica index retires first), app-dependency gating, `[[check]]` failure
  gating + recovery, graceful teardown in reverse dependency order, manifest
  hot-reload (incl. half-written-file rejection), orphan reclamation.

### Operations
- **FR-O1 — systemd integration.** artzain provides a systemd unit (template +
  a generator command, e.g. `artzain systemd`) that runs `up` as the service
  account with the hardening from FR-S1/S4/S6, restart-on-failure, and journald
  capture. `enable` → survives reboot (S1); `restart` → clean reclaim (S4).
- **FR-O2 — Deployment guide.** A `docs/deployment.md` covering: create the
  service account, place binary + manifest, install the unit, put a reverse
  proxy in front of the replicas (Caddy/nginx/Apache upstream example), upgrade
  procedure, and recovery runbook.
- **FR-O3 — Fast diagnosis.** `status` supports machine-readable output
  (`--json`) and a non-zero exit when the fleet is not fully ready (usable as an
  external health check). `logs` supports `--tail N` and `--follow` and never
  dumps an unbounded file (S6).

## 6. Non-functional requirements
- **NFR1 — Constitution compliance.** No change may violate `../constitution.md`;
  where one bumps against a principle, the spec/plan says so explicitly.
- **NFR2 — Zero new *runtime* heavy deps.** Security/ops features use libc +
  std where reasonable (Constitution II). Test-only deps are unconstrained.
- **NFR3 — Backward-compatible manifests.** Every existing `artzain.toml`
  (including sakela's) keeps working unchanged; all new fields are optional with
  safe defaults.
- **NFR4 — CI enforces the safety net.** Integration tests + clippy `-D warnings`
  + fmt run on every PR; a dependency vulnerability check runs in CI.
- **NFR5 — Portability honesty.** Unix is the supported target. Non-Unix keeps
  compiling but hardening that requires libc (priv-drop, rlimits, pgid reclaim)
  is documented as Unix-only, not silently no-op'd in a way that looks secure.

## 7. Acceptance criteria (v1.0 "done")
1. `cargo test` runs the full integration suite (FR-R4) and it is green in CI.
2. A documented `artzain systemd` + `enable` sequence brings the fleet up, and
   it returns after a reboot (S1) and after `kill -9` of the `up` process with
   no orphaned children left holding ports (S4).
3. Children run under a constructed env (FR-S2) and, when declared, a dropped
   uid/gid (FR-S3); `.artzain/` is `0700`, its files `0600` (FR-S4); a test
   asserts no env value appears in `state.json` or artzain's logs (FR-S5).
4. A crash-looping app under artzain does not grow disk usage without bound
   (FR-R3), verified by a test.
5. `docs/deployment.md` exists and a proxy config example fronts the replicas
   (FR-O2). `status --json` and `logs --tail/--follow` exist (FR-O3).
6. `cargo audit` (or `cargo deny`) passes in CI (NFR4).
7. sakela's existing manifest runs unchanged on the new version (NFR3).

## 8. Out of scope / explicitly deferred
- Migrating surge ports back to `port+i` after a roll (accept the documented
  temp-port trade-off for now).
- `status`/metrics HTTP endpoint (proxy + `status --json` + external check
  cover S6).
- Binary signature/SBOM/provenance (that is the supply-chain tier; a *checksum*
  verify hook is a cheap stretch in `plan.md`, not required for v1.0).
- Windows-native hardening.
