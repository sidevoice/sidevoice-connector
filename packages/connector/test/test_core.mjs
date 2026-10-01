/** The connector as the supervisor of this machine's core: installed once with uv at the pinned
 *  version, started when absent, linked over loopback with the credential its ready file names, and
 *  started again when it dies. `fake-uv.mjs` and `fake-sidevoice-core.mjs` stand in for the two
 *  programs; everything else is the real connector. */
import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { CORE_VERSION, NO_UV, coreSpec } from '../core.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeUv = path.join(here, 'fake-uv.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 15_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(25); } throw new Error('timed out waiting'); }

function ipc(socketPath) {
  const socket = net.createConnection(socketPath); let buffer = '', serial = 0; const waiting = new Map();
  socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const reply = JSON.parse(line); const w = waiting.get(reply.id); if (!w) continue; waiting.delete(reply.id); reply.ok ? w.resolve(reply.result) : w.reject(new Error(reply.error)); } });
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
    const venv = path.join(node.dataDir, 'core-runtime', CORE_VERSION, 'venv');
    assert.deepEqual(node.uvCalls(), [
      ['venv', '--clear', '--python-preference', 'only-managed', '--python', '3.12', venv],
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
  const node = machine({ SIDEVOICE_UV: undefined, PATH: path.dirname(process.execPath), HOME: empty });
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

test('core: the wheel put beside the sources is installed, and once rebuilt it is installed again and replaces the core still running', async () => {
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
    // Rebuilt in place (`uv build` after a pull): same path, other contents.
    writeFileSync(wheel, 'second build, longer');
    second = machine({ ...env, SIDEVOICE_DATA_DIR: first.dataDir, FAKE_UV_LOG: path.join(first.dataDir, 'uv.jsonl') });
    await until(() => existsSync(first.socketPath));
    facade = ipc(first.socketPath); await facade.ready;
    await register(facade);
    assert.equal(first.uvCalls().length, 4, 'the rebuilt wheel was installed');
    assert.notEqual(first.ready().pid, oldCore, 'and the core of the earlier build was asked to leave');
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
    const running = install(['--harness', 'cursor'], node.env, { progress: line => shown.push(line) });
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

test('install: a uv failure says what uv said and what usually fixes it — system certificates behind a re-signing proxy — and registers nothing', async () => {
  const { install } = await import('../install.mjs');
  const node = installMachine({ FAKE_UV_FAIL: '1', FAKE_UV_FAIL_OUTPUT: 'error: Request failed after 3 retries\n  Caused by: error sending request for url (https://pypi.org/simple/aiortc/)\n  Caused by: invalid peer certificate: UnknownIssuer' });
  try {
    await assert.rejects(install(['--harness', 'cursor'], node.env), error => {
      assert.match(error.message, /failed at "uv venv" \(exit 2\): .*UnknownIssuer/);
      assert.match(error.message, /UV_SYSTEM_CERTS=1/); assert.match(error.message, /SSL_CERT_FILE/);
      assert.match(error.message, /core\.log/);
      return true;
    });
    assert.ok(!existsSync(node.mcpJson), 'no harness points at a voice that is not there');
  } finally { node.stop(); }
  const offline = installMachine({ FAKE_UV_FAIL: '1', FAKE_UV_FAIL_OUTPUT: 'error: Failed to fetch: dns error: failed to lookup address information' });
  await assert.rejects(install(['--harness', 'cursor'], offline.env), /could not reach the network.*HTTPS_PROXY/);
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

/* ----- the node service's supervisor (§4.2), with the fake core's failure modes ----- */

const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');

/** A machine whose core is the fake, run by a supervisor (`connector --supervise`): `modes` is what each
 *  launch of the core does, in order (the last repeats). Timings shortened: probes 300 ms apart, 150 ms of
 *  backoff, 1 s for a core to leave before it is killed. */
export function supervisedNode({ modes = ['ok'], env: extra = {}, dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-node-')) } = {}) {
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-bin-'));
  const bin = path.join(tools, 'sidevoice-core');
  writeFileSync(bin, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  const modesFile = path.join(tools, 'modes');
  writeFileSync(modesFile, modes.join('\n') + '\n');
  const env = { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CORE_BIN: bin, FAKE_CORE_MODES: modesFile, FAKE_CORE_WRAPPER: bin,
    SIDEVOICE_CORE_PORT: '0', SIDEVOICE_PROBE_MS: '300', SIDEVOICE_BACKOFF_MS: '150', SIDEVOICE_CORE_STOP_GRACE_MS: '1000',
    SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_CONNECTOR_IDLE_MS: '20000', ...extra };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN']) delete env[key];
  const children = [];
  const node = {
    dataDir, bin, env, modesFile, children, socketPath: path.join(dataDir, 'connector.sock'),
    /** One more supervisor (`--supervise`), or a plain connector (`[]`). */
    start(args = ['--supervise'], more = {}) {
      const child = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'connector', ...args], { env: { ...env, ...more }, stdio: ['ignore', 'pipe', 'pipe'] });
      child.stderrText = ''; child.stderr.on('data', d => { child.stderrText += d; });
      children.push(child); return child;
    },
    said() { try { return readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } },
    ready() { try { return JSON.parse(readFileSync(path.join(dataDir, 'core', 'core.json'), 'utf8')); } catch { return null; } },
    async ask(method, params = {}) { const facade = ipc(node.socketPath); await facade.ready; try { return await facade.call(method, params); } finally { facade.end(); } },
    /** The status once `check` holds for it. */
    status(check, timeout = 15_000) {
      return until(async () => { try { const status = await node.ask('node.status'); return check(status) ? status : null; } catch { return null; } }, timeout);
    },
    log() { try { return readFileSync(path.join(dataDir, 'node-service.log'), 'utf8'); } catch { return ''; } },
    stop() {
      for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      for (const line of node.said()) { try { process.kill(line.pid, 'SIGKILL'); } catch {} }
    },
  };
  return node;
}
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

test('supervise: the core is the supervisor\'s child — never idle, one launch id each start — and a core that dies is started again', async () => {
  const node = supervisedNode();
  try {
    const supervisor = node.start();
    const running = await node.status(s => s.state === 'running');
    assert.equal(running.service, 'none'); assert.equal(running.supervisor, true); assert.equal(running.attempts, 1);
    assert.equal(running.core.pid, node.ready().pid); assert.equal(running.core.launch_id, node.ready().launch_id); assert.equal(running.core.api, 1);
    const argv = node.said().find(line => line.event === 'started').data.argv;
    assert.equal(argv[argv.indexOf('--idle-exit') + 1], '0', 'the service\'s core never leaves on its own');
    assert.equal(argv[argv.indexOf('--launch-id') + 1], running.core.launch_id);
    assert.equal(argv[argv.indexOf('--socket') + 1], path.join(node.dataDir, 'core', 'local.sock'));
    assert.equal(statSync(path.join(node.dataDir, 'core')).mode & 0o777, 0o700, 'the core\'s directory is this user\'s alone');
    // Linked over the core's socket with the credential the core wrote: a binding registers there.
    const facade = ipc(node.socketPath); await facade.ready;
    assert.equal((await register(facade)).binding_id, 'core-thread-1');
    assert.ok(!node.said().some(line => line.event === 'refused'), 'the link and the probes carry no Origin and a loopback Host');
    // Calls come fresh from the core's health.
    writeFileSync(path.join(node.dataDir, 'core', 'fake-calls'), '2');
    assert.equal((await facade.call('node.status', {})).calls, 2);
    writeFileSync(path.join(node.dataDir, 'core', 'fake-calls'), '0');
    // The core dies: backoff, then a new launch, and the binding registers with it under the same id.
    process.kill(running.core.pid, 'SIGKILL');
    const again = await node.status(s => s.state === 'running' && s.core.pid !== running.core.pid, 20_000);
    assert.notEqual(again.core.launch_id, running.core.launch_id);
    assert.equal(again.attempts, 2);
    await until(() => node.said().some(line => line.pid === again.core.pid && line.event === 'binding.register' && line.data.binding_id === 'core-thread-1'));
    assert.match(node.log(), /went away/, 'the supervisor writes node-service.log');
    assert.equal(supervisor.stderrText, '', 'and nothing to stderr: the service manager puts that in the same file');
    const snapshot = JSON.parse(readFileSync(path.join(node.dataDir, 'node-status.json'), 'utf8'));
    assert.equal(snapshot.state, 'running'); assert.equal(snapshot.attempts, 2); assert.ok(snapshot.window_started);
    facade.end();
    // The supervisor is stopped (as launchd or systemd stop it): its core leaves with it, the socket after it.
    supervisor.kill('SIGTERM');
    await until(() => supervisor.exitCode !== null);
    assert.equal(alive(again.core.pid), false);
    assert.equal(existsSync(path.join(node.dataDir, 'core', 'local.sock')), false);
  } finally { node.stop(); }
});

test('supervise: an import failure every time ends in failed with its cause; node.restart is a person\'s retry and closes the window', async () => {
  const node = supervisedNode({ modes: ['import'] });
  try {
    node.start();
    const failed = await node.status(s => s.state === 'failed', 20_000);
    assert.equal(failed.failure.key, 'import.missing-module');
    assert.equal(failed.failure.step, 'import');
    assert.equal(failed.failure.detail, 'soxr');
    assert.equal(failed.failure.attempts, 5);
    assert.equal(failed.attempts, 5);
    assert.ok(failed.failure.log_tail.some(line => /soxr/.test(line)), 'with the log tail');
    assert.equal(node.said().filter(line => line.event === 'started').length, 5, 'five starts');
    await wait(800);
    assert.equal(node.said().filter(line => line.event === 'started').length, 5, 'and then none');
    // The person fixed it and retries.
    writeFileSync(node.modesFile, 'ok\n');
    await node.ask('node.restart');
    const running = await node.status(s => s.state === 'running');
    assert.equal(running.attempts, 1, 'a fresh window');
    assert.equal(running.failure, null);
  } finally { node.stop(); }
});

test('supervise: alternating causes — an exit, an import error, a missing program — each read for its own launch, never a stale one', async () => {
  const node = supervisedNode({ modes: ['exit:3', 'import', 'exit:4', 'import+vanish'] });
  // What a previous life left: a ready file and a failure report, both of another launch.
  mkdirSync(path.join(node.dataDir, 'core'), { recursive: true, mode: 0o700 });
  writeFileSync(path.join(node.dataDir, 'core', 'core.json'), JSON.stringify({ pid: 999_999, port: 1, url: 'http://127.0.0.1:1', socket: path.join(node.dataDir, 'core', 'local.sock'), launch_id: 'stale', connector_id: 'x', token: 'y', version: '0.1.0', protocol: 2, api: 1 }));
  writeFileSync(path.join(node.dataDir, 'core', 'core-failure.json'), JSON.stringify({ launch_id: 'stale', step: 'identity', key: 'identity.unreadable', message: 'old news', at: '2026-01-01T00:00:00Z' }));
  try {
    node.start();
    const seen = [];
    const failed = await node.status(s => {
      const entry = s.failure ? `${s.failure.attempts}:${s.failure.key}` : null;
      if (entry && seen.at(-1) !== entry) seen.push(entry);
      return s.state === 'failed';
    }, 20_000);
    assert.equal(failed.failure.key, 'launch.missing-executable');
    assert.equal(failed.failure.detail, node.bin);
    assert.ok(!seen.some(entry => entry.endsWith('identity.unreadable')), 'the stale report of another launch is never a cause');
    assert.ok(seen.includes('4:import.missing-module') && seen.includes('5:launch.missing-executable'), seen.join(' '));
    assert.ok(seen.some(entry => entry.endsWith('launch.exited')), seen.join(' '));
  } finally { node.stop(); }
});

test('supervise: a hang — the core alive and its health silent — is terminated, killed when it ignores SIGTERM, before the next one starts', async () => {
  const node = supervisedNode({ modes: ['hang:300+stubborn', 'ok'] });
  try {
    node.start();
    const first = await node.status(s => s.state === 'running');
    const next = await node.status(s => s.state === 'running' && s.core.pid !== first.core.pid, 20_000);
    assert.equal(alive(first.core.pid), false, 'the wedged core is gone');
    const said = node.said();
    const term = said.findIndex(line => line.pid === first.core.pid && line.event === 'sigterm');
    const started = said.findIndex(line => line.pid === next.core.pid && line.event === 'started');
    assert.ok(term >= 0 && term < started, 'asked to leave before the next one started');
    assert.match(node.log(), /did not answer 3 health probes/);
    assert.match(node.log(), /did not leave within 1 s; killing it/, 'SIGKILL after the grace period');
    assert.equal(next.attempts, 2);
  } finally { node.stop(); }
});

test('supervise: a SIGKILLed supervisor leaves its core running — the next one adopts it if it answers for its launch, else ends it', async () => {
  const node = supervisedNode({ env: { SIDEVOICE_PROBE_MS: '60000' }, modes: ['ok', 'hang:1500', 'ok'] });
  try {
    const first = node.start();
    const running = await node.status(s => s.state === 'running');
    first.kill('SIGKILL');
    await until(() => first.signalCode !== null);
    assert.ok(alive(running.core.pid), 'nothing took the core down with it');
    // The next supervisor (a login, the service manager): the same core, no start counted.
    const second = node.start();
    const adopted = await node.status(s => s.state === 'running');
    assert.equal(adopted.core.pid, running.core.pid);
    assert.equal(adopted.core.launch_id, running.core.launch_id);
    assert.equal(adopted.attempts, running.attempts, 'no start counted (the window is the one persisted before)');
    assert.equal(node.said().filter(line => line.event === 'started').length, 1);
    // Its core replaced by one that stops answering once ready; the supervisor is SIGKILLed before it notices.
    await node.ask('node.restart');
    const wedged = (await node.status(s => s.state === 'running' && s.core.pid !== running.core.pid)).core;
    second.kill('SIGKILL');
    await wait(1600);
    await until(() => second.signalCode !== null);
    assert.ok(alive(wedged.pid));
    node.start();
    const replaced = await node.status(s => s.state === 'running' && s.core.pid !== wedged.pid, 20_000);
    assert.equal(alive(wedged.pid), false, 'a core that does not answer for its own launch is ended, not adopted');
    assert.ok(replaced.core.launch_id);
  } finally { node.stop(); }
});
