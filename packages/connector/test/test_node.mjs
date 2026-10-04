/** Sidevoice at login (§2.1–§2.9): the two jobs under stand-ins of launchd and systemd (`fake-service-manager.mjs`),
 *  installed for real by `sidevoice install --service`; the one launcher and its stop marker; the façade's reconnect
 *  after the connector is restarted; and `deriveStatus()` row by row. The fake core plays the core throughout; the
 *  real managers are `service-integration.mjs`'s, in CI. */
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { appendFileSync, chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { fakeNode } from './test_core.mjs';
import { deriveStatus, parseLaunchd, parseSystemd, plistText, serviceEnvironment, unitText } from '../service.mjs';
import { t } from '../i18n.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(here, '..', 'cli.mjs');
const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');
const fakeManager = path.join(here, 'fake-service-manager.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 15_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(25); } throw new Error('timed out waiting'); }
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

/** An environment with no harness of the machine running these tests in it: a test façade is who the test says. */
function clean(env) {
  return Object.fromEntries(Object.entries(env).filter(([name]) => !/^(CLAUDE|CODEX|CURSOR)_/.test(name)));
}

/** A stdio MCP façade (`sidevoice mcp`), as a harness runs it. */
function facade(env, clientInfo = { name: 'test', version: '1' }, capabilities = {}) {
  const child = spawn(process.execPath, [cli, 'mcp'], { env: clean(env), stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = []; let out = '', serial = 0;
  child.stdout.on('data', d => { out += d; let i; while ((i = out.indexOf('\n')) >= 0) { replies.push(JSON.parse(out.slice(0, i))); out = out.slice(i + 1); } });
  const send = (method, params) => { const id = ++serial; child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n'); return until(() => replies.find(r => r.id === id), 20_000); };
  const call = async (name, args = {}) => { const reply = await send('tools/call', { name, arguments: args }); if (reply.error) throw new Error(reply.error.message); return { raw: reply.result, value: JSON.parse(reply.result.content[0].text) }; };
  return { child, send, call, ready: send('initialize', { protocolVersion: '2025-11-25', clientInfo, capabilities }) };
}

/** What the fake core delivered, and how the connector answered, by event id. */
function delivered(node, eventId) {
  try { return readFileSync(path.join(node.dataDir, 'core', 'delivered.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse).find(line => line.frame.event_id === eventId) || null; } catch { return null; }
}
const deliver = (node, frame) => appendFileSync(path.join(node.dataDir, 'core', 'deliver.jsonl'), JSON.stringify(frame) + '\n');

/** A machine with Sidevoice installed for real — `sidevoice install --no-agents --service`, this checkout as the
 *  release, the fake core as its core (a wrapper with the launch modes baked in: a job carries only Sidevoice's own
 *  settings) — under a stand-in manager (`kind`), or none. */
async function managedNode(kind, { modes = ['ok'], install = true, extra = {} } = {}) {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-svc-'));
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-mgr-'));
  const state = path.join(tools, 'state');
  const manager = path.join(tools, kind === 'launchd' ? 'launchctl' : 'systemctl');
  writeFileSync(manager, `#!/bin/sh\nFAKE_MANAGER_DIR="${state}" exec "${process.execPath}" "${fakeManager}" ${kind === 'launchd' ? 'launchctl' : 'systemctl'} "$@"\n`, { mode: 0o755 });
  const modesFile = path.join(tools, 'modes');
  writeFileSync(modesFile, modes.join('\n') + '\n');
  const core = path.join(tools, 'sidevoice-core');
  writeFileSync(core, `#!/bin/sh\nFAKE_CORE_MODES="${modesFile}" exec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  const dataDir = path.join(home, '.sidevoice');
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir,
    SIDEVOICE_CORE_BIN: core, SIDEVOICE_CORE_PORT: '0', SIDEVOICE_SERVICE_MANAGER: kind, SIDEVOICE_LAUNCHCTL: manager, SIDEVOICE_SYSTEMCTL: manager,
    SIDEVOICE_LOGINCTL: '/bin/false', SIDEVOICE_TEARDOWN_MS: '3000', SIDEVOICE_SERVICE_START_WAIT_MS: '3000', SIDEVOICE_INSTALL_VERIFY_MS: '15000', ...extra };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_TEST_HOOKS', 'FAKE_CORE_MODES', 'FAKE_CORE_MODE']) delete env[key];
  const run = async (args, more = {}) => {
    const child = spawn(process.execPath, [cli, ...args, '--json'], { env: { ...env, ...more }, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '', err = ''; child.stdout.on('data', d => { out += d; }); child.stderr.on('data', d => { err += d; });
    const code = await new Promise(resolve => child.on('exit', resolve));
    try { return { code, ...JSON.parse(out.trim().split('\n').at(-1)) }; } catch { throw new Error(`${args.join(' ')}: ${out}${err}`); }
  };
  const read = (job, name) => { try { return readFileSync(path.join(state, job, name), 'utf8').trim(); } catch { return null; } };
  const jobName = job => kind === 'launchd' ? `dev.sidevoice.${job}` : `sidevoice-${job}.service`;
  const node = {
    home, dataDir, env, modesFile, kind, socketPath: path.join(dataDir, 'connector.sock'),
    run, service: (...args) => run(['service', ...args]),
    calls: () => { try { return readFileSync(path.join(state, 'calls.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } },
    /** The pid the manager runs a job as. */
    pid: job => Number(read(jobName(job), 'pid')) || null,
    /** The manager, asked directly (`start`, `restart`… for systemd; launchd's verbs take the job's target). */
    manage: (...args) => spawnSync(manager, kind === 'systemd' ? ['--user', ...args] : args, { encoding: 'utf8' }),
    definition: job => kind === 'launchd' ? path.join(home, 'Library', 'LaunchAgents', `${jobName(job)}.plist`) : path.join(home, '.config', 'systemd', 'user', jobName(job)),
    said() { try { return readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } },
    ready() { try { return JSON.parse(readFileSync(path.join(dataDir, 'core', 'core.json'), 'utf8')); } catch { return null; } },
    status: (check, timeout = 20_000) => until(async () => { const now = await run(['service', 'status']); return check(now) ? now : null; }, timeout),
    stop() {
      for (const job of ['core', 'connector']) { try { process.kill(node.pid(job), 'SIGKILL'); } catch {} }
      try { process.kill(JSON.parse(readFileSync(path.join(dataDir, 'connector.lock'), 'utf8')).pid, 'SIGKILL'); } catch {}
      for (const line of node.said()) { try { process.kill(line.pid, 'SIGKILL'); } catch {} }
    },
  };
  if (install) {
    const installed = await run(['install', '--no-agents', '--service']);
    assert.equal(installed.ok, true, JSON.stringify(installed));
  }
  return node;
}

test('launcher: after a person\'s stop nothing is started — a conversation is told how to start Sidevoice again', async () => {
  const node = fakeNode();
  mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(path.join(node.dataDir, 'node-stopped.json'), JSON.stringify({ at: new Date().toISOString() }));
  const owner = facade({ ...node.env, SIDEVOICE_THREAD: 'thread-x', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' });
  try {
    await owner.ready;
    await assert.rejects(owner.call('voice_connect', {}), error => error.message === t('node.stopped'));
    assert.match(t('node.stopped'), /sidevoice service start/);
    await wait(500);
    assert.equal(existsSync(node.socketPath), false, 'no connector was started');
    assert.equal(node.said().length, 0, 'and no core');
  } finally { owner.child.kill(); node.stop(); }
});

for (const kind of ['launchd', 'systemd']) {
  test(`service (${kind}, stand-in manager): install --service → both jobs up with no harness → stop is a person's stop → start → restart → service uninstall with a client connected leaves no job, and the installation on demand`, async () => {
    const node = await managedNode(kind);
    let owner = null;
    try {
      const running = await node.status(s => s.state === 'running' && s.connector.running);
      assert.equal(running.service, kind);
      assert.equal(running.installed, true);
      assert.equal(running.core.pid, node.pid('core'), 'the core is the core job\'s');
      for (const job of ['core', 'connector']) assert.ok(existsSync(node.definition(job)), `${job}: defined`);
      assert.ok(readFileSync(node.definition('core'), 'utf8').includes(path.join(node.home, 'xdg', 'sidevoice', 'current', 'core', 'bin', 'sidevoice-core')), 'the core job runs R/current\'s core');
      assert.ok(readFileSync(node.definition('connector'), 'utf8').includes(path.join(node.home, 'xdg', 'sidevoice', 'current', 'dist', 'cli.mjs')), 'the connector job runs through R/current');
      assert.deepEqual(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')), { command: [process.execPath, path.join(node.home, 'xdg', 'sidevoice', 'current', 'dist', 'cli.mjs')],
        nodeExecutable: process.execPath, releases: path.join(node.home, 'xdg', 'sidevoice'), definitions: [node.definition('core'), node.definition('connector')] });
      // A façade is served by the connector job: the launcher spawned nothing beside it.
      owner = facade({ ...node.env, SIDEVOICE_THREAD: 'thread-s', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' });
      await owner.ready;
      assert.equal((await owner.call('voice_connect', {})).value.binding_id, 'core-thread-s');
      assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'connector.lock'), 'utf8')).pid, node.pid('connector'));

      // Stop: the person's. Both jobs gone; the launcher refuses; status says so without starting anything.
      const pids = [node.pid('core'), node.pid('connector')];
      const stopped = await node.service('stop');
      assert.equal(stopped.state, 'stopped-by-person', JSON.stringify(stopped));
      for (const pid of pids) assert.equal(alive(pid), false);
      const asked = await node.service('status');
      assert.equal(asked.state, 'stopped-by-person');
      assert.equal(existsSync(node.socketPath), false);
      // Start: the marker goes, both jobs run again, and the façade's conversation is registered again by itself.
      const started = await node.service('start');
      assert.equal(started.ok, true, JSON.stringify(started));
      const startedStatus = await node.status(s => s.state === 'running', 60_000);
      assert.equal(startedStatus.state, 'running', JSON.stringify({ started, startedStatus }));
      assert.equal(existsSync(path.join(node.dataDir, 'node-stopped.json')), false);
      await until(() => node.said().some(line => line.event === 'binding.register' && line.pid === node.pid('core') && line.data.client_ref === 'thread-s'), 20_000);
      // Restart (the person's «Reintentar»): the core job only, a new process.
      const before = node.pid('core'), connector = node.pid('connector');
      const restarted = await node.service('restart');
      assert.equal(restarted.state, 'running', JSON.stringify(restarted));
      assert.notEqual(node.pid('core'), before);
      assert.equal(node.pid('connector'), connector, 'the connector job is left as it is');

      // Uninstall with a façade still connected: no job, no process, no socket — and nothing restarts it.
      const last = [node.pid('core'), node.pid('connector')];
      const removed = await node.service('uninstall');
      assert.equal(removed.ok, true, JSON.stringify(removed));
      assert.equal(removed.state, 'not-installed');
      for (const job of ['core', 'connector']) assert.equal(existsSync(node.definition(job)), false);
      for (const pid of last) assert.equal(alive(pid), false);
      // The installation stays (`service uninstall` drops only the login service): the façade still connected gets
      // Sidevoice on demand, as on a machine with no service — a plain connector, never a job's.
      const back = await node.status(s => s.connector.running && s.reachable, 20_000);
      assert.equal(back.state, 'not-installed');
      assert.equal(node.pid('connector') && alive(node.pid('connector')), false);
      const verbs = node.calls().map(call => call.filter(arg => arg !== '--user')[1]);
      assert.ok(kind === 'launchd' ? verbs.includes('bootout') : verbs.includes('disable'), verbs.join(' '));
    } finally { owner?.child.kill(); node.stop(); }
  });
}

test('service (launchd, stand-in): an unload the manager refuses deletes nothing, and says so', async () => {
  const node = await managedNode('launchd');
  try {
    await node.status(s => s.state === 'running');
    const refused = await node.run(['service', 'uninstall'], { FAKE_MANAGER_FAIL: 'bootout' });
    assert.equal(refused.ok, false);
    assert.equal(refused.code, 1);
    assert.equal(refused.error.key, 'service.unload-failed');
    for (const job of ['core', 'connector']) assert.ok(existsSync(node.definition(job)), `${job}: the definition is still there`);
    assert.ok(alive(node.pid('core')) && alive(node.pid('connector')), 'and so is what they run');
    const full = await node.run(['uninstall'], { FAKE_MANAGER_FAIL: 'bootout' });
    assert.equal(full.ok, false);
    assert.ok(existsSync(path.join(node.home, 'xdg', 'sidevoice', 'current')), 'uninstall deleted nothing either');
    assert.equal((await node.service('uninstall')).ok, true, 'once the manager lets go, it goes');
  } finally { node.stop(); }
});

test('launcher: with a connector job defined that does not answer, nothing is spawned — the failure is the node\'s derived status', async () => {
  const node = await managedNode('systemd');
  let owner = null;
  try {
    await node.status(s => s.state === 'running');
    // The connector job dies and its manager does not bring it back (the stand-in models no Restart=).
    process.kill(node.pid('connector'), 'SIGKILL');
    await until(() => !existsSync(node.socketPath) || !alive(node.pid('connector')));
    owner = facade({ ...node.env, SIDEVOICE_THREAD: 'thread-y', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' });
    await owner.ready;
    await assert.rejects(owner.call('voice_connect', {}), /not answering \(running\).*sidevoice service/s);
    await wait(500);
    assert.equal(alive(JSON.parse(readFileSync(path.join(node.dataDir, 'connector.lock'), 'utf8')).pid), false, 'no connector beside the job');
  } finally { owner?.child.kill(); node.stop(); }
});

test('concurrent starts — three façades and two service starts at once — end with one connector: the job\'s; and with no manager, one plain connector', async () => {
  const node = await managedNode('systemd');
  const owners = [];
  try {
    await node.status(s => s.state === 'running');
    process.kill(node.pid('connector'), 'SIGKILL');
    await wait(300);
    owners.push(...[1, 2, 3].map(i => facade({ ...node.env, SIDEVOICE_SERVICE_START_WAIT_MS: '20000', SIDEVOICE_THREAD: 'thread-' + i, SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' })));
    await Promise.all(owners.map(owner => owner.ready));
    const results = await Promise.allSettled([...owners.map(owner => owner.call('voice_connect', {})), node.service('start'), node.service('start')]);
    for (const result of results) assert.equal(result.status, 'fulfilled', String(result.reason?.message));
    await wait(1500);
    const log = readFileSync(path.join(node.dataDir, 'connector.log'), 'utf8');
    const started = [...new Set([...log.matchAll(/connector \S+ starting.*?: pid (\d+)/g)].map(match => Number(match[1])))];
    assert.deepEqual(started.filter(alive), [node.pid('connector')], `one connector alive of ${started.length} started, the job's`);
    const status = await node.run(['service', 'status']);
    assert.equal(status.connector.running, true);
  } finally { for (const owner of owners) owner.child.kill(); node.stop(); }

  const plain = fakeNode();
  const facades = [1, 2, 3].map(i => facade({ ...plain.env, SIDEVOICE_THREAD: 'thread-' + i, SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' }));
  try {
    await Promise.all(facades.map(owner => owner.ready));
    const joined = await Promise.allSettled(facades.map(owner => owner.call('voice_connect', {})));
    for (const result of joined) assert.equal(result.status, 'fulfilled', String(result.reason?.message));
    await wait(1000);
    const log = readFileSync(path.join(plain.dataDir, 'connector.log'), 'utf8');
    const started = [...new Set([...log.matchAll(/connector \S+ starting.*?: pid (\d+)/g)].map(match => Number(match[1])))];
    assert.equal(started.filter(alive).length, 1, 'one plain connector serves them all');
    assert.equal(new Set(plain.said().filter(line => line.event === 'started').map(line => line.pid)).size, 1, 'and one core');
  } finally { for (const owner of facades) owner.child.kill(); plain.stop(); }
});

test('service definitions: the two LaunchAgents and the two user units, exactly', () => {
  const core = ['/Users/ana/.local/share/sidevoice/current/core/bin/sidevoice-core', '--data-dir', '/Users/ana/.sidevoice/core', '--idle-exit', '0'];
  const connector = ['/opt/node/bin/node', '/Users/ana/.local/share/sidevoice/current/dist/cli.mjs', 'connector', '--service'];
  const environment = { SIDEVOICE_DATA_DIR: '/Users/ana/.sidevoice', SIDEVOICE_SERVICE: 'launchd' };
  const corePlist = plistText({ label: 'dev.sidevoice.core', program: core, log: '/Users/ana/.sidevoice/core.stderr.log', environment, keepAlive: 'crashed' });
  assert.match(corePlist, /<key>Label<\/key>\s*<string>dev\.sidevoice\.core<\/string>/);
  assert.match(corePlist, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(corePlist, /<key>KeepAlive<\/key>\s*<dict>\s*<key>SuccessfulExit<\/key>\s*<false\/>\s*<key>Crashed<\/key>\s*<true\/>\s*<\/dict>/, 'restarted after a crash or a non-zero exit, never after exit 0');
  assert.match(corePlist, /<key>ThrottleInterval<\/key>\s*<integer>10<\/integer>/);
  assert.match(corePlist, /<key>StandardErrorPath<\/key>\s*<string>\/Users\/ana\/\.sidevoice\/core\.stderr\.log<\/string>/, 'the core writes core.log itself: the manager gets only what a crash prints');
  const connectorPlist = plistText({ label: 'dev.sidevoice.connector', program: connector, log: '/Users/ana/.sidevoice/connector.log', environment, keepAlive: true });
  assert.match(connectorPlist, /<key>Label<\/key>\s*<string>dev\.sidevoice\.connector<\/string>/);
  assert.match(connectorPlist, /<key>ProgramArguments<\/key>\s*<array>\s*<string>\/opt\/node\/bin\/node<\/string>\s*<string>\/Users\/ana\/\.local\/share\/sidevoice\/current\/dist\/cli\.mjs<\/string>\s*<string>connector<\/string>\s*<string>--service<\/string>\s*<\/array>/);
  assert.match(connectorPlist, /<key>KeepAlive<\/key>\s*<true\/>/);
  assert.match(connectorPlist, /<key>SIDEVOICE_SERVICE<\/key>\s*<string>launchd<\/string>/);
  assert.match(plistText({ label: 'l', program: ['/a & b/<x>'], log: '/l', environment: {} }), /<string>\/a &amp; b\/&lt;x&gt;<\/string>/, 'escaped as XML');

  const coreUnit = unitText({ description: 'core', program: ['/home/ana/.local/share/sidevoice/current/core/bin/sidevoice-core', '--data-dir', '/home/ana/sv 1/core'], environment: { SIDEVOICE_SERVICE: 'systemd', SIDEVOICE_X: '50%$HOME' }, restart: 'on-failure', restartSec: 10 }).split('\n');
  for (const line of ['StartLimitIntervalSec=600', 'StartLimitBurst=5', 'Restart=on-failure', 'RestartSec=10', 'TimeoutStopSec=20', 'KillMode=control-group', 'WantedBy=default.target',
    'ExecStart="/home/ana/.local/share/sidevoice/current/core/bin/sidevoice-core" "--data-dir" "/home/ana/sv 1/core"',
    'Environment="SIDEVOICE_SERVICE=systemd"', 'Environment="SIDEVOICE_X=50%%$HOME"']) assert.ok(coreUnit.includes(line), `${line} in the core unit`);
  assert.ok(coreUnit.indexOf('StartLimitBurst=5') < coreUnit.indexOf('[Service]'), 'the start limit is the unit\'s, in [Unit]');
  assert.ok(!coreUnit.some(line => line.startsWith('StandardOutput=') || line.startsWith('StandardError=')), 'output goes to the journal');
  const connectorUnit = unitText({ description: 'connector', program: ['/usr/bin/node', '/c/current/dist/cli.mjs', 'connector', '--service'], restart: 'always', restartSec: 2 }).split('\n');
  for (const line of ['Restart=always', 'RestartSec=2', 'StartLimitBurst=5']) assert.ok(connectorUnit.includes(line), `${line} in the connector unit`);
  // `$` is literal in Environment= and an expansion in ExecStart=: escaped only there, and read back as written.
  assert.ok(unitText({ description: 'd', program: ['/a/$b'], environment: {} }).includes('ExecStart="/a/$$b"'));
  // No value may start a directive of its own (a newline in a setting, a path, an argument).
  for (const bad of [{ environment: { SIDEVOICE_PUBLIC_URLS: 'x\nExecStartPre=/bin/echo INJECTED' } }, { log: '/l\nExecStartPre=/bin/x' }, { program: ['/a\rb'] }, { description: 'x\nExecStartPre=/bin/x' }]) {
    if (!bad.log) assert.throws(() => unitText({ description: 'd', program: ['/a'], environment: {}, ...bad }), error => error.key === 'service.unsafe-value');
    if (!bad.description) assert.throws(() => plistText({ label: 'l', program: ['/a'], log: '/l', environment: {}, ...bad }), error => error.key === 'service.unsafe-value');
  }
  // What a definition carries from this environment: Sidevoice's settings — never a credential of a core somebody else
  // runs, nor where the installer took its core from — and the paths the installation lives at, resolved.
  assert.deepEqual(serviceEnvironment('systemd', { HOME: '/home/ana', SIDEVOICE_DATA_DIR: '/d', SIDEVOICE_CONNECTOR_TOKEN: 'secret', SIDEVOICE_URL: 'http://x', SIDEVOICE_CORE_BIN: '/x', PATH: '/bin', SIDEVOICE_SERVICE: 'launchd', XDG_CONFIG_HOME: '/cfg' }),
    { SIDEVOICE_DATA_DIR: '/d', XDG_DATA_HOME: '/home/ana/.local/share', XDG_CONFIG_HOME: '/cfg', SIDEVOICE_SERVICE: 'systemd' });
});

test('a plain connector: the app\'s status probes neither keep it alive nor reset its idle timer; a façade that asks for work does', async () => {
  const node = fakeNode({ env: { SIDEVOICE_CONNECTOR_IDLE_MS: '600' } });
  try {
    const plain = node.start([]);
    await until(() => existsSync(node.socketPath));
    // The app's poll: a fresh connection every 100 ms asking only node.status, then status.
    let probing = true, answered = 0;
    const probe = (async () => { while (probing) { try { await node.ask(answered % 2 ? 'status' : 'node.status'); answered++; } catch {} await wait(100); } })();
    const started = Date.now();
    await until(() => plain.exitCode !== null, 8000);
    probing = false; await probe;
    assert.ok(answered >= 3, `probed ${answered} times while it ran`);
    assert.ok(Date.now() - started < 6000, 'it left on its own idle time, probes or not');
    const second = node.start([]);
    await until(() => existsSync(node.socketPath));
    // A façade that asked for real work (pair_device, here) keeps it, probes or not, for as long as it is connected.
    const { default: net } = await import('node:net');
    const held = net.createConnection(node.socketPath);
    await new Promise(resolve => held.once('connect', resolve));
    held.write(JSON.stringify({ id: 1, method: 'pair_device', params: {} }) + '\n');
    await wait(1500);
    assert.equal(second.exitCode, null, 'still there past its idle time');
    held.end();
    await until(() => second.exitCode !== null, 8000);
    const third = node.start([]);
    await until(() => existsSync(node.socketPath));
    const keeper = spawn(process.execPath, [cli, 'service', 'status', '--json'], { env: node.env, stdio: ['ignore', 'pipe', 'ignore'] });
    let out = ''; keeper.stdout.on('data', d => { out += d; });
    await new Promise(resolve => keeper.on('exit', resolve));
    const status = JSON.parse(out);
    assert.equal(status.state, 'absent', 'nothing installed: a plain connector does not make a service');
    assert.equal(status.connector.running, true);
    third.kill('SIGKILL');
  } finally { node.stop(); }
});

test('teardown: units whose manager cannot be reached are still what is installed — nothing is deleted, and it says service.unload-failed', { skip: process.platform !== 'linux' && 'the systemd case' }, async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-nobus-'));
  const dataDir = path.join(home, '.sidevoice');
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(path.join(dataDir, 'install.json'), JSON.stringify({ command: [process.execPath, cli] }), { mode: 0o600 });
  const units = ['sidevoice-core.service', 'sidevoice-connector.service'].map(name => path.join(home, '.config', 'systemd', 'user', name));
  mkdirSync(path.dirname(units[0]), { recursive: true });
  for (const unit of units) writeFileSync(unit, unitText({ description: 'd', program: [process.execPath, cli, 'connector', '--service'], environment: {} }));
  // No user bus: every systemctl --user call fails, as in a container or an SSH session without one.
  const env = { ...process.env, HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir, XDG_DATA_HOME: path.join(home, 'xdg'), SIDEVOICE_SYSTEMCTL: '/bin/false' };
  delete env.SIDEVOICE_SERVICE_MANAGER;
  const service = spawnSync(process.execPath, [cli, 'service', 'uninstall', '--json'], { env, encoding: 'utf8' });
  const answer = JSON.parse(service.stdout.trim().split('\n').at(-1));
  assert.equal(service.status, 1);
  assert.equal(answer.error.key, 'service.unload-failed');
  for (const unit of units) assert.ok(existsSync(unit), 'the unit is still there');
  const full = spawnSync(process.execPath, [cli, 'uninstall'], { env, encoding: 'utf8' });
  assert.equal(full.status, 1);
  assert.match(full.stderr, /nothing was deleted|untouched/);
  assert.ok(units.every(existsSync) && existsSync(path.join(dataDir, 'install.json')), 'uninstall stopped before deleting anything the units point at');
});

test('façade: the connector job restarted under it — a new voice turn reaches its conversation with no tool call in between; after a person\'s stop it starts nothing, and comes back with the service', async () => {
  const node = await managedNode('systemd');
  const received = [];
  const receiver = http.createServer(async (req, res) => { let body = ''; for await (const chunk of req) body += chunk; received.push(JSON.parse(body)); res.writeHead(200); res.end('{}'); });
  await new Promise(resolve => receiver.listen(0, '127.0.0.1', resolve));
  let owner = null;
  try {
    await node.status(s => s.state === 'running' && s.connector.running);
    owner = facade({ ...node.env, SIDEVOICE_SERVICE_START_WAIT_MS: '20000', SIDEVOICE_THREAD: 'thread-r', SIDEVOICE_DELIVERY_URL: `http://127.0.0.1:${receiver.address().port}/deliver` });
    await owner.ready;
    assert.equal((await owner.call('voice_connect', {})).value.binding_id, 'core-thread-r');
    /** Ready for a turn: the connector job holds this conversation again, linked to the core. */
    const registeredWith = pid => until(() => node.said().some(line => line.event === 'binding.register' && line.data.client_ref === 'thread-r') &&
      JSON.parse(readFileSync(path.join(node.dataDir, 'connector.lock'), 'utf8')).pid === pid, 20_000);
    // The manager restarts the connector job (a crash, an upgrade): the old one is gone at once.
    const first = node.pid('connector');
    process.kill(first, 'SIGKILL');
    await until(() => !alive(first));
    const registrations = node.said().filter(line => line.event === 'binding.register').length;
    node.manage('start', 'sidevoice-connector.service');
    await registeredWith(node.pid('connector'));
    await until(() => node.said().filter(line => line.event === 'binding.register').length > registrations, 20_000);
    deliver(node, { event_id: 'after-restart', binding_id: 'core-thread-r', channel: 'voice', session_id: 's', revision: 1, message_id: 'm-r', text: 'otra vez' });
    await until(() => received.length === 1, 20_000);
    assert.match(received[0].text, /otra vez/);
    assert.equal((await until(() => delivered(node, 'after-restart'))).answer.status, 'accepted');
    // A person's stop: both jobs go, and the façade's loop starts nothing in their place.
    assert.equal((await node.service('stop')).state, 'stopped-by-person');
    await wait(2000);
    assert.equal(existsSync(node.socketPath), false, 'nothing started while stopped');
    // The person starts it again: the conversation is registered again by itself.
    await node.service('start');
    deliver(node, { event_id: 'after-stop', binding_id: 'core-thread-r', channel: 'voice', session_id: 's', revision: 2, message_id: 'm-s', text: 'de vuelta' });
    await until(() => received.length === 2, 30_000);
  } finally { owner?.child.kill(); node.stop(); receiver.close(); }
});

test('--json: every failure is one object {ok:false, error:{key, message}} and exit 1; a core job refused before it starts says its own key', async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-json-'));
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), SIDEVOICE_SERVICE_MANAGER: 'none' };
  for (const [args, key] of [[['pair', '--json'], 'pair.usage'], [['pair', 'https://room.example', '--json'], 'pair.usage'], [['service', 'bogus', '--json'], 'service.usage'],
    [['service', '--json'], 'service.usage'], [['bogus', '--json'], 'command.unknown'], [['install', 'https://room.example', '--json'], 'install.usage'],
    [['rollback', '--json'], 'install.no-previous'], [['service', 'install', '--json'], 'service.no-installation']]) {
    const run = spawnSync(process.execPath, [cli, ...args], { env, encoding: 'utf8' });
    const lines = run.stdout.trim().split('\n');
    assert.equal(lines.length, 1, `${args.join(' ')}: exactly one line`);
    const answer = JSON.parse(lines[0]);
    assert.equal(run.status, 1, args.join(' '));
    assert.deepEqual([answer.ok, answer.error.key, typeof answer.error.message], [false, key, 'string'], args.join(' '));
  }
  // The core's directory open to others: the core refuses it before anything, says so in its report, exits 0 — and
  // the status is that, with its key, not a generic exit.
  const node = await managedNode('systemd');
  try {
    await node.status(s => s.state === 'running');
    chmodSync(path.join(node.dataDir, 'core'), 0o755);
    await node.service('restart');
    const status = await node.status(s => s.state === 'failed');
    assert.equal(status.failure.key, 'identity.unsafe-directory');
    assert.ok(Array.isArray(status.failure.log_tail));
  } finally { node.stop(); }
});

/* ----- deriveStatus(), row by row (§2.8), against what the stand-in managers print ----- */

/** What the stand-in manager prints for one job in a given state: `print` (launchd) or `show` (systemd). */
function printed(kind, { loaded = true, running = false, force = null } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-print-'));
  const job = kind === 'launchd' ? 'dev.sidevoice.core' : 'sidevoice-core.service';
  const state = path.join(dir, job);
  mkdirSync(state, { recursive: true });
  let sleeper = null;
  if (running) { sleeper = spawn('sleep', ['30'], { stdio: 'ignore' }); writeFileSync(path.join(state, 'pid'), String(sleeper.pid)); }
  if (force) writeFileSync(path.join(state, 'force.json'), JSON.stringify(force));
  let output;
  if (kind === 'launchd') {
    if (loaded) writeFileSync(path.join(state, 'loaded'), 'yes');
    const ran = spawnSync(process.execPath, [fakeManager, 'launchctl', 'print', `gui/${process.getuid()}/${job}`], { env: { ...process.env, FAKE_MANAGER_DIR: dir }, encoding: 'utf8' });
    output = parseLaunchd({ ok: ran.status === 0, output: ran.stdout + ran.stderr });
  } else {
    if (loaded) {
      writeFileSync(path.join(dir, 'unit'), ''); writeFileSync(path.join(state, 'definition'), path.join(dir, 'unit'));
      writeFileSync(path.join(state, 'loaded-spec.json'), JSON.stringify({ program: ['/bin/true'], environment: {}, log: '/dev/null' }));
    }
    const ran = spawnSync(process.execPath, [fakeManager, 'systemctl', '--user', 'show', '-p', 'x', job], { env: { ...process.env, FAKE_MANAGER_DIR: dir }, encoding: 'utf8' });
    output = parseSystemd(ran.stdout);
  }
  return { job: output, pid: sleeper?.pid ?? null, done: () => sleeper?.kill('SIGKILL') };
}

test('deriveStatus: rows 1–10 — health, nothing installed, no core job, a person\'s stop, the manager\'s failures, starting, hang, the core\'s report, backoff, an exit', () => {
  const health = { status: 200, body: { pid: 7, version: '0.1.0', api: 1, launch_id: 'l', calls: 2 } };
  const report = { key: 'import.missing-module', step: 'import', message: 'No module named soxr', at: '2026-10-01T00:00:00Z', log_tail: ['x'] };
  for (const kind of ['launchd', 'systemd']) {
    const base = { service: kind, installed: true, defined: { core: true, connector: true }, stopped: false, health: null, ready: null, failure: null, coreAge: null, program: null, logTail: ['tail'], connectorRunning: true };
    const with_ = (job, more = {}) => deriveStatus({ ...base, jobs: { core: job.job, connector: null }, ...more });
    const up = printed(kind, { running: true }), down = printed(kind), unloaded = printed(kind, { loaded: false });
    const restarting = printed(kind, { force: { restarting: true, runs: 3 } });
    try {
      // 1. The core answers: running, with its calls — unless the service condition says otherwise, which wins.
      const running = with_(up, { health });
      assert.deepEqual([running.state, running.reachable, running.calls, running.core.pid], ['running', true, 2, 7], kind);
      assert.equal(with_(up, { health, stopped: true }).state, 'stopped-by-person');
      assert.deepEqual([with_(unloaded, { health }).state, with_(unloaded, { health }).reachable], ['service-failed', true]);
      // 2. Nothing installed, nothing defined: absent — even with an on-demand core answering.
      assert.equal(deriveStatus({ ...base, installed: false, defined: { core: false, connector: false }, jobs: {}, health }).state, 'absent');
      // 3. Installed, no core job: not-installed.
      assert.equal(deriveStatus({ ...base, defined: { core: false, connector: false }, jobs: {} }).state, 'not-installed');
      // 4. A person's stop.
      assert.equal(with_(down, { stopped: true }).state, 'stopped-by-person');
      // 5. The manager does not have it, gave up, or its program is not there.
      assert.deepEqual([with_(unloaded).state, with_(unloaded).failure.key], ['service-failed', 'not-loaded']);
      assert.equal(with_(down, { program: 'executable-missing' }).failure.key, 'executable-missing');
      if (kind === 'systemd') { const limited = printed(kind, { force: { startLimit: true } }); assert.equal(with_(limited).failure.key, 'start-limit'); limited.done(); }
      // 6. Running, not ready yet: starting; for more than 60 s, ready.timeout.
      assert.equal(with_(up, { coreAge: 5 }).state, 'starting');
      assert.deepEqual([with_(up, { coreAge: 90 }).state, with_(up, { coreAge: 90 }).failure.key], ['failed', 'ready.timeout']);
      // 7. Running and ready (its own ready file), health silent: hang.
      assert.equal(with_(up, { ready: { pid: up.pid } }).failure.key, 'hang');
      assert.equal(with_(up, { ready: { pid: 1 } }).state, 'starting', 'a ready file of another process is not this one\'s');
      // 8. Not running, the core's report there: failed with it.
      assert.deepEqual(with_(down, { failure: report }).failure, report);
      // 9. Not running, the manager starts it again: backoff, the manager's count; a limit only where there is one.
      const backoff = with_(restarting);
      assert.deepEqual([backoff.state, backoff.attempts, backoff.limit, backoff.failure.key], ['backoff', 3, kind === 'systemd' ? 5 : null, 'launch.exited'], kind);
      // 10. Otherwise: failed, launch.exited.
      const exited = with_(down);
      assert.deepEqual([exited.state, exited.failure.key, exited.failure.log_tail], ['failed', 'launch.exited', ['tail']]);
      // The shape SEAMS §4 pins, whatever the row.
      for (const status of [running, backoff, exited]) {
        assert.deepEqual(Object.keys(status).sort(), ['attempts', 'calls', 'connector', 'core', 'failure', 'installed', 'limit', 'next_retry_at', 'ok', 'reachable', 'service', 'since', 'state', 'window_started']);
        assert.equal(status.calls === null, !status.reachable, 'calls unknown, not zero, when the core does not answer');
      }
    } finally { for (const item of [up, down, unloaded, restarting]) item.done(); }
  }
});

/* ----- review blockers (R1-b-rebuild-astra.md), one regression each ----- */

test('blocker 1: a manager that cannot be asked, or cannot confirm a job gone after its bootout, is not absence — uninstall says service.unload-failed and deletes nothing', async () => {
  const node = await managedNode('launchd');
  try {
    await node.status(s => s.state === 'running');
    const kept = () => ['core', 'connector'].every(job => existsSync(node.definition(job))) && existsSync(path.join(node.home, 'xdg', 'sidevoice', 'current'));
    // Unreachable throughout.
    const unreachable = await node.run(['uninstall'], { SIDEVOICE_LAUNCHCTL: '/bin/false' });
    assert.deepEqual([unreachable.ok, unreachable.error?.key], [false, 'service.unload-failed'], JSON.stringify(unreachable));
    assert.ok(kept(), 'definitions and releases are all there');
    assert.ok(alive(node.pid('core')) && alive(node.pid('connector')), 'and the jobs still run');
    // Reachable for the bootout, not for the probe after it: unconfirmed is not gone.
    const wrapper = path.join(node.home, 'launchctl-forgetful');
    const real = node.env.SIDEVOICE_LAUNCHCTL;
    writeFileSync(wrapper, `#!/bin/sh\nif [ "$1" = print ] && [ -e "${node.home}/booted" ]; then echo "launchctl: unavailable" >&2; exit 5; fi\n[ "$1" = bootout ] && touch "${node.home}/booted"\nexec "${real}" "$@"\n`, { mode: 0o755 });
    const unconfirmed = await node.run(['uninstall'], { SIDEVOICE_LAUNCHCTL: wrapper });
    assert.deepEqual([unconfirmed.ok, unconfirmed.error?.key], [false, 'service.unload-failed'], JSON.stringify(unconfirmed));
    assert.match(unconfirmed.error.message, /not confirmed/);
    assert.ok(kept(), 'still nothing deleted');
    // A manager that answers: it goes.
    assert.equal((await node.run(['uninstall'])).ok, true);
    assert.ok(!existsSync(node.definition('core')) && !existsSync(path.join(node.home, 'xdg', 'sidevoice')));
  } finally { node.stop(); }
});

test('blocker 2: uninstall acts where the installation recorded it is — the definitions and R in install.json — whatever this shell\'s XDG paths say', async () => {
  const node = await managedNode('systemd');
  try {
    await node.status(s => s.state === 'running');
    const record = JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8'));
    const R = path.join(node.home, 'xdg', 'sidevoice');
    assert.deepEqual(record, { command: [process.execPath, path.join(R, 'current', 'dist', 'cli.mjs')], nodeExecutable: process.execPath, releases: R,
      definitions: [node.definition('core'), node.definition('connector')] });
    const elsewhere = { XDG_CONFIG_HOME: path.join(node.home, 'other-config'), XDG_DATA_HOME: path.join(node.home, 'other-data') };
    // Its status, from that shell, is still this installation's.
    const seen = await node.run(['service', 'status'], elsewhere);
    assert.deepEqual([seen.state, seen.installed], ['running', true]);
    // The manager cannot confirm: refused, nothing deleted.
    const refused = await node.run(['uninstall'], { ...elsewhere, FAKE_MANAGER_FAIL: 'show', SIDEVOICE_SYSTEMCTL: '/bin/false' });
    assert.equal(refused.error?.key, 'service.unload-failed');
    assert.ok(existsSync(node.definition('core')) && existsSync(R));
    const pids = [node.pid('core'), node.pid('connector')];
    const removed = await node.run(['uninstall'], elsewhere);
    assert.equal(removed.ok, true, JSON.stringify(removed));
    for (const job of ['core', 'connector']) assert.equal(existsSync(node.definition(job)), false, `${job}: the recorded definition is gone`);
    assert.ok(pids.every(pid => !alive(pid)), 'both jobs stopped');
    assert.equal(existsSync(R), false, 'the recorded releases are gone');
  } finally { node.stop(); }
});
