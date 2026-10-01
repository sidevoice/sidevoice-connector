/** The node service around the connector (§4.2): the handover from a plain connector to the supervisor, the
 *  one launcher and its stop marker, and `sidevoice service` against stand-ins of launchd and systemd
 *  (`fake-service-manager.mjs`). The fake core plays the core throughout; `test_core.mjs` has its failure modes. */
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { appendFileSync, chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
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
  // A turn whose text says "slow" takes 1.5 s to be taken: it is still being delivered when the takeover starts.
  const receiver = http.createServer(async (req, res) => { let body = ''; for await (const chunk of req) body += chunk; const turn = JSON.parse(body); if (/slow/.test(turn.text)) await wait(1500); received.push(turn); res.writeHead(200); res.end('{}'); });
  await new Promise(resolve => receiver.listen(0, '127.0.0.1', resolve));
  const env = { ...node.env, SIDEVOICE_THREAD: 'thread-http', SIDEVOICE_DELIVERY_URL: `http://127.0.0.1:${receiver.address().port}/deliver` };
  const editorCaps = { extensions: { 'io.modelcontextprotocol/ui': { mimeTypes: ['text/html;profile=mcp-app'] } } };
  let owner = null, editor = null, view = null;
  try {
    // Before any service: the façade gets a plain connector through the launcher, which starts the core detached.
    owner = facade(env); await owner.ready;
    const joined = await owner.call('voice_connect', { title: 'HTTP' });
    assert.equal(joined.value.binding_id, 'core-thread-http');
    const plain = JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid;
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

    // A turn to the card that its chat will answer only after the takeover, and a slow turn still being taken
    // when the takeover begins.
    deliver(node, { event_id: 'crossing', binding_id: cardBinding, channel: 'voice', session_id: 'x', revision: 9, message_id: 'm-x', text: 'contesta luego' });
    await until(() => view.dispatched.length === 2);
    deliver(node, { event_id: 'slow-one', binding_id: 'core-thread-http', channel: 'voice', session_id: 's', revision: 2, message_id: 'm-slow', text: 'slow turn' });
    await until(() => readFileSync(path.join(node.dataDir, 'connector.log'), 'utf8').includes('m-slow') || received.length >= 2, 5000).catch(() => {});
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
    await until(() => received.some(turn => /después/.test(turn.text)) && view.dispatched.length === 3, 20_000);
    assert.ok(received.some(turn => turn.text === 'slow turn'), 'the slow turn was taken');
    assert.match(view.dispatched[2].content[0].text, /después card/);
    assert.equal((await until(() => delivered(node, 'after-http'))).answer.status, 'accepted');
    // The façade still speaks for its conversation through the supervisor.
    const said = await owner.call('voice_say', { text: 'hola', session_id: 's', revision: 3 });
    assert.ok(['queued', 'published'].includes(said.value.status), said.value.status);
    // The core saw each binding come back with its own id and never saw one leave.
    assert.ok(!node.said().some(line => line.event === 'binding.unregister'), 'no unregister');
    const registered = node.said().filter(line => line.event === 'binding.register').map(line => line.data.binding_id);
    assert.ok(registered.filter(id => id === 'core-thread-http').length >= 1);
    assert.ok(registered.includes(cardBinding), 'the orphaned card\'s binding rejoined under its id');
    assert.match(node.log(), /restored from the handover/);
    // The slow turn finished being delivered before the old connector let its bindings go: its answer reached the core.
    assert.equal((await until(() => delivered(node, 'slow-one'))).answer.status, 'accepted');
    // The crossing turn: delivered before the takeover, answered after it from a new editor process — routed by the
    // turn to the card's conversation, which the supervisor learned from the handover.
    const later = facade(editorEnv, { name: 'cursor-vscode', version: '1.0.0' }, editorCaps); await later.ready;
    try {
      const answer = await later.call('voice_say', { text: 'ya está', session_id: 'x', revision: 9 });
      assert.ok(['queued', 'published'].includes(answer.value.status), JSON.stringify(answer.value));
      await until(() => node.said().some(line => line.event === 'speech.publish' && line.data.binding_id === cardBinding && line.data.text === 'ya está'));
    } finally { later.child.kill(); }
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
      assert.equal(JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid, supervisorPid);

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
    assert.deepEqual(living, [JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid], `one connector alive of ${started.length} started`);
    assert.equal(status.supervisor, true);
    // The façades are all served by it: their three conversations are there.
    const bindings = await until(async () => { const list = (await node.ask('status')).bindings.map(binding => binding.client_ref).sort(); return list.length === 3 ? list : null; }, 20_000);
    assert.deepEqual(bindings, ['thread-1', 'thread-2', 'thread-3']);
    assert.ok(!node.said().some(line => line.event === 'binding.unregister'), 'none of them left on the way');
    for (const owner of owners) owner.child.kill();
  } finally {
    // A supervisor still waiting to take over takes the lock once the first is killed: cleaned up across that window.
    node.stop(); await wait(2500); node.stop();
  }
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
  // `$` is literal in Environment= and an expansion in ExecStart=: escaped only there, and read back as written.
  assert.ok(unitText({ program: ['/a/$b'], log: '/l', environment: {} }).includes('ExecStart="/a/$$b"'));
  // No value may start a directive of its own (a newline in a setting, a path, an argument).
  for (const bad of [{ environment: { SIDEVOICE_PUBLIC_URLS: 'x\nExecStartPre=/bin/echo INJECTED' } }, { log: '/l\nExecStartPre=/bin/x' }, { program: ['/a\rb'] }]) {
    assert.throws(() => unitText({ program: ['/a'], log: '/l', environment: {}, ...bad }), error => error.key === 'service.unsafe-value');
    assert.throws(() => plistText({ program: ['/a'], log: '/l', environment: {}, ...bad }), error => error.key === 'service.unsafe-value');
  }
  const lines = unit.split('\n');
  for (const line of ['StartLimitIntervalSec=600', 'StartLimitBurst=5', 'Restart=on-failure', 'RestartSec=2', 'WantedBy=default.target',
    'ExecStart="/usr/bin/node" "/home/ana/sv 1/cli.mjs" "connector" "--supervise"',
    'StandardOutput=append:/home/ana/.sidevoice/node-service.log', 'StandardError=append:/home/ana/.sidevoice/node-service.log',
    'Environment="SIDEVOICE_SERVICE=systemd"', 'Environment="SIDEVOICE_X=50%%$HOME"']) assert.ok(lines.includes(line), `${line} in the unit`);
  assert.ok(lines.indexOf('StartLimitBurst=5') < lines.indexOf('[Service]'), 'the start limit is the unit\'s, in [Unit]');

  // What the definition carries from this environment: Sidevoice's settings, never a credential of a core somebody else runs.
  assert.deepEqual(serviceEnvironment('systemd', { SIDEVOICE_DATA_DIR: '/d', SIDEVOICE_CONNECTOR_TOKEN: 'secret', SIDEVOICE_URL: 'http://x', PATH: '/bin', SIDEVOICE_SERVICE: 'launchd' }),
    { SIDEVOICE_DATA_DIR: '/d', SIDEVOICE_SERVICE: 'systemd' });
});

test('a plain connector: the app\'s status probes neither keep it alive nor reset its idle timer; a façade that asks for work does', async () => {
  const node = supervisedNode({ env: { SIDEVOICE_CONNECTOR_IDLE_MS: '600' } });
  try {
    const plain = node.start([]);
    await until(() => existsSync(node.socketPath));
    // The app's poll: a fresh connection every 100 ms asking only node.status, then status.
    let probing = true, answered = 0;
    const probe = (async () => { while (probing) { try { await node.ask(answered % 2 ? 'status' : 'node.status'); answered++; } catch {} await wait(100); } })();
    const started = Date.now();
    await until(() => plain.exitCode !== null, 5000);
    probing = false; await probe;
    assert.ok(answered >= 3, `probed ${answered} times while it ran`);
    assert.ok(Date.now() - started < 3000, 'it left on its own idle time, probes or not');
    // The plain connector's status for the app: its core, and the service state from the files (here: no service).
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
    await until(() => second.exitCode !== null, 5000);
    const third = node.start([]);
    await until(() => existsSync(node.socketPath));
    const keeper = spawn(process.execPath, [cli, 'service', 'status', '--json'], { env: node.env, stdio: ['ignore', 'pipe', 'ignore'] });
    let out = ''; keeper.stdout.on('data', d => { out += d; });
    await new Promise(resolve => keeper.on('exit', resolve));
    const status = JSON.parse(out);
    assert.equal(status.state, 'absent', 'nothing installed: a plain connector does not make a service');
    assert.equal(status.supervisor, undefined);
    third.kill('SIGKILL');
  } finally { node.stop(); }
});

test('teardown: a unit whose manager cannot be reached is still what is installed — nothing is deleted, and it says service.unload-failed', { skip: process.platform !== 'linux' && 'the systemd case' }, async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-nobus-'));
  const dataDir = path.join(home, '.sidevoice');
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(path.join(dataDir, 'install.json'), JSON.stringify({ id: '0.6.0', connector: '0.6.0', command: [process.execPath, cli] }), { mode: 0o600 });
  const unit = path.join(home, '.config', 'systemd', 'user', 'sidevoice-node.service');
  mkdirSync(path.dirname(unit), { recursive: true });
  writeFileSync(unit, unitText({ program: [process.execPath, cli, 'connector', '--supervise'], log: path.join(dataDir, 'node-service.log'), environment: {} }));
  // No user bus: every systemctl --user call fails, as in a container or an SSH session without one.
  const env = { ...process.env, HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir, XDG_DATA_HOME: path.join(home, 'xdg'), SIDEVOICE_SYSTEMCTL: '/bin/false' };
  delete env.SIDEVOICE_SERVICE_MANAGER;
  const service = spawnSync(process.execPath, [cli, 'service', 'uninstall', '--json'], { env, encoding: 'utf8' });
  const answer = JSON.parse(service.stdout.trim().split('\n').at(-1));
  assert.equal(service.status, 1);
  assert.equal(answer.error.key, 'service.unload-failed');
  assert.ok(existsSync(unit), 'the unit is still there');
  const full = spawnSync(process.execPath, [cli, 'uninstall'], { env, encoding: 'utf8' });
  assert.equal(full.status, 1);
  assert.match(full.stderr, /nothing was deleted|untouched/);
  assert.ok(existsSync(unit) && existsSync(path.join(dataDir, 'install.json')), 'uninstall stopped before deleting anything the unit points at');
});

test('façade: the supervisor replaced under it — a new voice turn reaches its conversation with no tool call in between; after a person\'s stop it starts nothing and comes back with the service', async () => {
  const node = supervisedNode();
  const received = [];
  const receiver = http.createServer(async (req, res) => { let body = ''; for await (const chunk of req) body += chunk; received.push(JSON.parse(body)); res.writeHead(200); res.end('{}'); });
  await new Promise(resolve => receiver.listen(0, '127.0.0.1', resolve));
  let owner = null;
  try {
    const first = node.start();
    await node.status(s => s.state === 'running');
    owner = facade({ ...node.env, SIDEVOICE_THREAD: 'thread-r', SIDEVOICE_DELIVERY_URL: `http://127.0.0.1:${receiver.address().port}/deliver` });
    await owner.ready;
    assert.equal((await owner.call('voice_connect', {})).value.binding_id, 'core-thread-r');
    const registrations = () => node.said().filter(line => line.event === 'binding.register' && line.data.client_ref === 'thread-r').length;
    // The service manager replaces the supervisor (an upgrade, a crash): the old one is gone at once.
    first.kill('SIGKILL');
    await until(() => first.signalCode !== null);
    node.start();
    await until(() => registrations() >= 2, 20_000);
    deliver(node, { event_id: 'after-restart', binding_id: 'core-thread-r', channel: 'voice', session_id: 's', revision: 1, message_id: 'm-r', text: 'otra vez' });
    await until(() => received.length === 1, 20_000);
    assert.match(received[0].text, /otra vez/);
    assert.equal((await until(() => delivered(node, 'after-restart'))).answer.status, 'accepted');
    // A person's stop: the supervisor goes, and the façade's loop starts nothing in its place.
    writeFileSync(path.join(node.dataDir, 'node-stopped.json'), JSON.stringify({ at: new Date().toISOString() }), { mode: 0o600 });
    const second = node.children.at(-1);
    second.kill('SIGTERM');
    await until(() => second.exitCode !== null);
    await wait(1500);
    assert.equal(existsSync(node.socketPath), false, 'nothing started while stopped');
    // The person starts it again: the conversation is registered again by itself.
    const before = registrations();
    node.start();
    await until(() => registrations() > before, 20_000);
    deliver(node, { event_id: 'after-stop', binding_id: 'core-thread-r', channel: 'voice', session_id: 's', revision: 2, message_id: 'm-s', text: 'de vuelta' });
    await until(() => received.length === 2, 20_000);
  } finally { owner?.child.kill(); node.stop(); receiver.close(); }
});

test('--json: every failure is one object {ok:false, error:{key, message}} and exit 1; a refusal before the core starts keeps its key in node.status', async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-json-'));
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), SIDEVOICE_SERVICE_MANAGER: 'none' };
  for (const [args, key] of [[['pair', '--json'], 'pair.usage'], [['pair', 'https://room.example', '--json'], 'pair.usage'], [['service', 'bogus', '--json'], 'service.usage'],
    [['service', '--json'], 'service.usage'], [['bogus', '--json'], 'command.unknown'], [['install', 'https://room.example', '--json'], 'install.usage']]) {
    const run = spawnSync(process.execPath, [cli, ...args], { env, encoding: 'utf8' });
    const lines = run.stdout.trim().split('\n');
    assert.equal(lines.length, 1, `${args.join(' ')}: exactly one line`);
    const answer = JSON.parse(lines[0]);
    assert.equal(run.status, 1, args.join(' '));
    assert.deepEqual([answer.ok, answer.error.key, typeof answer.error.message], [false, key, 'string'], args.join(' '));
  }
  // The core's directory open to others: refused before any core starts, and said with its own key.
  const node = supervisedNode();
  mkdirSync(path.join(node.dataDir, 'core'), { recursive: true }); chmodSync(path.join(node.dataDir, 'core'), 0o755);
  try {
    node.start();
    const status = await node.status(s => s.failure, 15_000);
    assert.equal(status.failure.key, 'identity.unsafe-directory');
  } finally { node.stop(); }
});
