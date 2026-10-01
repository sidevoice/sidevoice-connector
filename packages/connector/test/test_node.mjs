/** The node service around the connector (§4.2): the handover from a plain connector to the supervisor, the
 *  one launcher and its stop marker, and `sidevoice service` against stand-ins of launchd and systemd
 *  (`fake-service-manager.mjs`). The fake core plays the core throughout; `test_core.mjs` has its failure modes. */
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { supervisedNode } from './test_core.mjs';
import { runView } from './test_harness_cursor.mjs';
import { plistText, unitText, serviceEnvironment } from '../service.mjs';
import { t } from '../i18n.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(here, '..', 'cli.mjs');
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

test('handover: a supervisor takes over a plain connector — a new voice turn reaches a façade\'s conversation and an orphaned editor card, and the core never sees one leave', async () => {
  const node = supervisedNode();
  // The harness behind the façade-owned conversation: a receiver the http harness delivers to.
  const received = [];
  const receiver = http.createServer(async (req, res) => { let body = ''; for await (const chunk of req) body += chunk; received.push(JSON.parse(body)); res.writeHead(200); res.end('{}'); });
  await new Promise(resolve => receiver.listen(0, '127.0.0.1', resolve));
  const env = { ...node.env, SIDEVOICE_THREAD: 'thread-http', SIDEVOICE_DELIVERY_URL: `http://127.0.0.1:${receiver.address().port}/deliver` };
  const editorCaps = { extensions: { 'io.modelcontextprotocol/ui': { mimeTypes: ['text/html;profile=mcp-app'] } } };
  let owner = null, editor = null, view = null;
  try {
    // Before any service: the façade gets a plain connector through the launcher, which starts the core detached.
    owner = facade(env); await owner.ready;
    const joined = await owner.call('voice_connect', { title: 'HTTP' });
    assert.equal(joined.value.binding_id, 'core-thread-http');
    const plain = Number(readFileSync(node.socketPath + '.lock', 'utf8'));
    // An editor chat joins, and Cursor replaces its MCP process: its card keeps the conversation without a façade.
    const editorEnv = { ...node.env }; delete editorEnv.SIDEVOICE_THREAD; delete editorEnv.SIDEVOICE_DELIVERY_URL;
    editor = facade(editorEnv, { name: 'cursor-vscode', version: '1.0.0' }, editorCaps); await editor.ready;
    const html = (await editor.send('resources/read', { uri: 'ui://sidevoice/voice-link' })).result.contents[0].text;
    const card = await editor.call('voice_connect', { title: 'Editor' });
    view = runView({ html, toolResult: card.raw });
    // The card is polling the connector's bridge.
    await until(async () => (await node.ask('status')).bindings.find(b => b.client_ref === card.value.conversation)?.delivery_state?.card_connected);
    editor.child.stdin.end(); editor.child.kill();
    await wait(300);
    const cardBinding = 'core-' + card.value.conversation;
    // Both are reached before the takeover.
    deliver(node, { event_id: 'before-http', binding_id: 'core-thread-http', channel: 'voice', session_id: 's', revision: 1, message_id: 'm-1', text: 'antes' });
    deliver(node, { event_id: 'before-card', binding_id: cardBinding, channel: 'voice', session_id: 's', revision: 2, message_id: 'm-2', text: 'antes card' });
    await until(() => received.length === 1 && view.dispatched.length === 1);

    // The node service starts: its supervisor finds the plain connector, which hands over and leaves.
    const supervisor = node.start(['--supervise']);
    await until(() => !alive(plain), 20_000);
    const status = await node.status(s => s.supervisor && s.state === 'running', 20_000);
    assert.equal(status.attempts, 0, 'the core the plain connector started is adopted, not started again');
    assert.equal(existsSync(path.join(node.dataDir, 'handover.json')), false, 'restored, then deleted');
    // The façade re-registers with the supervisor, which only re-attaches its owner.
    await until(() => { try { return readFileSync(path.join(node.dataDir, 'mcp.log'), 'utf8').includes('"reattached"'); } catch { return false; } });
    await until(() => node.said().filter(line => line.event === 'handshake').length >= 2, 10_000);

    // A new voice turn, in each conversation, after the takeover.
    deliver(node, { event_id: 'after-http', binding_id: 'core-thread-http', channel: 'voice', session_id: 's', revision: 3, message_id: 'm-3', text: 'después' });
    deliver(node, { event_id: 'after-card', binding_id: cardBinding, channel: 'voice', session_id: 's', revision: 4, message_id: 'm-4', text: 'después card' });
    await until(() => received.length === 2 && view.dispatched.length === 2, 20_000);
    assert.match(received[1].text, /después/);
    assert.match(view.dispatched[1].content[0].text, /después card/);
    assert.equal(delivered(node, 'after-http').answer.status, 'accepted');
    // The façade still speaks for its conversation through the supervisor.
    const said = await owner.call('voice_say', { text: 'hola', session_id: 's', revision: 3 });
    assert.ok(['queued', 'published'].includes(said.value.status), said.value.status);
    // The core saw each binding come back with its own id and never saw one leave.
    assert.ok(!node.said().some(line => line.event === 'binding.unregister'), 'no unregister');
    const registered = node.said().filter(line => line.event === 'binding.register').map(line => line.data.binding_id);
    assert.ok(registered.filter(id => id === 'core-thread-http').length >= 1);
    assert.ok(registered.includes(cardBinding), 'the orphaned card\'s binding rejoined under its id');
    assert.match(node.log(), /restored from the handover/);
    void supervisor;
  } finally { view?.stop(); owner?.child.kill(); editor?.child.kill(); node.stop(); receiver.close(); }
});

test('launcher: after a person\'s stop nothing is started — a conversation is told how to start Sidevoice again', async () => {
  const node = supervisedNode();
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

/** A machine with an installed copy (`install.json` naming this checkout's CLI) and a fake service manager. */
function managedNode(kind, extra = {}) {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-svc-'));
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-mgr-'));
  const manager = path.join(tools, kind === 'launchd' ? 'launchctl' : 'systemctl');
  writeFileSync(manager, `#!/bin/sh\nexec "${process.execPath}" "${path.join(here, 'fake-service-manager.mjs')}" ${kind === 'launchd' ? 'launchctl' : 'systemctl'} "$@"\n`, { mode: 0o755 });
  const node = supervisedNode({ dataDir: path.join(home, '.sidevoice'), env: { HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_SERVICE_MANAGER: kind,
    SIDEVOICE_LAUNCHCTL: manager, SIDEVOICE_SYSTEMCTL: manager, SIDEVOICE_LOGINCTL: '/bin/false', FAKE_MANAGER_DIR: path.join(tools, 'state'), SIDEVOICE_TEARDOWN_MS: '5000', ...extra } });
  mkdirSync(path.join(home, '.sidevoice'), { recursive: true, mode: 0o700 });
  writeFileSync(path.join(home, '.sidevoice', 'install.json'), JSON.stringify({ connector: '0.6.0', core: '0.1.0', channel: 'release', build_seq: 0, runtime: 'uv', service: 'none', command: [process.execPath, cli] }), { mode: 0o600 });
  const calls = () => { try { return readFileSync(path.join(tools, 'state', 'calls.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
  const service = async (...args) => {
    const child = spawn(process.execPath, [cli, 'service', ...args, '--json'], { env: node.env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = ''; child.stdout.on('data', d => { out += d; });
    const code = await new Promise(resolve => child.on('exit', resolve));
    return { code, ...JSON.parse(out.trim().split('\n').at(-1)) };
  };
  return { ...node, home, calls, service, managerPid: () => Number(readFileSync(path.join(tools, 'state', 'pid'), 'utf8')) };
}

for (const kind of ['launchd', 'systemd']) {
  test(`service (${kind}, stand-in manager): install → running with no harness → stop is a person's stop → start → uninstall leaves no job, process or socket`, async () => {
    const node = managedNode(kind);
    try {
      const installed = await node.service('install');
      assert.equal(installed.ok, true, JSON.stringify(installed));
      assert.equal(installed.service, kind);
      if (kind === 'systemd') {
        assert.equal(installed.linger.command, `loginctl enable-linger ${process.env.USER || os.userInfo().username}`);
        assert.match(installed.linger.reason, /logged in/);
      }
      const running = await node.status(s => s.state === 'running' && s.supervisor, 20_000);
      assert.equal(running.service, kind);
      const definition = kind === 'launchd' ? path.join(node.home, 'Library', 'LaunchAgents', 'dev.sidevoice.node.plist') : path.join(node.home, '.config', 'systemd', 'user', 'sidevoice-node.service');
      assert.ok(readFileSync(definition, 'utf8').includes(cli), 'the definition runs install.json\'s command');
      assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).service, kind);
      // A façade connecting meanwhile is served by the supervisor: the launcher spawned nothing beside it.
      const owner = facade({ ...node.env, SIDEVOICE_THREAD: 'thread-s', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' });
      await owner.ready;
      assert.equal((await owner.call('voice_connect', {})).value.binding_id, 'core-thread-s');
      const supervisorPid = node.managerPid();
      assert.equal(Number(readFileSync(node.socketPath + '.lock', 'utf8')), supervisorPid);

      // Stop: the person's. Supervisor and core gone; the launcher refuses; status says so without starting anything.
      const corePid = running.core.pid;
      const stopped = await node.service('stop');
      assert.equal(stopped.state, 'stopped-by-person');
      assert.ok(!alive(supervisorPid) && !alive(corePid));
      await assert.rejects(owner.call('voice_status', { conversation: undefined }).then(() => owner.call('voice_connect', {})), error => error.message === t('node.stopped'));
      const asked = await node.service('status');
      assert.equal(asked.state, 'stopped-by-person');
      assert.equal(existsSync(node.socketPath), false);
      // Start: the marker goes, the service runs again, and the façade's conversation can join again.
      const started = await node.service('start');
      assert.equal(started.ok, true, JSON.stringify(started));
      await node.status(s => s.state === 'running' && s.supervisor, 20_000);
      assert.equal(existsSync(path.join(node.dataDir, 'node-stopped.json')), false);
      assert.equal((await owner.call('voice_connect', {})).value.binding_id, 'core-thread-s');

      // Uninstall with a façade still connected: no job, no process, no socket — and nothing restarts it.
      const pids = [node.managerPid(), (await node.ask('node.status')).core.pid];
      const removed = await node.service('uninstall');
      assert.equal(removed.ok, true, JSON.stringify(removed));
      assert.equal(removed.state, 'not-installed');
      assert.equal(existsSync(definition), false);
      for (const pid of pids) assert.equal(alive(pid), false);
      assert.equal(existsSync(node.socketPath), false);
      assert.equal(existsSync(path.join(node.dataDir, 'core', 'local.sock')), false);
      const after = await node.service('status');
      assert.equal(after.state, 'not-installed');
      owner.child.kill();
      const verbs = node.calls().map(call => call.filter(arg => arg !== '--user')[1]);
      assert.ok(kind === 'launchd' ? verbs.includes('bootout') : verbs.includes('disable'), verbs.join(' '));
    } finally { node.stop(); try { process.kill(node.managerPid(), 'SIGKILL'); } catch {} }
  });
}

test('service (launchd, stand-in): an unload the manager refuses deletes nothing, and says so', async () => {
  const node = managedNode('launchd');
  try {
    assert.equal((await node.service('install')).ok, true);
    await node.status(s => s.state === 'running' && s.supervisor, 20_000);
    const refused = await (async () => { node.env.FAKE_MANAGER_FAIL = 'bootout'; try { return await node.service('uninstall'); } finally { delete node.env.FAKE_MANAGER_FAIL; } })();
    assert.equal(refused.ok, false);
    assert.equal(refused.code, 1);
    assert.equal(refused.error.key, 'service.unload-failed');
    assert.ok(existsSync(path.join(node.home, 'Library', 'LaunchAgents', 'dev.sidevoice.node.plist')), 'the definition is still there');
    assert.ok(alive(node.managerPid()), 'and so is what it runs');
    assert.equal((await node.service('uninstall')).ok, true, 'once the manager lets go, it goes');
  } finally { node.stop(); try { process.kill(node.managerPid(), 'SIGKILL'); } catch {} }
});

test('launcher: with a registered service whose manager will not start it, nothing is spawned and the remedy is said', async () => {
  const node = managedNode('systemd');
  try {
    // Registered (the definition exists) but the manager refuses to start it.
    const unit = path.join(node.home, '.config', 'systemd', 'user', 'sidevoice-node.service');
    mkdirSync(path.dirname(unit), { recursive: true });
    writeFileSync(unit, unitText({ program: [process.execPath, cli, 'connector', '--supervise'], log: '/dev/null', environment: {} }));
    const owner = facade({ ...node.env, FAKE_MANAGER_FAIL: 'start', SIDEVOICE_THREAD: 'thread-y', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' });
    await owner.ready;
    await assert.rejects(owner.call('voice_connect', {}), /service manager did not start it.*sidevoice service start.*install --no-agents --service/s);
    await wait(500);
    assert.equal(existsSync(node.socketPath), false, 'no connector beside the service');
    owner.child.kill();
  } finally { node.stop(); }
});

test('concurrent starts — three façades, the app\'s service start and npx\'s, at once — end with one connector, the supervisor', async () => {
  const node = managedNode('none');
  try {
    const owners = [1, 2, 3].map(i => facade({ ...node.env, SIDEVOICE_THREAD: 'thread-' + i, SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/none' }));
    await Promise.all(owners.map(owner => owner.ready));
    const results = await Promise.allSettled([...owners.map(owner => owner.call('voice_connect', {})), node.service('start'), node.service('start')]);
    for (const result of results) assert.equal(result.status, 'fulfilled', String(result.reason?.message));
    const status = await node.status(s => s.supervisor && s.state === 'running', 20_000);
    // Every connector that ever started says so in its log; one is alive, and it is the supervisor.
    await wait(1500);
    const logs = ['connector.log', 'node-service.log'].map(name => { try { return readFileSync(path.join(node.dataDir, name), 'utf8'); } catch { return ''; } }).join('\n');
    const started = [...logs.matchAll(/connector \S+ starting.*?: pid (\d+)/g)].map(match => Number(match[1]));
    const living = [...new Set(started)].filter(alive);
    assert.deepEqual(living, [Number(readFileSync(node.socketPath + '.lock', 'utf8'))], `one connector alive of ${started.length} started`);
    assert.equal(status.supervisor, true);
    // The façades are all served by it: their three conversations are there.
    const bindings = await until(async () => { const list = (await node.ask('status')).bindings.map(binding => binding.client_ref).sort(); return list.length === 3 ? list : null; }, 20_000);
    assert.deepEqual(bindings, ['thread-1', 'thread-2', 'thread-3']);
    assert.ok(!node.said().some(line => line.event === 'binding.unregister'), 'none of them left on the way');
    for (const owner of owners) owner.child.kill();
  } finally { node.stop(); }
});

test('service definitions: the LaunchAgent and the user unit, exactly', () => {
  const program = ['/opt/node/bin/node', '/Users/ana/.local/share/sidevoice/0.6.0/dist/cli.mjs', 'connector', '--supervise'];
  const plist = plistText({ program, log: '/Users/ana/.sidevoice/node-service.log', environment: { SIDEVOICE_DATA_DIR: '/Users/ana/.sidevoice', SIDEVOICE_SERVICE: 'launchd' } });
  assert.match(plist, /<key>Label<\/key>\s*<string>dev\.sidevoice\.node<\/string>/);
  assert.match(plist, /<key>ProgramArguments<\/key>\s*<array>\s*<string>\/opt\/node\/bin\/node<\/string>\s*<string>\/Users\/ana\/\.local\/share\/sidevoice\/0\.6\.0\/dist\/cli\.mjs<\/string>\s*<string>connector<\/string>\s*<string>--supervise<\/string>\s*<\/array>/);
  assert.match(plist, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(plist, /<key>KeepAlive<\/key>\s*<true\/>/);
  assert.match(plist, /<key>ThrottleInterval<\/key>\s*<integer>10<\/integer>/);
  assert.match(plist, /<key>StandardOutPath<\/key>\s*<string>\/Users\/ana\/\.sidevoice\/node-service\.log<\/string>/);
  assert.match(plist, /<key>StandardErrorPath<\/key>\s*<string>\/Users\/ana\/\.sidevoice\/node-service\.log<\/string>/);
  assert.match(plist, /<key>SIDEVOICE_SERVICE<\/key>\s*<string>launchd<\/string>/);
  assert.match(plistText({ program: ['/a & b/<x>'], log: '/l', environment: {} }), /<string>\/a &amp; b\/&lt;x&gt;<\/string>/, 'escaped as XML');

  const unit = unitText({ program: ['/usr/bin/node', '/home/ana/sv 1/cli.mjs', 'connector', '--supervise'], log: '/home/ana/.sidevoice/node-service.log', environment: { SIDEVOICE_SERVICE: 'systemd', SIDEVOICE_X: '50%$HOME' } });
  const lines = unit.split('\n');
  for (const line of ['StartLimitIntervalSec=600', 'StartLimitBurst=5', 'Restart=on-failure', 'RestartSec=2', 'WantedBy=default.target',
    'ExecStart="/usr/bin/node" "/home/ana/sv 1/cli.mjs" "connector" "--supervise"',
    'StandardOutput=append:/home/ana/.sidevoice/node-service.log', 'StandardError=append:/home/ana/.sidevoice/node-service.log',
    'Environment="SIDEVOICE_SERVICE=systemd"', 'Environment="SIDEVOICE_X=50%%$$HOME"']) assert.ok(lines.includes(line), `${line} in the unit`);
  assert.ok(lines.indexOf('StartLimitBurst=5') < lines.indexOf('[Service]'), 'the start limit is the unit\'s, in [Unit]');

  // What the definition carries from this environment: Sidevoice's settings, never a credential of a core somebody else runs.
  assert.deepEqual(serviceEnvironment('systemd', { SIDEVOICE_DATA_DIR: '/d', SIDEVOICE_CONNECTOR_TOKEN: 'secret', SIDEVOICE_URL: 'http://x', PATH: '/bin', SIDEVOICE_SERVICE: 'launchd' }),
    { SIDEVOICE_DATA_DIR: '/d', SIDEVOICE_SERVICE: 'systemd' });
});
