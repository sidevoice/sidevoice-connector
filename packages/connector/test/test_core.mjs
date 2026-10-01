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
import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { CORE_VERSION, NO_UV, coreSpec } from '../core.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeUv = path.join(here, 'fake-uv.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 15_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(25); } throw new Error('timed out waiting'); }

function ipc(socketPath) {
  const socket = net.createConnection(socketPath); let buffer = '', serial = 0; const waiting = new Map();
  socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const reply = JSON.parse(line); const w = waiting.get(reply.id); waiting.delete(reply.id); reply.ok ? w.resolve(reply.result) : w.reject(new Error(reply.error)); } });
  return { ready: new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); }),
    call: (method, params) => new Promise((resolve, reject) => { const id = ++serial; waiting.set(id, { resolve, reject }); socket.write(JSON.stringify({ id, method, params }) + '\n'); }),
    end: () => socket.end() };
}

/** A machine with nothing installed: its own data dir, and uv only where the test says. */
function machine(extraEnv = {}, entry = [path.join(packageDir, 'connector.mjs')]) {
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
    const again = spawn(process.execPath, [path.join(packageDir, 'connector.mjs')], { env: { ...process.env, SIDEVOICE_DATA_DIR: node.dataDir, SIDEVOICE_UV: fakeUv, SIDEVOICE_CORE_PORT: '0', FAKE_UV_LOG: path.join(node.dataDir, 'uv.jsonl'), SIDEVOICE_CORE_SPEC: '/wheels/x.whl', SIDEVOICE_CONNECTOR_IDLE_MS: '20000' }, stdio: 'ignore' });
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
    assert.deepEqual(issued, { code: 'SV1.fake-' + core.pid, payload: { v: 1, host: 'fake' }, expires_in: 600 });
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
    connector = spawn(process.execPath, [path.join(packageDir, 'connector.mjs')], { env: { ...node.env, SIDEVOICE_CONNECTOR_IDLE_MS: '20000' }, stdio: 'ignore' });
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
