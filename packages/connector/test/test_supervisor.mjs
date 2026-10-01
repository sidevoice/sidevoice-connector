/** The supervisor's state machine on a fake clock, with fake children: every decision it makes about
 *  starting, waiting, giving up and terminating, without a process or a second of real time. And the cause
 *  keys, read from what a launch leaves behind. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { BACKOFF_MS, BUDGET_STARTS, HEALTHY_MS, PROBE_MS, Supervisor, WINDOW_MS } from '../supervisor.mjs';
import { failureCause } from '../core.mjs';

const settle = async () => { for (let i = 0; i < 20; i++) await new Promise(resolve => setImmediate(resolve)); };

function fakeClock(start = Date.parse('2026-10-01T10:00:00Z')) {
  let now = start, serial = 0;
  const timers = new Map();
  const clock = {
    now: () => now,
    setTimeout: (fn, ms) => { const id = ++serial; timers.set(id, { at: now + ms, fn }); return id; },
    clearTimeout: id => timers.delete(id),
    /** Move time on, running every timer that falls due, in order, and what each one sets going. */
    async advance(ms) {
      const until = now + ms;
      for (;;) {
        await settle();
        const due = [...timers.entries()].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
        if (!due) break;
        timers.delete(due[0]); now = due[1].at; due[1].fn();
      }
      now = until; await settle();
    },
  };
  return clock;
}

/** Children that do what each launch's plan says: `ready`, `fail:<key>` (dies before ready, having
 *  reported), `timeout` (never ready), and that are terminated only when the supervisor says so. */
function harness({ plans = [], restored = null, clock = fakeClock(), adopt = null } = {}) {
  const events = [], alive = new Set(), handles = [];
  let pid = 100, launches = 0, healthy = true, persisted = null;
  const deps = {
    clock, restored,
    newLaunchId: () => `launch-${launches + 1}`,
    async launch(launchId) {
      assert.equal(alive.size, 0, 'no core starts while another is alive');
      const plan = plans[Math.min(launches, plans.length - 1)] ?? 'ready';
      launches++;
      let resolve; const exit = new Promise(r => { resolve = r; });
      const handle = { pid: ++pid, launchId, plan, done: null, exit };
      handle.die = outcome => { if (handle.done) return; handle.done = outcome; alive.delete(handle.pid); events.push(['exit', handle.pid]); resolve(outcome); };
      alive.add(handle.pid); handles.push(handle); events.push(['launch', handle.pid, launchId]);
      return handle;
    },
    async awaitReady(handle, launchId) {
      if (handle.plan === 'ready') return { ready: { pid: handle.pid, launch_id: launchId, version: '0.1.0', api: 1, calls: 0 } };
      if (handle.plan === 'timeout') return { failure: { key: 'ready.timeout' } };
      const key = handle.plan.split(':')[1];
      handle.die({ code: 1 });
      return { failure: { key, launch_id: launchId } };
    },
    probe: async () => (healthy ? { calls: 0 } : null),
    async terminate(handle) { events.push(['terminate', handle.pid]); handle.die?.({ signal: 'SIGTERM' }); },
    cause: ({ launchId, exit, key }) => ({ key: key || (exit?.signal ? 'launch.exited' : 'launch.exited'), launch_id: launchId, detail: exit?.code ?? exit?.signal ?? null }),
    adopt: async () => adopt,
    persist: snapshot => { persisted = { ...snapshot, failure: snapshot.failure }; },
  };
  const supervisor = new Supervisor(deps);
  return { supervisor, clock, events, handles, alive,
    get persisted() { return persisted; }, setHealthy(value) { healthy = value; }, get launches() { return launches; } };
}

test('supervisor: five starts in one window, each failing, end in failed with the last cause; nothing starts after', async () => {
  const h = harness({ plans: ['fail:import.missing-module'] });
  h.supervisor.boot(); await settle();
  for (let start = 1; start < BUDGET_STARTS; start++) {
    assert.equal(h.supervisor.state, 'backoff', `after start ${start}`);
    assert.equal(h.supervisor.status().attempts, start);
    assert.equal(h.supervisor.status().next_retry_at, new Date(h.clock.now() + BACKOFF_MS[start - 1]).toISOString());
    await h.clock.advance(BACKOFF_MS[start - 1]);
  }
  const status = h.supervisor.status();
  assert.equal(status.state, 'failed');
  assert.equal(status.attempts, BUDGET_STARTS);
  assert.equal(status.failure.key, 'import.missing-module');
  assert.equal(status.failure.attempts, BUDGET_STARTS);
  assert.equal(status.next_retry_at, null);
  await h.clock.advance(WINDOW_MS * 2);
  assert.equal(h.launches, BUDGET_STARTS, 'failed is final until a person asks');
  assert.equal(h.persisted.state, 'failed', 'and the snapshot says so');
});

test('supervisor: alternating causes do not escape the budget', async () => {
  const h = harness({ plans: ['fail:import.missing-module', 'fail:launch.exited', 'fail:import.missing-module', 'fail:bind.port-in-use', 'fail:launch.missing-executable'] });
  h.supervisor.boot(); await settle();
  for (let start = 1; start < BUDGET_STARTS; start++) await h.clock.advance(BACKOFF_MS[start - 1]);
  assert.equal(h.supervisor.state, 'failed');
  assert.equal(h.supervisor.status().failure.key, 'launch.missing-executable', 'the cause is the last one, not the most frequent');
});

test('supervisor: the window survives a supervisor restart — persisted, restored, and the budget is not reset', async () => {
  const clock = fakeClock();
  const first = harness({ plans: ['fail:import.missing-module'], clock });
  first.supervisor.boot(); await settle();
  await clock.advance(BACKOFF_MS[0]); await clock.advance(BACKOFF_MS[1]);
  assert.equal(first.supervisor.status().attempts, 3);
  const snapshot = JSON.parse(JSON.stringify(first.persisted));   // what node-status.json holds
  await first.supervisor.stop();
  // A new supervisor, a minute later: two more starts are all the window has left.
  await clock.advance(60_000);
  const second = harness({ plans: ['fail:import.missing-module'], clock, restored: snapshot });
  second.supervisor.boot(); await settle();
  assert.equal(second.supervisor.status().attempts, 4);
  assert.equal(second.supervisor.status().window_started, snapshot.window_started);
  await clock.advance(BACKOFF_MS[3]);
  assert.equal(second.supervisor.state, 'failed');
  assert.equal(second.launches, 2);
  // Restarted again while failed and still inside the window: it starts nothing, and says why.
  const third = harness({ clock, restored: JSON.parse(JSON.stringify(second.persisted)) });
  third.supervisor.boot(); await settle();
  assert.equal(third.supervisor.state, 'failed');
  assert.equal(third.launches, 0);
  assert.equal(third.supervisor.status().failure.key, 'import.missing-module');
  // A window that has run out is not restored: a supervisor starting after it has its whole budget.
  await clock.advance(WINDOW_MS);
  const later = harness({ clock, restored: JSON.parse(JSON.stringify(second.persisted)) });
  later.supervisor.boot(); await settle();
  assert.equal(later.supervisor.state, 'running');
  assert.equal(later.supervisor.status().attempts, 1);
});

test('supervisor: five continuous minutes running close the window; a crash after that starts a new one', async () => {
  const h = harness({ plans: ['fail:launch.exited', 'ready', 'ready'] });
  h.supervisor.boot(); await settle();
  await h.clock.advance(BACKOFF_MS[0]);
  assert.equal(h.supervisor.state, 'running');
  assert.equal(h.supervisor.status().attempts, 2);
  await h.clock.advance(HEALTHY_MS - 1000);
  assert.equal(h.supervisor.status().attempts, 2, 'not yet');
  await h.clock.advance(1000);
  assert.equal(h.supervisor.status().attempts, 0);
  assert.equal(h.supervisor.status().window_started, null);
  h.handles.at(-1).die({ code: 1 }); await settle();
  assert.equal(h.supervisor.state, 'backoff');
  assert.equal(h.supervisor.status().failure.key, 'launch.exited');
  await h.clock.advance(BACKOFF_MS[0]);
  assert.equal(h.supervisor.state, 'running');
  assert.equal(h.supervisor.status().attempts, 1, 'a fresh window');
});

test('supervisor: a crash before five minutes keeps counting, and an explicit restart closes the window', async () => {
  const h = harness({ plans: ['ready'] });
  h.supervisor.boot(); await settle();
  for (let start = 1; start < BUDGET_STARTS; start++) {
    await h.clock.advance(60_000);
    h.handles.at(-1).die({ code: 1 }); await settle();
    assert.equal(h.supervisor.state, 'backoff');
    await h.clock.advance(BACKOFF_MS[start - 1]);
    assert.equal(h.supervisor.state, 'running');
  }
  h.handles.at(-1).die({ code: 1 }); await settle();
  assert.equal(h.supervisor.state, 'failed', 'running cores dying count as much as launches that never came up');
  h.supervisor.restart(); await settle();
  assert.equal(h.supervisor.state, 'running');
  assert.equal(h.supervisor.status().attempts, 1);
  assert.equal(h.supervisor.status().failure, null);
});

test('supervisor: a hang — three failed probes 30 s apart — is terminated before anything starts again', async () => {
  const h = harness({ plans: ['ready'] });
  h.supervisor.boot(); await settle();
  const wedged = h.handles[0];
  h.setHealthy(false);
  await h.clock.advance(PROBE_MS * 2);
  assert.equal(h.supervisor.state, 'running', 'two misses are not a hang');
  await h.clock.advance(PROBE_MS);
  assert.equal(h.supervisor.state, 'backoff');
  assert.equal(h.supervisor.status().failure.key, 'hang');
  assert.deepEqual(h.events.filter(([what]) => what !== 'exit').map(([what, pid]) => [what, pid]), [['launch', wedged.pid], ['terminate', wedged.pid]]);
  h.setHealthy(true);
  await h.clock.advance(BACKOFF_MS[0]);
  assert.equal(h.supervisor.state, 'running');
  const order = h.events.map(([what, pid]) => `${what}:${pid}`);
  assert.ok(order.indexOf(`exit:${wedged.pid}`) < order.indexOf(`launch:${h.handles[1].pid}`), 'the old one exited before the new one started');
  // A probe that answers again resets the count: misses must be consecutive.
  h.setHealthy(false); await h.clock.advance(PROBE_MS * 2);
  h.setHealthy(true); await h.clock.advance(PROBE_MS);
  h.setHealthy(false); await h.clock.advance(PROBE_MS * 2);
  assert.equal(h.supervisor.state, 'running');
});

test('supervisor: a launch never ready in time is terminated, and is ready.timeout', async () => {
  const h = harness({ plans: ['timeout', 'ready'] });
  h.supervisor.boot(); await settle();
  assert.equal(h.supervisor.state, 'backoff');
  assert.equal(h.supervisor.status().failure.key, 'ready.timeout');
  assert.ok(h.events.some(([what, pid]) => what === 'terminate' && pid === h.handles[0].pid));
  assert.equal(h.alive.size, 0);
  await h.clock.advance(BACKOFF_MS[0]);
  assert.equal(h.supervisor.state, 'running');
});

test('supervisor: a live core of the right launch is adopted at boot — no start counted — and stop terminates it', async () => {
  let resolve; const exit = new Promise(r => { resolve = r; });
  const handle = { pid: 42, exit, done: null };
  const h = harness({ adopt: { handle, core: { pid: 42, launch_id: 'old-launch', version: '0.1.0', api: 1, calls: 1 } } });
  h.supervisor.boot(); await settle();
  assert.equal(h.supervisor.state, 'running');
  assert.equal(h.launches, 0);
  assert.equal(h.supervisor.status().attempts, 0);
  assert.deepEqual(h.supervisor.status().core, { pid: 42, version: '0.1.0', api: 1, launch_id: 'old-launch' });
  assert.equal(h.supervisor.status().calls, 1);
  const stopping = h.supervisor.stop();
  handle.done = { signal: 'SIGTERM' }; resolve(handle.done);
  await stopping;
  assert.equal(h.supervisor.state, 'stopped');
  assert.deepEqual(h.events, [['terminate', 42]]);
});

test('supervisor: ensure starts a stopped core and answers once it settles; a failed one is not started by it', async () => {
  const h = harness({ plans: ['ready'] });
  const status = await h.supervisor.ensure();
  assert.equal(status.state, 'running');
  const failed = harness({ plans: ['fail:import.missing-module'] });
  failed.supervisor.boot(); await settle();
  for (let start = 1; start < BUDGET_STARTS; start++) await failed.clock.advance(BACKOFF_MS[start - 1]);
  assert.equal((await failed.supervisor.ensure()).state, 'failed');
  assert.equal(failed.launches, BUDGET_STARTS);
});

test('cause keys: the core\'s report counts only for its own launch; a stale one from another launch is ignored', () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-cause-'));
  mkdirSync(path.join(dataDir, 'core'), { mode: 0o700 });
  writeFileSync(path.join(dataDir, 'core.log'), 'line 1\nTraceback (most recent call last):\nModuleNotFoundError: No module named \'soxr\'\n');
  const report = launch => writeFileSync(path.join(dataDir, 'core', 'core-failure.json'), JSON.stringify({ launch_id: launch, step: 'import', key: 'import.missing-module', message: 'No module named \'soxr\'', at: '2026-10-01T10:00:00Z' }));
  report('launch-1');
  const first = failureCause({ dataDir, launchId: 'launch-1', exit: { code: 1 } });
  assert.equal(first.key, 'import.missing-module'); assert.equal(first.step, 'import'); assert.equal(first.detail, 'soxr');
  assert.match(first.log_tail.at(-1), /soxr/);
  // The next launch's program is gone; the report on disk is the previous launch's, and says nothing about this one.
  const second = failureCause({ dataDir, launchId: 'launch-2', exit: { error: { code: 'ENOENT', message: 'spawn ENOENT', path: '/gone/sidevoice-core' } } });
  assert.equal(second.key, 'launch.missing-executable'); assert.equal(second.detail, '/gone/sidevoice-core');
  assert.equal(failureCause({ dataDir, launchId: 'launch-3', exit: { error: { code: 'EACCES', path: '/x' } } }).key, 'launch.permission');
  const exited = failureCause({ dataDir, launchId: 'launch-4', exit: { code: 3 } });
  assert.equal(exited.key, 'launch.exited'); assert.equal(exited.detail, 3); assert.ok(exited.log_tail.length > 0);
  assert.equal(failureCause({ dataDir, launchId: 'launch-1', key: 'hang' }).key, 'hang', 'the supervisor\'s own findings win over any report');
  assert.equal(failureCause({ dataDir, launchId: 'launch-5', key: 'ready.timeout' }).key, 'ready.timeout');
});

test('supervisor: two restarts at once, the first core slow to leave — one core at a time, its socket removed only after it exited, before the next starts', async () => {
  const clock = fakeClock();
  const events = [], alive = new Set();
  let pid = 200, endings = [];
  const supervisor = new Supervisor({
    clock, newLaunchId: () => `launch-${pid + 1}`,
    async launch(launchId) {
      assert.equal(alive.size, 0, `no core starts while another is alive (${[...alive]})`);
      const handle = { pid: ++pid, launchId, done: null }; let resolve;
      handle.exit = new Promise(r => { resolve = r; }); handle.die = () => { handle.done = {}; alive.delete(handle.pid); events.push(`exit:${handle.pid}`); resolve({}); };
      alive.add(handle.pid); events.push(`launch:${handle.pid}`); return handle;
    },
    awaitReady: async (handle, launchId) => ({ ready: { pid: handle.pid, launch_id: launchId, api: 1 } }),
    probe: async () => ({ calls: 0 }),
    // A core that ignores SIGTERM: its termination ends only when the test says so.
    terminate: handle => new Promise(resolve => { events.push(`terminate:${handle.pid}`); endings.push(() => { handle.die(); events.push(`unlink-socket-after:${handle.pid}`); resolve(); }); }),
    cause: ({ key }) => ({ key: key || 'launch.exited' }),
  });
  await supervisor.boot();
  assert.equal(supervisor.state, 'running');
  const first = supervisor.restart(), second = supervisor.restart();
  await settle();
  assert.deepEqual(events, ['launch:201', 'terminate:201'], 'nothing launched while the first core is still leaving');
  assert.equal(endings.length, 1, 'and it is asked to leave once, not twice');
  endings.shift()();
  await first; await second; await settle();
  assert.deepEqual(events, ['launch:201', 'terminate:201', 'exit:201', 'unlink-socket-after:201', 'launch:202']);
  assert.equal(alive.size, 1);
  assert.equal(supervisor.state, 'running');
  assert.equal(supervisor.status().core.pid, 202);
  // A stop while a restart waits behind a slow termination: the stop wins, and nothing is left running.
  const restarting = supervisor.restart(); const stopping = supervisor.stop();
  await settle();
  endings.shift()();
  await restarting; await stopping; await settle();
  assert.equal(alive.size, 0);
  assert.equal(supervisor.state, 'stopped');
  assert.ok(!events.includes('launch:203'), 'the superseded restart launched nothing');
});

test('supervisor: a restart while a launch waits to be ready aborts that wait, and the waiting child is ended before the next', async () => {
  const clock = fakeClock();
  const alive = new Set(), events = [];
  let pid = 300;
  const supervisor = new Supervisor({
    clock, newLaunchId: () => `l-${pid + 1}`,
    async launch(launchId) { assert.equal(alive.size, 0); const handle = { pid: ++pid, done: null }; handle.exit = new Promise(() => {}); alive.add(handle.pid); events.push(`launch:${handle.pid}`); return handle; },
    // The first launch never becomes ready: only its abort ends the wait.
    awaitReady: (handle, launchId, signal) => handle.pid === 301
      ? new Promise(resolve => signal.addEventListener('abort', () => resolve({ aborted: true, failure: { key: 'aborted' } })))
      : Promise.resolve({ ready: { pid: handle.pid, launch_id: launchId } }),
    probe: async () => ({ calls: 0 }),
    async terminate(handle) { events.push(`terminate:${handle.pid}`); alive.delete(handle.pid); handle.done = {}; },
    cause: () => ({ key: 'launch.exited' }),
  });
  supervisor.boot(); await settle();
  assert.equal(supervisor.state, 'starting');
  await supervisor.restart(); await settle();
  assert.deepEqual(events, ['launch:301', 'terminate:301', 'launch:302']);
  assert.equal(supervisor.state, 'running');
});

test('supervisor: a refusal before spawning keeps its own key', async () => {
  const supervisor = new Supervisor({ clock: fakeClock(),
    async launch() { throw Object.assign(new Error('unsafe'), { key: 'identity.unsafe-directory' }); },
    awaitReady: async () => ({}), probe: async () => null, terminate: async () => {},
    cause: ({ exit }) => (exit?.error?.key ? { key: exit.error.key } : { key: 'launch.exited' }) });
  await supervisor.boot(); await settle();
  assert.equal(supervisor.status().failure.key, 'identity.unsafe-directory');
});
