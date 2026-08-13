# artzain — roadmap

*A tiny, k8s-shaped orchestrator in Rust for prebuilt binaries: a declarative manifest and a
reconcile loop that keeps N replicas of each app alive. `artzain` = Basque for shepherd.*

**Updated** 2026-08-13 · v0.3.0 published · ~5k LOC, 61 tests · **running in production**

---

## Where this actually is

- **It works, it is published, and it is genuinely load-bearing.** v0.3.0 is a GitHub release,
  and it is what keeps bildu's API alive on the shared droplet. Of everything in this
  ecosystem, artzain is the project with the least gap between what the README claims and
  what is true.
- **The k8s-shaped behaviours are verified live**, not just tested: N replicas on distinct
  ports, crash recovery with exponential backoff, non-disruptive scale-up, rolling updates
  with `maxUnavailable=1` and optional `maxSurge=1` on a temporary port, graceful teardown,
  stale non-Ready instances rolled immediately, and scale-down retiring higher replica indices
  first.
- **The design constraints held.** No git, no Docker, no daemon. A single sequential reconcile
  loop on a 500 ms tick with no shared state and no locks — which is why it is 5,000 lines and
  comprehensible. Probes are hand-rolled TCP and HTTP/1.1 rather than pulling in reqwest,
  matching sutegi's ethos.
- **The ops hardening is real**: `env_clear()` with an explicit inherit allowlist, per-app
  user/group fail-closed privilege drop, 0700/0600 permissions, a secrets-not-leaked test,
  orphan reclamation via recorded pgid when the owner pid is dead, bounded log rotation, a
  systemd unit generator with `NoNewPrivileges`/`ProtectSystem`/`ProtectHome`/`PrivateTmp`,
  `status --json`, and a deployment guide with a recovery runbook.
- **M0 through M3 of the production-hardening SDD are done. M4 — CI and release — is the
  remaining milestone**, and CI plus release workflows already exist, so the gap is narrower
  than the plan implies.
- **28 days since the last commit**, which for a stable tool in production is a healthy sign
  rather than a worrying one.
- **Single-tenant, single-host, by design.** Scope was locked deliberately: systemd-wrapped
  foreground rather than a native daemon, single-tenant hardening, ingress out of scope.

## The one thing that decides this project

**Nothing — and that is the point.**

artzain is the healthiest project here. It has a real user (bildu in production), a locked
scope, a published release, verified behaviour, and a finite remaining plan. It does not need
a strategic decision; it needs M4 finished and then to be left alone.

The only genuine risk is **scope creep by good idea**. Every operational annoyance on the
droplet will look like an artzain feature. Most of them are not. The value of this tool is
that it is 5,000 lines you can read in an afternoon, and every feature spends that.

**Finish v1.0. Then stop.**

---

## M1 — M4: close out the hardening SDD ← next

The last milestone of `specs/001-production-hardening`. Mostly consolidation.

- [ ] Confirm CI and release workflows gate everything the SDD requires — test, clippy
      `-D warnings`, fmt, plus the release binary build.
- [ ] Cut **v1.0.0**. The scope was locked, the milestones are done, and a tool running
      someone's production API should not be on 0.x.
- [ ] Version the manifest format explicitly. `state_version` exists for state; `artzain.toml`
      has no compatibility marker, and the first breaking manifest change without one is an
      outage on a box with no cargo.

**Done when:** v1.0.0 is released and the SDD is closed.

## M2 — Earn the production claim it already has

artzain keeps bildu alive on a 2 GB shared box with six other sites. Two things that a
production supervisor should be able to answer and cannot yet.

- [ ] **Prove the recovery runbook.** The deployment guide documents stale lock, wedged app,
      disk-full, and multiple-fleet recovery. Rehearse each one against the real droplet and
      record what actually happened. A runbook that has only been written is a hypothesis.
- [ ] **Behaviour when the box is out of memory.** 2 GB, six sites, and a reconcile loop that
      spawns replicas. What happens when a spawn fails on OOM rather than on crash? The
      backoff path is tested for crashes; resource exhaustion is a different failure.
- [ ] A `--dry-run` diff on `up` so a manifest change on a production host can be reviewed
      before it reconciles. `plan` exists; make sure it covers the rolling-update path.

**Done when:** every recovery scenario in `docs/deployment.md` has been executed once for
real.

## M3 — Only what a second fleet demands

artzain runs bildu. mintzo and sakela both target it via `deploy.yml`. That is the natural
next pressure and it should drive the backlog, not speculation.

- [ ] Whatever mintzo's deploy needs, given its extra runtime gate (Postgres with pgvector on
      the host, plus a migrated database). The `[[check]]` mechanism already models this; find
      out whether it models it *well* when the dependency is slow to come up rather than
      absent.
- [ ] Nothing else without a second real fleet asking for it.

---

## Not doing

- **Multi-host scheduling.** That is k8s. artzain is k8s-*shaped*, on one box, on purpose.
- **A daemon.** Locked out of scope. systemd-wrapped foreground is the model.
- **Containers or images.** Prebuilt binaries is the entire premise; it is why there is no
  Docker and why cross-compiled musl works on a box with no toolchain.
- **Ingress, TLS, or reverse proxying.** Out of scope. Apache and Caddy do this, and
  `docs/deployment.md` already shows how to point them at artzain.
- **Multi-tenancy.** Single-tenant hardening was the locked decision; per-app user/group
  privilege drop is as far as it goes.

## Risks worth naming

- **Its success is invisible until it fails.** artzain gets no attention precisely because it
  works, and the day it does not, it will be holding a production API on a box with no build
  toolchain. M2 exists for that day.
- **Scope creep by good idea** is the main threat to the property that makes it valuable.
- **Bus factor and dependency direction.** bildu depends on artzain, artzain depends on
  nothing. That is the right direction, and it means an artzain bug is an outage rather than
  an inconvenience.
