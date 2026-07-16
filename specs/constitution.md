# artzain constitution

The non-negotiable principles every spec, plan, and change in this repo must
respect. When a proposed change conflicts with a principle, the change loses —
or the principle is amended here first, in its own commit, with a rationale.

These exist because artzain's whole value is being *small enough to trust*. A
1000-line orchestrator you have read end-to-end is safer to run your server on
than a 100k-line one you haven't. Every principle below defends that property.

---

## I. Small and readable over featureful

The core stays comprehensible in one sitting (target: `artzain-core` under
~3.5k LOC). A feature that cannot be added without materially growing cognitive
load must be justified against this principle explicitly, or pushed to a
consumer (a reverse proxy, systemd, a shell script).

## II. Minimal dependency surface

Runtime deps stay close to today's set (tokio, clap, serde, toml, libc). No
`reqwest`/`hyper` — probes are hand-rolled HTTP/1.1 over `TcpStream`. A new
runtime dependency requires a line in the relevant plan justifying why it can't
be hand-rolled in <100 lines. Dev/test deps are freer.

## III. One sequential reconcile loop, no shared state

All cluster state transitions happen in one place, one 500ms tick at a time
(`reconcile.rs`). No background mutation of instance state, no locks over
shared cluster data, no actor soup. This is what makes behavior reproducible
and testable. New behavior is a step in the tick, not a new thread.

## IV. The file is the control plane

Desired state is `artzain.toml`; observable state is `.artzain/state.json`.
There is **no daemon, no socket, no API to secure**. Editing the manifest is
the apply. Server lifecycle (detach, reboot-survival, restart-on-crash) is
delegated to systemd wrapping the foreground `up`, not built in.

## V. Unprivileged by default, least authority always

artzain runs as a dedicated non-root service account. It never requires root to
function (low ports are the reverse proxy's job). Children get the *narrowest*
environment, file permissions, and privileges that let them run — never more
than artzain itself holds. Secrets never land in `state.json` or in artzain's
own logs.

## VI. Fail closed, never fail destructive

Ambiguous or partial input is rejected, not guessed. A half-written manifest,
a zero-app reload, a mismatched lock file — each keeps the last-good state or
refuses to act, and says why. artzain never tears down a running fleet because
of its own uncertainty. (See the two v0.1 bugs: both were fail-destructive
defaults, both are now fail-closed.)

## VII. Every behavior has an executable test

k8s-shaped behavior — crash backoff, readiness gating, liveness restart,
rolling maxUnavailable/maxSurge, scale up/down, dependency gating, graceful
teardown, orphan reclamation — is proven by an integration test driving a real
child process, not only by unit tests of pure logic and not only by hand. "It
was verified live once" is not coverage. A behavior without a test is a
regression waiting to happen.

## VIII. Predictable operations

Same inputs → same outcome. No hidden state outside `artzain.toml` + `.artzain/`.
Every state-changing action is observable (`status`, `logs`, structured tracing)
and every failure mode has a documented recovery. Disk, fd, and memory use are
bounded — a long-running artzain must not exhaust the host that hosts it.

---

*Amendments: append a dated note below with the principle changed and why.*
