/** This machine's core as a plain connector has it (no core job): installed once with uv at the pinned version,
 *  started when absent, linked over its socket with the credential its ready file names, and started again when it
 *  dies — and the core job's, which a connector only links to. `fake-uv.mjs` and `fake-sidevoice-core.mjs` stand in
 *  for the two programs; everything else is the real connector. */
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { CORE_VERSION, NO_UV, coreArgs, coreSpec, runtimeIdentity } from '../core.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeUv = path.join(here, 'fake-uv.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 15_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(25); } throw new Error('timed out waiting'); }

function localHttp(socketPath, method, route, body = null) {
  const payload = body === null ? null : JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const request = http.request({ socketPath, path: route, method, headers: payload === null ? {} : {
      'content-type': 'application/json', 'content-length': Buffer.byteLength(payload),
    } }, response => {
      let text = '';
      response.setEncoding('utf8'); response.on('data', chunk => { text += chunk; });
      response.on('end', () => {
        let json = null; try { json = JSON.parse(text); } catch {}
        resolve({ status: response.statusCode, json, text });
      });
    });
    request.once('error', reject);
    if (payload !== null) request.write(payload);
    request.end();
  });
}

async function coreHttp(url, method, route, token) {
  const response = await fetch(new URL(route, url), { method, headers: token ? { authorization: `Bearer ${token}` } : {} });
  const text = await response.text();
  let json = null; try { json = JSON.parse(text); } catch {}
  return { status: response.status, json, text };
}

function ipc(socketPath) {
  const socket = net.createConnection(socketPath); let buffer = '', serial = 0; const waiting = new Map();
  socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const reply = JSON.parse(line); const w = waiting.get(reply.id); if (!w) continue; waiting.delete(reply.id); reply.ok ? w.resolve(reply.result) : w.reject(new Error(reply.error)); } });
  // A connection closed with requests unanswered (a connector restarted) fails them: a caller polling for a state asks
  // again instead of waiting for ever.
  socket.on('close', () => { for (const w of waiting.values()) w.reject(new Error('connection closed')); waiting.clear(); });
  socket.on('error', () => {});
  return { ready: new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); }),
    call: (method, params) => new Promise((resolve, reject) => { const id = ++serial; waiting.set(id, { resolve, reject }); socket.write(JSON.stringify({ id, method, params }) + '\n'); }),
    end: () => socket.end() };
}

/** A machine with nothing installed: its own data dir, and uv only where the test says. */
function machine(extraEnv = {}, entry = [path.join(packageDir, 'cli.mjs'), 'connector']) {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-core-'));
  const uvLog = path.join(dataDir, 'uv.jsonl');
  const env = { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CONNECTOR_IDLE_MS: '20000', SIDEVOICE_URL: undefined,
    SIDEVOICE_UV: fakeUv, SIDEVOICE_CORE_PORT: '0', FAKE_UV_LOG: uvLog, SIDEVOICE_CORE_SPEC: '', ...extraEnv };
  for (const [key, value] of Object.entries(env)) if (value === undefined) delete env[key];
  const child = spawn(process.execPath, entry, { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let stderr = ''; child.stderr.on('data', d => { stderr += d; });
  const said = () => { try { return readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
  const uvCalls = () => { try { return readFileSync(uvLog, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
  const ready = () => { try { return JSON.parse(readFileSync(path.join(dataDir, 'core', 'core.json'), 'utf8')); } catch { return null; } };
  const stop = () => {
    if (child.exitCode === null) child.kill();
    const core = ready(); if (core?.pid) { try { process.kill(core.pid, 'SIGKILL'); } catch {} }
    for (const line of said()) { try { process.kill(line.pid, 'SIGKILL'); } catch {} }
  };
  return { dataDir, child, said, uvCalls, ready, stop, stderr: () => stderr, socketPath: path.join(dataDir, 'connector.sock') };
}

const register = (facade, ref = 'thread-1') => facade.call('register', { client_ref: ref, harness: 'test', thread: ref, title: 'T',
  delivery: { kind: 'http', url: 'http://127.0.0.1:9/none', thread: ref } });

test('core: the connector installs the pinned core with uv, once, starts it and links to it over loopback with the file\'s credential', async () => {
  const node = machine({ SIDEVOICE_CORE_SPEC: '/wheels/sidevoice_core-' + CORE_VERSION + '-py3-none-any.whl' });
  try {
    await until(() => existsSync(node.socketPath));
    const facade = ipc(node.socketPath); await facade.ready;
    const joined = await register(facade);
    assert.equal(joined.binding_id, 'core-thread-1', 'the binding is the core\'s, over the loopback link');
    // A runtime of its own for this build, never one a core may be running from: nothing to clear.
    const venv = path.join(node.dataDir, 'core-runtime', runtimeIdentity({ SIDEVOICE_CORE_SPEC: '/wheels/sidevoice_core-' + CORE_VERSION + '-py3-none-any.whl' }).id, 'venv');
    assert.match(path.basename(path.dirname(venv)), new RegExp(`^${CORE_VERSION.replace(/\./g, '\\.')}-[0-9a-f]{12}$`));
    assert.deepEqual(node.uvCalls(), [
      ['venv', '--python-preference', 'only-managed', '--python', '3.12', venv],
      ['pip', 'install', '--python', path.join(venv, 'bin', 'python'), '/wheels/sidevoice_core-' + CORE_VERSION + '-py3-none-any.whl'],
    ]);
    const core = node.ready();
    const started = node.said().find(line => line.event === 'started').data.argv;
    assert.equal(started[started.indexOf('--room-credential') + 1], path.join(node.dataDir, 'credentials.json'),
      'the core is told where the machine\'s pairing is, to dial the room with it');
    const handshake = node.said().find(line => line.event === 'handshake');
    assert.equal(handshake.data.connector_id, core.connector_id, 'it links with the credential the core wrote for it');
    assert.ok(node.said().some(line => line.event === 'binding.register' && line.data.thread === 'thread-1'));
    const status = await facade.call('status', {});
    assert.equal(status.connected, true);
    assert.equal(status.core.url, core.url); assert.equal(status.core.pid, core.pid);
    // Speech goes to the core the way it went to a room.
    const said = await facade.call('publish', { binding_id: 'core-thread-1', session_id: 's', revision: 1, text: 'hola' });
    assert.equal(said.status, 'queued');
    facade.end();
  } finally { node.stop(); }
});

test('core: a core that dies is started again without installing again, and every binding registers with it', async () => {
  const node = machine({ SIDEVOICE_CORE_SPEC: '/wheels/x.whl' });
  try {
    await until(() => existsSync(node.socketPath));
    const facade = ipc(node.socketPath); await facade.ready;
    await register(facade, 'thread-a'); await register(facade, 'thread-b');
    const first = node.ready();
    process.kill(first.pid, 'SIGKILL');   // no goodbye: the ready file is left behind, naming a dead pid
    const second = await until(() => { const ready = node.ready(); return ready && ready.pid !== first.pid ? ready : null; });
    await until(() => ['thread-a', 'thread-b'].every(thread =>
      node.said().some(line => line.pid === second.pid && line.event === 'binding.register' && line.data.thread === thread)));
    assert.equal(node.uvCalls().length, 2, 'installed once: the second start found the installation in place');
    const status = await until(async () => { const s = await facade.call('status', {}); return s.connected && s.core?.pid === second.pid ? s : null; });
    assert.equal(status.bindings.length, 2);
    facade.end();
  } finally { node.stop(); }
});

test('core: a running core is found, not started twice, and a connector that leaves does not take it along', async () => {
  const node = machine({ SIDEVOICE_CORE_SPEC: '/wheels/x.whl', SIDEVOICE_CONNECTOR_IDLE_MS: '300' });
  try {
    await until(() => existsSync(node.socketPath));
    let facade = ipc(node.socketPath); await facade.ready;
    await register(facade);
    const core = node.ready();
    facade.end();
    await until(() => node.child.exitCode !== null, 5000);
    assert.doesNotThrow(() => process.kill(core.pid, 0), 'the core outlives its connector: a call may be going on');
    // A new connector, as the next conversation starts one.
    const again = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'connector'], { env: { ...process.env, SIDEVOICE_DATA_DIR: node.dataDir, SIDEVOICE_UV: fakeUv, SIDEVOICE_CORE_PORT: '0', FAKE_UV_LOG: path.join(node.dataDir, 'uv.jsonl'), SIDEVOICE_CORE_SPEC: '/wheels/x.whl', SIDEVOICE_CONNECTOR_IDLE_MS: '20000' }, stdio: 'ignore' });
    try {
      await until(() => existsSync(node.socketPath));
      facade = ipc(node.socketPath); await facade.ready;
      await register(facade, 'thread-2');
      assert.equal(node.ready().pid, core.pid, 'the same core');
      assert.equal(new Set(node.said().map(line => line.pid)).size, 1, 'nobody started a second one');
      facade.end();
    } finally { again.kill(); }
  } finally { node.stop(); }
});

test('core: a device pairing code is the core\'s to issue: asked with nothing running, the connector starts the core and asks it', async () => {
  const node = machine({ SIDEVOICE_CORE_SPEC: '/wheels/x.whl' });
  try {
    await until(() => existsSync(node.socketPath));
    const facade = ipc(node.socketPath); await facade.ready;
    const issued = await facade.call('pair_device', {});
    const core = node.ready();
    assert.deepEqual(issued, { code: 'SV1.fake-' + core.pid, payload: { v: 1, host: 'fake', urls: ['http://127.0.0.1:8768'], rv: null }, expires_in: 600 });
    assert.ok(node.said().some(line => line.pid === core.pid && line.event === 'device.pairing_code'), 'asked of the core over its link');
    facade.end();
  } finally { node.stop(); }
});

test('core: with no uv on the machine, joining says how to install it, and nothing else breaks', async () => {
  const empty = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
  const node = machine({ SIDEVOICE_UV: undefined, PATH: path.dirname(process.execPath), HOME: empty,
    SIDEVOICE_CORE_SPEC: '/wheels/sidevoice_core-' + CORE_VERSION + '-py3-none-any.whl' });
  try {
    await until(() => existsSync(node.socketPath));
    const facade = ipc(node.socketPath); await facade.ready;
    await assert.rejects(register(facade), error => error.message === NO_UV);
    await assert.rejects(facade.call('pair_device', {}), error => error.message === NO_UV, 'a device pairing code needs the core too, and says why');
    const status = await facade.call('status', {});
    assert.equal(status.core_error, NO_UV);
    assert.equal(node.child.exitCode, null, 'the connector is still there for the next attempt');
    facade.end();
  } finally { node.stop(); }
});

test('core: the published bundle installs the wheel it carries, and the pin is that wheel\'s version', async () => {
  // Built the way `npm pack` builds it, with the core's wheel handed to the build.
  const wheels = mkdtempSync(path.join(os.tmpdir(), 'sv-wheel-'));
  const wheel = path.join(wheels, `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`);
  writeFileSync(wheel, 'not really a wheel');
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { env: { ...process.env, SIDEVOICE_CORE_WHEEL: wheel }, stdio: 'ignore' });
  const shipped = path.join(packageDir, 'dist', 'core', path.basename(wheel));
  assert.ok(existsSync(shipped), 'the wheel travels inside dist/, which is what npm publishes');
  const node = machine({}, [path.join(packageDir, 'dist', 'cli.mjs'), 'connector']);
  try {
    await until(() => existsSync(node.socketPath));
    const facade = ipc(node.socketPath); await facade.ready;
    await register(facade);
    assert.equal(node.uvCalls()[1].at(-1), shipped, 'uv installs the wheel beside the bundle, not something from an index');
    facade.end();
  } finally { node.stop(); }
  // A build handed a wheel of another version refuses: the pin and the payload must agree.
  const other = path.join(wheels, 'sidevoice_core-9.9.9-py3-none-any.whl');
  writeFileSync(other, 'x');
  assert.throws(() => execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { env: { ...process.env, SIDEVOICE_CORE_WHEEL: other }, stdio: 'pipe' }));
  // Rebuilt without one, the bundle falls back to the index requirement.
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  assert.equal(coreSpec({ SIDEVOICE_CORE_WHEEL_DIR: path.join(packageDir, 'dist', 'core') }), `sidevoice-core==${CORE_VERSION}`);
});

test('core: the wheel put beside the sources is installed; a core still serving is used as it is, and once it is gone the rebuilt wheel is installed', async () => {
  // A stand-in for `packages/connector/core/`: the tests never write into the source tree.
  const beside = mkdtempSync(path.join(os.tmpdir(), 'sv-beside-'));
  const wheel = path.join(beside, `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`);
  writeFileSync(wheel, 'first build');
  const env = { SIDEVOICE_CORE_WHEEL_DIR: beside };
  let first = null, second = null;
  try {
    assert.equal(coreSpec(env), wheel, 'a wheel beside the sources comes before the index');
    first = machine(env);
    await until(() => existsSync(first.socketPath));
    let facade = ipc(first.socketPath); await facade.ready;
    await register(facade);
    assert.equal(first.uvCalls()[1].at(-1), wheel);
    const oldCore = first.ready().pid;
    // The connector goes; the core it started stays up, as it does for a call in progress.
    facade.end(); first.child.kill();
    await until(() => first.child.exitCode !== null || first.child.signalCode !== null);
    // Rebuilt in place (`uv build` after a pull): same path, other contents. The core serving is nobody's to end —
    // a call may be going on — so the next connector links to it, and installs nothing.
    writeFileSync(wheel, 'second build, longer');
    second = machine({ ...env, SIDEVOICE_DATA_DIR: first.dataDir, FAKE_UV_LOG: path.join(first.dataDir, 'uv.jsonl') });
    await until(() => existsSync(first.socketPath));
    facade = ipc(first.socketPath); await facade.ready;
    await register(facade);
    assert.equal(first.ready().pid, oldCore);
    assert.equal(first.uvCalls().length, 2);
    // Gone (its idle exit, a reboot): the next start installs the rebuilt wheel, a runtime of its own.
    process.kill(oldCore, 'SIGTERM');
    await until(() => first.ready()?.pid !== oldCore && first.ready()?.pid, 20_000);
    assert.equal(first.uvCalls().length, 4, 'the rebuilt wheel was installed');
    facade.end();
  } finally { first?.stop(); second?.stop(); rmSync(beside, { recursive: true, force: true }); }
});

/** A machine for `sidevoice install`: its own HOME with Cursor in it, fake uv, the fake core on a free port. */
function installMachine(extraEnv = {}) {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-install-'));
  mkdirSync(path.join(home, '.cursor'));
  const dataDir = path.join(home, '.sidevoice');
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_UV: fakeUv,
    SIDEVOICE_CORE_PORT: '0', FAKE_UV_LOG: path.join(home, 'uv.jsonl'), SIDEVOICE_CORE_SPEC: '/wheels/x.whl', ...extraEnv };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_CORE_BIN']) delete env[key];
  const uvCalls = () => { try { return readFileSync(env.FAKE_UV_LOG, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
  const ready = () => { try { return JSON.parse(readFileSync(path.join(dataDir, 'core', 'core.json'), 'utf8')); } catch { return null; } };
  const stop = () => { const core = ready(); if (core?.pid) { try { process.kill(core.pid, 'SIGKILL'); } catch {} } };
  return { home, dataDir, env, uvCalls, ready, stop, mcpJson: path.join(home, '.cursor', 'mcp.json') };
}

test('install: the core is installed, started and asked whether it answers before anything is registered, and a code comes at once afterwards', async () => {
  const { install } = await import('../install.mjs');
  const { installInProgress } = await import('../core.mjs');
  const node = installMachine({ FAKE_UV_DELAY_MS: '600' });
  const shown = [];
  let connector = null;
  try {
    const running = install(['--harness', 'cursor', '--no-service'], node.env, { progress: line => shown.push(line) });
    // While it runs, anyone asking sees who is installing: a connector waits for it instead of starting a second uv.
    await until(() => installInProgress(node.dataDir));
    assert.equal(installInProgress(node.dataDir).pid, process.pid);
    const result = await running;
    assert.equal(installInProgress(node.dataDir), null, 'the lock is let go');
    assert.match(shown[0], /Installing this machine's Sidevoice core/);
    assert.ok(shown.some(line => /uv: Resolved 93 packages/.test(line)), 'uv\'s progress is shown as it comes');
    assert.ok(!shown.some(line => /\+ aiortc/.test(line)), 'not every package listed');
    assert.match(result.done.join('\n'), /core 0\.1\.0 is installed and answering at http:\/\/127\.0\.0\.1:\d+/);
    assert.ok(existsSync(node.mcpJson), 'registered once the core answered');
    assert.equal(node.uvCalls().length, 2);
    const core = node.ready();
    // The first voice_pair_device after installing: no install, no start, the code at once.
    connector = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'connector'], { env: { ...node.env, SIDEVOICE_CONNECTOR_IDLE_MS: '20000' }, stdio: 'ignore' });
    const socketPath = path.join(node.dataDir, 'connector.sock');
    await until(() => existsSync(socketPath));
    const facade = ipc(socketPath); await facade.ready;
    const started = Date.now();
    const code = await facade.call('pair_device', {});
    assert.equal(code.code, 'SV1.fake-' + core.pid, 'the core install started is the one that answered');
    assert.ok(Date.now() - started < 5000);
    assert.equal(node.uvCalls().length, 2, 'nothing installed again');
    facade.end();
  } finally { if (connector?.exitCode === null) connector.kill(); node.stop(); }
});

test('install: uv proxy and network failures keep stable keys, and a refused install registers nothing', async () => {
  const { install } = await import('../install.mjs');
  const node = installMachine({ FAKE_UV_FAIL: '1', FAKE_UV_FAIL_OUTPUT: 'error: Request failed after 3 retries\n  Caused by: error sending request for url (https://pypi.org/simple/aiortc/)\n  Caused by: invalid peer certificate: UnknownIssuer' });
  try {
    await assert.rejects(install(['--harness', 'cursor'], node.env), error => error.key === 'install.proxy');
    assert.ok(!existsSync(node.mcpJson), 'no harness points at a voice that is not there');
  } finally { node.stop(); }
  const offline = installMachine({ FAKE_UV_FAIL: '1', FAKE_UV_FAIL_OUTPUT: 'error: Failed to fetch: dns error: failed to lookup address information' });
  try { await assert.rejects(install(['--harness', 'cursor'], offline.env), error => error.key === 'install.network'); }
  finally { offline.stop(); }
  const noUv = installMachine({ SIDEVOICE_UV: '/nonexistent/uv', PATH: '/nonexistent' });
  const { findUv } = await import('../core.mjs');
  if (!findUv(noUv.env)) await assert.rejects(install(['--harness', 'cursor'], noUv.env), error => error.message === NO_UV);   // a uv where installers put it is still found
  // Asked to leave the core for later, it registers and says so.
  const later = installMachine();
  const skipped = await install(['--harness', 'cursor', '--no-core'], later.env);
  assert.match(skipped.done.join('\n'), /not installed now \(--no-core\)/);
  assert.equal(later.uvCalls().length, 0); assert.ok(existsSync(later.mcpJson));
});

test('core: a device code asked during a first install waits for it, within a bound, and past it says the install is still going', async () => {
  const slow = machine({ SIDEVOICE_CORE_SPEC: '/wheels/x.whl', FAKE_UV_DELAY_MS: '2500', SIDEVOICE_CORE_WAIT_MS: '300' });
  try {
    await until(() => existsSync(slow.socketPath));
    const facade = ipc(slow.socketPath); await facade.ready;
    const code = await facade.call('pair_device', {});
    assert.match(code.code, /^SV1\.fake-/, 'waited through the install rather than refusing');
    facade.end();
  } finally { slow.stop(); }
  const slower = machine({ SIDEVOICE_CORE_SPEC: '/wheels/x.whl', FAKE_UV_DELAY_MS: '8000', SIDEVOICE_CORE_WAIT_MS: '300', SIDEVOICE_CORE_INSTALL_WAIT_MS: '1000' });
  try {
    await until(() => existsSync(slower.socketPath));
    const facade = ipc(slower.socketPath); await facade.ready;
    await assert.rejects(facade.call('pair_device', {}), /still being installed \(\d+ s so far; last step: .*\)\. Ask again in a minute; progress is in .*core\.log/);
    facade.end();
  } finally { slower.stop(); }
});

/* ----- a node with the fake core: the core job's, or a plain connector's ----- */

const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');

/** A machine whose core is the fake: `modes` is what each launch of it does, in order (the last repeats). `start()`
 *  runs a connector — plain (`[]`) or the connector job (`['--service']`) — and `startCore()` the core as its job runs
 *  it (no launch id, never idle), without a manager: what the two jobs would be. */
export function fakeNode({ modes = ['ok'], env: extra = {}, dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-node-')) } = {}) {
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-bin-'));
  const bin = path.join(tools, 'sidevoice-core');
  writeFileSync(bin, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  const modesFile = path.join(tools, 'modes');
  writeFileSync(modesFile, modes.join('\n') + '\n');
  const env = { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CORE_BIN: bin, FAKE_CORE_MODES: modesFile, FAKE_CORE_WRAPPER: bin,
    SIDEVOICE_CORE_PORT: '0', SIDEVOICE_CORE_STOP_GRACE_MS: '1000', SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_CONNECTOR_IDLE_MS: '20000', ...extra };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_TEST_HOOKS']) delete env[key];
  for (const [key, value] of Object.entries(extra)) if (value === undefined) delete env[key];
  const children = [];
  const node = {
    dataDir, bin, env, modesFile, children, socketPath: path.join(dataDir, 'connector.sock'),
    /** A connector: plain (`[]`) or the connector job (`['--service']`). */
    start(args = [], more = {}) {
      const child = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'connector', ...args], { env: { ...env, ...more }, stdio: ['ignore', 'pipe', 'pipe'] });
      child.stderrText = ''; child.stderr.on('data', d => { child.stderrText += d; });
      children.push(child); return child;
    },
    /** The core, as the core job starts it. */
    startCore(more = {}) {
      mkdirSync(path.join(dataDir, 'core'), { recursive: true, mode: 0o700 });
      const child = spawn(node.bin, coreArgs({ dataDir, env, idleExit: 0, roomCredential: path.join(dataDir, 'credentials.json') }), { env: { ...env, ...more }, stdio: 'ignore' });
      children.push(child); return child;
    },
    said() { try { return readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } },
    ready() { try { return JSON.parse(readFileSync(path.join(dataDir, 'core', 'core.json'), 'utf8')); } catch { return null; } },
    async ask(method, params = {}) { const facade = ipc(node.socketPath); await facade.ready; try { return await facade.call(method, params); } finally { facade.end(); } },
    /** The status once `check` holds for it. */
    status(check, timeout = 15_000) {
      return until(async () => { try { const status = await node.ask('node.status'); return check(status) ? status : null; } catch { return null; } }, timeout);
    },
    log() { try { return readFileSync(path.join(dataDir, 'connector.log'), 'utf8'); } catch { return ''; } },
    stop() {
      for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      // Whatever else runs here: a connector a launcher spawned, and every core.
      try { process.kill(JSON.parse(readFileSync(path.join(dataDir, 'connector.lock'), 'utf8')).pid, 'SIGKILL'); } catch {}
      for (const line of node.said()) { try { process.kill(line.pid, 'SIGKILL'); } catch {} }
      // And anything started for this node that has not said so yet — a detached core a plain connector spawned just
      // before the test ended: found by its data directory on its command line.
      try {
        for (const line of execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' }).split('\n')) {
          const match = line.trim().match(/^(\d+)\s+(.*)$/);
          if (match && Number(match[1]) !== process.pid && match[2].includes(`--data-dir ${path.join(dataDir, 'core')}`)) { try { process.kill(Number(match[1]), 'SIGKILL'); } catch {} }
        }
      } catch {}
    },
  };
  return node;
}
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

test('core job and connector job: the connector never starts a core — it links when the job\'s core answers, again after that core is restarted, and a connector restart leaves the core running', async () => {
  const node = fakeNode();
  try {
    const connector = node.start(['--service']);
    await until(() => existsSync(node.socketPath));
    await wait(1500);
    assert.deepEqual(node.said(), [], 'nobody started a core');
    assert.equal((await node.ask('node.status')).connector.running, true);
    const core = node.startCore();
    const first = await node.status(s => s.state === 'running' || s.reachable);
    assert.equal(first.core.launch_id, node.ready().launch_id, 'the core made its own launch id');
    await until(() => node.said().some(line => line.event === 'handshake'));
    // The core restarted (as its manager would): linked again, to the new launch, with nothing asked.
    core.kill('SIGKILL');
    await wait(200);
    node.startCore();
    await until(() => node.said().filter(line => line.event === 'handshake').length >= 2 && node.said().at(-1).pid !== core.pid, 15_000);
    // The connector restarted: the core is not its child, and keeps serving.
    const serving = node.ready().pid;
    connector.kill('SIGKILL');
    await wait(300);
    assert.equal(alive(serving), true, 'the core outlives the connector');
    node.start(['--service']);
    await until(() => node.said().filter(line => line.event === 'handshake' && line.pid === serving).length >= 2, 15_000);
  } finally { node.stop(); }
});

test('core: one core per data directory — a second start exits 75 with bind.core-running, and the first one\'s socket keeps answering', async () => {
  const node = fakeNode();
  try {
    node.startCore();
    const first = await until(() => node.ready());
    const second = spawnSync(node.bin, coreArgs({ dataDir: node.dataDir, env: node.env, idleExit: 0 }), { env: node.env, encoding: 'utf8', timeout: 15_000 });
    assert.equal(second.status, 75);
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'core', 'core-failure.json'), 'utf8')).key, 'bind.core-running');
    assert.equal(node.ready().pid, first.pid);
    const health = await new Promise(resolve => {
      const request = (net.createConnection(first.socket).on('connect', function () { this.write('GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n'); })
        .on('data', chunk => resolve(String(chunk))).on('error', () => resolve('')));
      return request;
    });
    assert.match(health, /^HTTP\/1\.1 200/);
    // A failed start before ready is not to be restarted: exit 0, the report says why.
    const failing = fakeNode({ modes: ['import'] });
    const ran = spawnSync(failing.bin, coreArgs({ dataDir: failing.dataDir, env: failing.env, idleExit: 0 }), { env: failing.env, encoding: 'utf8', timeout: 15_000 });
    assert.equal(ran.status, 0);
    assert.equal(JSON.parse(readFileSync(path.join(failing.dataDir, 'core', 'core-failure.json'), 'utf8')).key, 'import.missing-module');
    // The next start that holds the directory clears that report itself.
    writeFileSync(failing.modesFile, 'ok\n');
    failing.startCore();
    await until(() => failing.ready());
    assert.equal(existsSync(path.join(failing.dataDir, 'core', 'core-failure.json')), false);
    failing.stop();
  } finally { node.stop(); }
});

/** The real core when a checkout of it is at hand — `SIDEVOICE_INTEROP_CORE_DIR`, or a `core` checkout beside
 *  this repository — with its environment in `.venv`; skipped otherwise, except where `SIDEVOICE_INTEROP_REQUIRED=1`
 *  (the CI job that installs reviewed core #38): there a missing core is a failure, never a skip. */
const interopCore = process.env.SIDEVOICE_INTEROP_CORE_DIR || path.join(packageDir, '..', '..', '..', 'core');
const interopPython = path.join(interopCore, '.venv', 'bin', 'python');
const interopRequired = process.env.SIDEVOICE_INTEROP_REQUIRED === '1';
const interopSkip = !interopRequired && !existsSync(interopPython) && `no core checkout with a .venv at ${interopCore}`;
function realNode() {
  assert.ok(existsSync(interopPython), `the real core is required here, and there is none at ${interopCore}`);
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-real-'));
  const bin = path.join(tools, 'sidevoice-core');
  writeFileSync(bin, `#!/bin/sh\nexec "${interopPython}" -m sidevoice_core.server "$@"\n`, { mode: 0o755 });
  const node = fakeNode({ env: { SIDEVOICE_CORE_STOP_GRACE_MS: '15000' } });
  node.bin = bin;
  return node;
}

test('interop: the connector job links to the real core it did not start — ready and healthy on its socket, linked there with no Origin, a binding and a device code from it; the core outlives the connector', { skip: interopSkip }, async () => {
  const node = realNode();
  try {
    node.startCore();
    const connector = node.start(['--service']);
    const running = await node.status(s => s.state === 'running' || s.reachable, 90_000);
    // Health answers as soon as it listens; the ready file follows.
    const ready = await until(() => node.ready(), 30_000);
    assert.equal(running.core.api, 1);
    assert.equal(running.core.launch_id, ready.launch_id, 'a launch id the core made itself');
    assert.equal(node.ready().socket, path.join(node.dataDir, 'core', 'local.sock'));
    const facade = ipc(node.socketPath); await facade.ready;
    const joined = await until(async () => { try { return await register(facade, 'interop-thread'); } catch { return null; } }, 30_000);
    assert.ok(joined.binding_id && !joined.binding_id.startsWith('local-'), `the core minted the binding (${joined.binding_id})`);
    const code = await facade.call('pair_device', {});
    assert.match(code.code, /^SV1\./, 'a device code, which only the link on the socket can ask for');
    // The link is not on TCP: the same path there is not found.
    const tcp = await fetch(new URL('/api/connectors/link/?EIO=4&transport=polling', node.ready().url)).then(response => response.status).catch(() => null);
    assert.equal(tcp, 404);
    facade.end();
    connector.kill('SIGTERM');
    await until(() => connector.exitCode !== null, 30_000);
    assert.equal(alive(running.core.pid), true, 'the core is not the connector\'s: it keeps running');
  } finally { node.stop(); }
});

test('interop: core #38 authenticates host agent API calls and relays the connector key and no-connector failure', { skip: interopSkip }, async () => {
  const node = realNode();
  let connector = null;
  try {
    const coreCommit = execFileSync('git', ['-C', interopCore, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
    assert.equal(coreCommit, '59e32df778c7ac68590f05310b2df8db91f5873f', 'this interop must exercise the reviewed core #38 head');
    node.startCore();
    const ready = await until(() => node.ready(), 90_000);
    connector = node.start(['--service']);
    await node.status(status => status.reachable, 90_000);
    await until(() => existsSync(node.socketPath), 30_000);

    const pairing = await localHttp(ready.socket, 'POST', '/api/device/local/pair', { name: 'R2 connector interop' });
    assert.equal(pairing.status, 200, pairing.text);
    assert.ok(pairing.json?.token, 'the core issued the device credential through its local-only socket');

    const unauthenticated = await coreHttp(ready.url, 'GET', '/api/host/agents?rescan=1&watch=codex');
    assert.equal(unauthenticated.status, 401, 'the host-agent API requires a paired device');

    const listed = await until(async () => {
      const response = await coreHttp(ready.url, 'GET', '/api/host/agents?rescan=1&watch=codex', pairing.json.token);
      return response.status === 200 ? response : null;
    }, 90_000);
    assert.equal(listed.status, 200, listed.text);
    assert.deepEqual(Object.keys(listed.json || {}).sort(), ['agents', 'custom', 'scanned_at']);
    assert.ok(Array.isArray(listed.json?.agents));
    assert.ok(listed.json?.custom && typeof listed.json.custom === 'object');

    const keyedFailure = await coreHttp(ready.url, 'POST', '/api/host/agents/not-registered/connect', pairing.json.token);
    assert.equal(keyedFailure.status, 409, keyedFailure.text);
    assert.equal(keyedFailure.json?.error?.key, 'agents.unknown');
    assert.equal(keyedFailure.json?.error?.params?.id, 'not-registered');
    assert.doesNotMatch(keyedFailure.text, /stderr|stdout|command output/i);

    connector.kill('SIGTERM');
    await until(() => connector.exitCode !== null, 30_000);
    const withoutConnector = await until(async () => {
      const response = await coreHttp(ready.url, 'GET', '/api/host/agents?rescan=0', pairing.json.token);
      return response.status === 503 ? response : null;
    }, 10_000);
    assert.deepEqual(withoutConnector.json, { key: 'no-connector' });
  } finally { node.stop(); }
});

test('interop: the real core holds its data directory — a second start exits 75 with bind.core-running, and a start that fails before ready exits 0 with its key', { skip: interopSkip }, async () => {
  const node = realNode();
  try {
    node.startCore();
    const first = await until(() => node.ready(), 90_000);
    const second = spawnSync(node.bin, coreArgs({ dataDir: node.dataDir, env: node.env, idleExit: 0 }), { env: node.env, encoding: 'utf8', timeout: 90_000 });
    assert.equal(second.status, 75, second.stderr);
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'core', 'core-failure.json'), 'utf8')).key, 'bind.core-running');
    assert.equal(node.ready()?.pid, first.pid, 'the first core still serves');
    // Its port taken by somebody else: a failure before ready, exit 0 — the manager does not start it again.
    const taken = net.createServer().listen(0, '127.0.0.1');
    await new Promise(resolve => taken.once('listening', resolve));
    const other = fakeNode();
    const ran = spawnSync(node.bin, coreArgs({ dataDir: other.dataDir, env: { ...other.env, SIDEVOICE_CORE_PORT: String(taken.address().port) }, idleExit: 0 }), { env: other.env, encoding: 'utf8', timeout: 90_000 });
    taken.close();
    assert.equal(ran.status, 0, ran.stderr);
    assert.equal(JSON.parse(readFileSync(path.join(other.dataDir, 'core', 'core-failure.json'), 'utf8')).key, 'bind.port-in-use');
  } finally { node.stop(); }
});
