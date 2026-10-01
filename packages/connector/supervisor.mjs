/** The node service's core, as a state machine (§4.2): what the supervisor does with its one child.
 *
 *  ```
 *  stopped ─start─▶ starting ─ready─▶ running ─(exit | 3 failed health probes, 30 s apart)─▶ backoff ─▶ starting
 *                      │ fail                                 the 5th start within one 10-min window ─▶ failed
 *  ```
 *
 *  - **Budget.** A window opens at a start when none is open; each start inside it counts. A launch that
 *    fails — before it was ready, or after — goes to `backoff` and is started again, unless it was the 5th
 *    start of a window still open: then `failed`, and nothing starts until a person asks. Only 5 continuous
 *    minutes in `running` close the window (attempts back to 0); an explicit start or restart closes it too.
 *    A window older than 10 minutes is not "this" window: the next start opens a new one. The window is
 *    handed to `persist` at every change and given back at construction (`restored`), so a supervisor that
 *    restarts within it does not get a fresh budget.
 *  - **No overlapping cores.** Before every launch the previous child is terminated and its exit awaited
 *    (`terminate`, which also removes the socket only after that exit); a hang and a launch that never got
 *    ready are terminated at once, so neither outlives the decision about it.
 *  - **Causes** come from `cause` (the core's report for this launch, else the spawn error or the exit), plus
 *    the two this machine finds itself: `hang` and `ready.timeout`.
 *
 *  Everything with a side effect or a clock is handed in, so a test drives it with a fake clock and fake
 *  children and sees every decision; `connector.mjs` hands in the real ones (`core.mjs`). */
import { randomUUID } from 'node:crypto';

export const BUDGET_STARTS = 5;
export const WINDOW_MS = 10 * 60_000;
export const HEALTHY_MS = 5 * 60_000;
export const PROBE_MS = 30_000;
export const PROBE_FAILURES = 3;
/** Waits before each next start in a window, by how many starts it has seen. */
export const BACKOFF_MS = [2000, 4000, 8000, 16_000];

export const systemClock = { now: () => Date.now(), setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: timer => clearTimeout(timer) };

const iso = ms => (ms === null || ms === undefined ? null : new Date(ms).toISOString());

export class Supervisor {
  /** `launch(launchId)` → a handle `{pid, exit: Promise, done}`; `awaitReady(handle, launchId)` → `{ready}` or
   *  `{failure}`; `probe(core)` → health body or null; `terminate(handle)` → when it has exited;
   *  `cause({launchId, exit, key})` → a failure; `adopt()` → `{handle, core}` of a live core, or null;
   *  `persist(snapshot)`; `restored` → `{window_started, attempts}` from the last snapshot. */
  constructor({ clock = systemClock, launch, awaitReady, probe, terminate, cause, adopt = async () => null,
    persist = () => {}, restored = null, log = () => {}, newLaunchId = randomUUID, service = 'none', timing = {} }) {
    Object.assign(this, { clock, launchCore: launch, awaitReady, probe, terminate, cause, adopt, persist, log, newLaunchId, service });
    // The design's numbers; a test of real processes shortens them (`connector.mjs` reads SIDEVOICE_*_MS).
    this.timing = { budget: BUDGET_STARTS, window: WINDOW_MS, healthy: HEALTHY_MS, probe: PROBE_MS, probes: PROBE_FAILURES, backoff: BACKOFF_MS, ...timing };
    this.state = 'stopped';
    this.since = clock.now();
    this.attempts = 0;
    this.windowStarted = null;
    const window = restored?.window_started ? Date.parse(restored.window_started) : null;
    if (window !== null && Number.isFinite(window) && clock.now() - window < this.timing.window) {
      this.windowStarted = window;
      this.attempts = Number(restored.attempts) || 0;
      this.failure = restored.failure ?? null;
    }
    this.nextRetryAt = null;
    this.core = null;
    this.calls = 0;
    this.failure ??= null;
    this.handle = null;
    this.launchId = null;
    this.generation = 0;   // every decision in flight belongs to one; a later one makes it moot
    this.timers = {};
    this.waiters = new Set();
  }

  status() {
    return { ok: true, state: this.state, since: iso(this.since), attempts: this.attempts,
      window_started: iso(this.windowStarted), next_retry_at: iso(this.nextRetryAt),
      core: this.core ? { pid: this.core.pid ?? null, version: this.core.version ?? null, api: this.core.api ?? null, launch_id: this.core.launch_id ?? null } : null,
      calls: this.calls, failure: this.failure, service: this.service };
  }

  set(state, changes = {}) {
    Object.assign(this, changes);
    if (state !== this.state) { this.state = state; this.since = this.clock.now(); }
    try { this.persist(this.status()); } catch {}
    for (const waiter of [...this.waiters]) waiter();
  }

  clear(...names) {
    for (const name of names.length ? names : Object.keys(this.timers)) { if (this.timers[name]) this.clock.clearTimeout(this.timers[name]); delete this.timers[name]; }
  }

  /** The supervisor's own first start: a live core of the right launch is adopted (no start counted), else
   *  one is started. */
  async boot() {
    const generation = ++this.generation;
    const found = await this.adopt().catch(() => null);
    if (generation !== this.generation) return;
    if (found) {
      this.handle = found.handle; this.launchId = found.core.launch_id;
      this.log(`adopted the running core (pid ${found.core.pid}, launch ${found.core.launch_id})`);
      return this.running(found.core, generation);
    }
    return this.launch(generation);
  }

  /** Start, unless already on its way. An explicit start (a person's) closes the window: they asked again. */
  start({ explicit = false } = {}) {
    if (!explicit && ['starting', 'running', 'backoff'].includes(this.state)) return;
    if (explicit && ['starting', 'running'].includes(this.state)) return;
    if (explicit) this.closeWindow();
    this.clear();
    return this.launch(++this.generation);
  }

  /** A person's restart: the window closes and the core starts again, whatever state it was in. */
  restart() {
    this.closeWindow();
    this.clear();
    return this.launch(++this.generation);
  }

  /** Stop the core and stay stopped. */
  async stop() {
    const generation = ++this.generation;
    this.clear();
    const handle = this.handle; this.handle = null;
    if (handle) await this.terminate(handle);
    if (generation === this.generation) this.set('stopped', { core: null, calls: 0, nextRetryAt: null });
  }

  /** What `node.ensure` is: started if it was stopped, then the first settled state — running, failed, or
   *  backoff (a start that failed and will be tried again) — within `timeout`. */
  async ensure(timeout = 90_000) {
    if (this.state === 'stopped') this.start();
    const settled = () => ['running', 'failed'].includes(this.state) || (this.state === 'backoff' && this.failure);
    if (settled()) return this.status();
    await new Promise(resolve => {
      const done = () => { if (settled()) { this.waiters.delete(done); this.clock.clearTimeout(timer); resolve(); } };
      const timer = this.clock.setTimeout(() => { this.waiters.delete(done); resolve(); }, timeout);
      this.waiters.add(done);
    });
    return this.status();
  }

  closeWindow() { this.windowStarted = null; this.attempts = 0; }

  async launch(generation) {
    const now = this.clock.now();
    if (this.windowStarted === null || now - this.windowStarted >= this.timing.window) { this.windowStarted = now; this.attempts = 0; }
    // A window whose budget is spent — restored from before this supervisor started — starts nothing.
    if (this.attempts >= this.timing.budget) return this.set('failed', { failure: this.failure, core: null, calls: 0, nextRetryAt: null });
    this.attempts++;
    this.set('starting', { nextRetryAt: null, core: null, calls: 0 });
    // No overlapping cores: whatever ran before is gone, and seen gone, before another starts.
    const previous = this.handle; this.handle = null;
    if (previous) await this.terminate(previous);
    if (generation !== this.generation) return;
    const launchId = this.launchId = this.newLaunchId();
    let handle;
    try { handle = await this.launchCore(launchId); }
    catch (error) { return this.failed(this.cause({ launchId, exit: { error: { code: error.code, message: error.message, path: error.path } } }), generation); }
    if (generation !== this.generation) { await this.terminate(handle); return; }
    this.handle = handle;
    this.log(`started the core (pid ${handle.pid ?? '?'}, launch ${launchId}, start ${this.attempts} in this window)`);
    const outcome = await this.awaitReady(handle, launchId);
    if (generation !== this.generation) return;
    if (outcome.ready) return this.running(outcome.ready, generation);
    // Never ready in time: it is not left running beside the next one.
    if (outcome.failure.key === 'ready.timeout' && this.handle === handle) { this.handle = null; await this.terminate(handle); }
    return this.failed(outcome.failure, generation);
  }

  running(core, generation) {
    this.set('running', { core, calls: core.calls ?? 0, failure: null, nextRetryAt: null });
    const handle = this.handle;
    handle?.exit?.then(exit => {
      if (generation !== this.generation || this.handle !== handle) return;
      this.handle = null;
      this.clear('probe', 'healthy');
      this.log(`the core (pid ${handle.pid}) went away: ${JSON.stringify(exit)}`);
      this.failed(this.cause({ launchId: this.launchId, exit }), generation);
    });
    // Five continuous minutes running: the budget is whole again.
    this.timers.healthy = this.clock.setTimeout(() => {
      if (generation !== this.generation || this.state !== 'running') return;
      this.closeWindow(); this.set('running');
    }, this.timing.healthy);
    let misses = 0;
    const probe = () => {
      this.timers.probe = this.clock.setTimeout(async () => {
        if (generation !== this.generation || this.state !== 'running') return;
        const health = await this.probe(this.core).catch(() => null);
        if (generation !== this.generation || this.state !== 'running') return;
        if (health) { misses = 0; if (typeof health.calls === 'number' && health.calls !== this.calls) this.set('running', { calls: health.calls }); return probe(); }
        if (++misses < this.timing.probes) return probe();
        // Alive and not answering: a hang. It is ended before anything else is decided.
        this.log(`the core did not answer ${this.timing.probes} health probes ${this.timing.probe / 1000} s apart: terminating it`);
        this.clear('healthy');
        const wedged = this.handle; this.handle = null;
        if (wedged) await this.terminate(wedged);
        if (generation !== this.generation) return;
        this.failed(this.cause({ launchId: this.launchId, key: 'hang' }), generation);
      }, this.timing.probe);
    };
    probe();
  }

  failed(failure, generation) {
    if (generation !== this.generation) return;
    const now = this.clock.now();
    failure = { ...failure, attempts: this.attempts };
    this.log(`the core failed: ${failure.key}${failure.detail ? ' (' + failure.detail + ')' : ''}, start ${this.attempts} of ${this.timing.budget} in this window`);
    if (this.attempts >= this.timing.budget && this.windowStarted !== null && now - this.windowStarted < this.timing.window) {
      return this.set('failed', { failure, core: null, calls: 0, nextRetryAt: null });
    }
    const backoff = this.timing.backoff;
    const delay = backoff[Math.min(this.attempts, backoff.length) - 1] ?? backoff[0];
    this.set('backoff', { failure, core: null, calls: 0, nextRetryAt: now + delay });
    this.timers.retry = this.clock.setTimeout(() => { if (generation === this.generation) this.launch(generation); }, delay);
  }
}
