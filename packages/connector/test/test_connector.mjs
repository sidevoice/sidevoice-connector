import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createWsServer } from './ws-server.mjs';
import { envelope } from '../harness-contract.mjs';
import { declaredHarness, interpret, interpretTurnEnd, interpretWorking, nudge, voiceEnvelope } from '../hook.mjs';
import { sessionWorking } from '../harness-claude.mjs';
import { install as installSkill, remove as removeSkill, status as skillStatus } from '../skill.mjs';
import './test_harness_contract.mjs';
import './test_harness_claude.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const hookPath = path.join(here, '..', 'cli.mjs');
const connectorPath = path.join(here, '..', 'connector.mjs');
const mcpPath = path.join(here, '..', 'mcp.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 5000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(25); } throw new Error('timed out waiting'); }

function ipcClient(socketPath) {
  const socket = net.createConnection(socketPath); let buffer = ''; let serial = 0; const waiting = new Map();
  socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const reply = JSON.parse(line); const w = waiting.get(reply.id); waiting.delete(reply.id); reply.ok ? w.resolve(reply.result) : w.reject(new Error(reply.error)); } });
  return { socket, ready: new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); }),
    call: (method, params) => new Promise((resolve, reject) => { const id = ++serial; waiting.set(id, { resolve, reject }); socket.write(JSON.stringify({ id, method, params }) + '\n'); }), end: () => socket.end() };
}

async function startRoom() {
  const frames = []; let conn = null; const server = createWsServer(c => { conn = c; c.onMessage = text => { const frame = JSON.parse(text); frames.push(frame); room.handle?.(frame, c); }; });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const room = { server, frames, get conn() { return conn; }, url: `ws://127.0.0.1:${server.address().port}/api/connectors/ws`, handle: null, close: () => new Promise(r => server.close(r)) };
  return room;
}

function startConnector(room, dataDir, extraEnv = {}) {
  const socketPath = path.join(dataDir, 'connector.sock');
  writeFileSync(path.join(dataDir, 'credentials.json'), JSON.stringify({ url: room.url, connector_id: 'c-1', token: 't-1' }));
  const child = spawn(process.execPath, [connectorPath], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CONNECTOR_IDLE_MS: '400', ...extraEnv }, stdio: ['ignore', 'pipe', 'pipe'] });
  let stderr = ''; child.stderr.on('data', d => { stderr += d; });
  return { child, socketPath, stderr: () => stderr };
}

test('envelope carries the header the skill expects', () => {
  const text = envelope({ channel: 'voice', session_id: 's', revision: 2, message_id: 'm', text: 'hola' });
  const [header, body] = text.split('\n\n');
  assert.deepEqual(JSON.parse(header), { channel: 'voice', session_id: 's', revision: 2, message_id: 'm' });
  assert.equal(body, 'hola');
});

test('connector: hello, register, ordered delivery with acks, speech round trip, façade death unregisters', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  // A local http receiver stands in for the harness.
  const received = []; let release = null;
  const harness = http.createServer(async (req, res) => { let body = ''; for await (const c of req) body += c; received.push(JSON.parse(body)); await new Promise(r => { release = r; }); res.writeHead(200); res.end('{}'); });
  await new Promise(r => harness.listen(0, '127.0.0.1', r));
  const deliveryUrl = `http://127.0.0.1:${harness.address().port}/presentation/message`;
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') { assert.equal(frame.connector_id, 'c-1'); assert.equal(frame.token, 't-1'); c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1, heartbeat_seconds: 15 })); }
    if (frame.type === 'binding.register') {
      // First registration carries no server binding id; reconnects do.
      assert.ok(frame.binding_id === undefined || frame.binding_id === 'b-' + frame.client_ref);
      c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-' + frame.client_ref, thread: frame.thread }));
    }
    if (frame.type === 'heartbeat') c.send(JSON.stringify({ type: 'heartbeat.ack', nonce: frame.nonce }));
    if (frame.type === 'speech.publish') c.send(JSON.stringify({ type: 'speech.published', event_id: frame.event_id, status: 'queued', text_saved: true, utterance_id: frame.utterance_id }));
  };
  const { child, socketPath, stderr } = startConnector(room, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const registered = await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: deliveryUrl, thread: 'thread-1' } });
    assert.equal(registered.binding_id, 'b-thread-1');
    assert.equal(room.frames.filter(f => f.type === 'binding.register').length, 1);
    // Two deliveries: the second must wait for the first to be acknowledged by the harness.
    room.conn.send(JSON.stringify({ type: 'input.deliver', event_id: 'e1', binding_id: 'b-thread-1', thread: 'thread-1', text: 'uno', channel: 'voice', session_id: 's', revision: 1, message_id: 'm1' }));
    room.conn.send(JSON.stringify({ type: 'input.deliver', event_id: 'e2', binding_id: 'b-thread-1', thread: 'thread-1', text: 'dos', channel: 'voice', session_id: 's', revision: 1, message_id: 'm2' }));
    await until(() => received.length === 1); await wait(100);
    assert.equal(received.length, 1); assert.equal(received[0].text, 'uno');
    release();
    await until(() => room.frames.some(f => f.type === 'input.ack' && f.event_id === 'e1' && f.status === 'accepted'));
    await until(() => received.length === 2); assert.equal(received[1].text, 'dos'); release();
    await until(() => room.frames.some(f => f.type === 'input.ack' && f.event_id === 'e2'));
    // Unknown binding is acknowledged as such, never silently dropped.
    room.conn.send(JSON.stringify({ type: 'input.deliver', event_id: 'e3', binding_id: 'nope', text: 'x' }));
    await until(() => room.frames.some(f => f.type === 'input.ack' && f.event_id === 'e3' && f.status === 'unknown_binding'));
    // Speech goes out and comes back confirmed; the durable outbox is empty afterwards.
    const said = await facade.call('publish', { binding_id: 'b-thread-1', session_id: 's', revision: 1, text: 'hola', language: 'es' });
    assert.equal(said.status, 'queued'); assert.equal(said.text_saved, true);
    assert.deepEqual(JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')), []);
    assert.equal(room.frames.find(f => f.type === 'speech.publish').final, undefined);
    // Heartbeat from the room is answered.
    room.conn.send(JSON.stringify({ type: 'heartbeat', nonce: 'n1' }));
    await until(() => room.frames.some(f => f.type === 'heartbeat.ack' && f.nonce === 'n1'));
    // The façade dies: its binding is unregistered and the connector exits once idle.
    facade.end();
    await until(() => room.frames.some(f => f.type === 'binding.unregister' && f.binding_id === 'b-thread-1'));
    const code = await until(() => child.exitCode !== null ? child.exitCode + 1 : null, 5000);
    assert.equal(code - 1, 0, stderr());
  } finally { if (child.exitCode === null) child.kill(); await room.close(); harness.close(); }
});

test('connector: speech while offline is queued durably and replayed on reconnect', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const port = room.server.address().port;
  // Make the first connector attempts genuinely refused. Leaving a WebSocket
  // peer open without a welcome races the client's connection-error handling.
  await room.close();
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1, heartbeat_seconds: 15 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-1', thread: frame.thread }));
    if (frame.type === 'speech.publish') c.send(JSON.stringify({ type: 'speech.published', event_id: frame.event_id, status: 'queued', text_saved: true }));
  };
  const { child, socketPath } = startConnector(room, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const registration = facade.call('register', { client_ref: 'r', harness: 'test', thread: 'thread-1', delivery: { kind: 'http', url: 'http://127.0.0.1:1/', thread: 'thread-1' } });
    const said = await facade.call('publish', { client_ref: 'r', session_id: 's', revision: 0, text: 'sin sala' });
    assert.equal(said.status, 'queued');
    assert.equal(JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')).length, 1);
    await new Promise(r => room.server.listen(port, '127.0.0.1', r));
    await until(() => room.frames.some(f => f.type === 'speech.publish' && f.text === 'sin sala'));
    await until(() => JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')).length === 0);
    const result = await registration; assert.equal(result.binding_id, 'b-1');
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: keeps retrying while the room is down and connects once it appears', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  // Reserve a port, then free it so the connector's first attempts are refused.
  const probe = http.createServer(); await new Promise(r => probe.listen(0, '127.0.0.1', r)); const port = probe.address().port; await new Promise(r => probe.close(r));
  const frames = []; let conn = null;
  const server = createWsServer(c => { conn = c; c.onMessage = text => { const f = JSON.parse(text); frames.push(f); if (f.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 })); }; });
  const socketPath = path.join(dataDir, 'connector.sock');
  writeFileSync(path.join(dataDir, 'credentials.json'), JSON.stringify({ url: `ws://127.0.0.1:${port}/api/connectors/ws`, connector_id: 'c-1', token: 't-1' }));
  const child = spawn(process.execPath, [connectorPath], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CONNECTOR_IDLE_MS: '20000' }, stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    await until(() => existsSync(socketPath));
    await wait(1500); // several refused attempts happen in here
    await new Promise(r => server.listen(port, '127.0.0.1', r));
    await until(() => frames.some(f => f.type === 'connector.hello'), 12000);
    const facade = ipcClient(socketPath); await facade.ready;
    await until(async () => (await facade.call('status', {})).connected, 5000);
    facade.end();
  } finally { child.kill(); await new Promise(r => server.close(r)); }
});

test('connector: a second instance defers to the live one', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (frame, c) => { if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 })); };
  const first = startConnector(room, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '5000' });
  try {
    await until(() => existsSync(first.socketPath));
    const second = startConnector(room, dataDir);
    const code = await until(() => second.child.exitCode !== null ? second.child.exitCode + 1 : null);
    assert.equal(code - 1, 0);
    assert.equal(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8'), String(first.child.pid));
    const facade = ipcClient(first.socketPath); await facade.ready; assert.equal((await facade.call('status', {})).host.length > 0, true); facade.end();
  } finally { if (first.child.exitCode === null) first.child.kill(); await room.close(); }
});

test('mcp façade: identity comes from the harness, tools are exposed, instructions travel in initialize', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const socketPath = path.join(dataDir, 'connector.sock');
  const commands = [];
  const fake = net.createServer(socket => { let buffer = ''; socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const input = JSON.parse(line); commands.push(input); const result = input.method === 'register' ? { binding_id: 'b-9', thread: input.params.thread, connected: true } : input.method === 'publish' ? { status: 'queued', text_saved: true } : { connected: true, bindings: [] }; socket.write(JSON.stringify({ id: input.id, ok: true, result }) + '\n'); } }); });
  await new Promise(r => fake.listen(socketPath, r));
  writeFileSync(path.join(dataDir, 'credentials.json'), JSON.stringify({ url: 'wss://room.example/api/connectors/ws', connector_id: 'c-1', token: 't-1' }));
  const child = spawn(process.execPath, [mcpPath], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CLAUDE_CODE_SESSION_ID: 'sess-abc', CLAUDE_CODE_MESSAGING_SOCKET: '/tmp/x.sock', CLAUDE_CODE_MESSAGING_TOKEN: 'tok' }, stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = []; let out = ''; child.stdout.on('data', d => { out += d; let i; while ((i = out.indexOf('\n')) >= 0) { replies.push(JSON.parse(out.slice(0, i))); out = out.slice(i + 1); } });
  const ask = (id, method, params) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  try {
    ask(1, 'initialize', { protocolVersion: '2025-06-18' }); ask(2, 'tools/list', {}); ask(3, 'tools/call', { name: 'voice_connect', arguments: { title: 'Prueba' } });
    await until(() => replies.length === 3);
    assert.match(replies[0].result.instructions, /voice_say/);
    assert.match(replies[0].result.instructions, /immediate acknowledgement/);
    assert.match(replies[0].result.instructions, /meaningful progress checkpoints/);
    assert.match(replies[0].result.instructions, /process newly arrived user input before starting the next step/);
    assert.match(replies[0].result.instructions, /same original session_id and revision/);
    assert.deepEqual(replies[1].result.tools.map(t => t.name), ['voice_connect', 'voice_pair', 'voice_say', 'voice_disconnect', 'voice_status']);
    assert.match(replies[0].result.instructions, /Never try to obtain a code from the room yourself/);
    const joined = JSON.parse(replies[2].result.content[0].text);
    assert.equal(joined.status, 'joined'); assert.equal(joined.harness, 'claude'); assert.equal(joined.conversation, 'sess-abc');
    assert.deepEqual(commands[0].params.delivery, { kind: 'claude-uds', socket: '/tmp/x.sock', token: 'tok' });
    assert.deepEqual(commands[0].params.capabilities, {
      deliver: 'supported', inspectInbound: 'supported', working: 'supported', endOfTurn: 'supported', sessionIdentity: 'supported',
    });
    ask(4, 'tools/call', { name: 'voice_say', arguments: { text: 'hola', session_id: 's', revision: 1 } });
    await until(() => replies.length === 4);
    assert.equal(JSON.parse(replies[3].result.content[0].text).status, 'published');
    assert.equal(commands[1].params.binding_id, 'b-9');
  } finally { child.kill(); fake.close(); }
});

test('connector: the room closing a conversation\'s voice removes the binding and the façade is told on its next call', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1, heartbeat_seconds: 15 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-' + frame.client_ref, thread: frame.thread }));
    if (frame.type === 'speech.publish') c.send(JSON.stringify({ type: 'speech.published', event_id: frame.event_id, status: 'queued', text_saved: true, utterance_id: frame.utterance_id }));
  };
  const { child, socketPath } = startConnector(room, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });
    room.conn.send(JSON.stringify({ type: 'binding.close', binding_id: 'b-thread-1', thread: 'thread-1', reason: 'closed_from_room' }));
    await until(async () => (await facade.call('status', {})).closed_by_room.includes('thread-1'));
    await assert.rejects(facade.call('publish', { binding_id: 'b-thread-1', client_ref: 'thread-1', session_id: 's', revision: 1, text: 'tarde' }), /CLOSED_BY_ROOM/);
    assert.equal((await facade.call('status', {})).bindings.length, 0);
    // The room never hears an unregister for a binding it closed itself.
    assert.equal(room.frames.filter(f => f.type === 'binding.unregister').length, 0);
    // Joining again is the user's explicit request and clears the closure.
    const again = await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });
    assert.equal(again.binding_id, 'b-thread-1');
    assert.deepEqual((await facade.call('status', {})).closed_by_room, []);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('hook: only a Sidevoice voice message being admitted counts, and it names the conversation the harness names', () => {
  const prompt = envelope({ channel: 'voice', session_id: 's-9', revision: 4, message_id: 'm-9', text: 'hola desde la sala' });
  assert.deepEqual(voiceEnvelope(prompt), { message_id: 'm-9', session_id: 's-9', revision: 4, text: 'hola desde la sala' });
  assert.equal(voiceEnvelope('just a prompt'), null);
  assert.equal(voiceEnvelope('{"channel":"room-control","message_id":"x","session_id":"s","revision":1}\n\nhi'), null);
  const claude = interpret({ hook_event_name: 'UserPromptSubmit', prompt }, { CLAUDE_CODE_SESSION_ID: 'claude-session' });
  assert.equal(claude.thread, 'claude-session'); assert.equal(claude.message_id, 'm-9');
  const codex = interpret({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', turn_id: 't-1', prompt }, {});
  assert.equal(codex.thread, 'codex-thread'); assert.equal(codex.turn_id, 't-1');
  assert.equal(interpretTurnEnd({ hook_event_name: 'Stop', session_id: 'codex-thread', turn_id: 't-1' }, {}), null,
    'event-backed Codex stops use the correlated working path, not the legacy uncorrelated end path');
  assert.deepEqual(interpretWorking({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', turn_id: 't-1', prompt }, {}),
    { thread: 'codex-thread', turn_id: 't-1', working: true, session_id: 's-9', revision: 4, message_id: 'm-9' });
  assert.deepEqual(interpretWorking({ hook_event_name: 'Stop', session_id: 'codex-thread', turn_id: 't-1' }, {}),
    { thread: 'codex-thread', turn_id: 't-1', working: false });
  assert.equal(interpretWorking({ hook_event_name: 'Stop', session_id: 'codex-thread' }, {}), null);
  assert.equal(interpretTurnEnd({ hook_event_name: 'Stop', session_id: 'codex-thread' }, {}), null);
  assert.equal(interpret({ hook_event_name: 'PreToolUse', prompt }, { CLAUDE_CODE_SESSION_ID: 'x' }), null);
  assert.equal(interpret({ hook_event_name: 'UserPromptSubmit', prompt: 'typed by the user' }, { CLAUDE_CODE_SESSION_ID: 'x' }), null);
  // The hook command names its harness. A Codex session launched from a Claude Code terminal inherits
  // CLAUDE_CODE_SESSION_ID, and both payloads carry a session_id, so only the declaration settles it.
  const leaked = { CLAUDE_CODE_SESSION_ID: 'a-claude-session-on-this-machine' };
  assert.equal(interpretWorking({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', turn_id: 't-1', prompt }, leaked), null,
    'without a declaration the inherited Claude environment claims the event');
  assert.deepEqual(interpretWorking({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', turn_id: 't-1', prompt },
    { ...leaked, SIDEVOICE_HOOK_HARNESS: 'codex' }),
    { thread: 'codex-thread', turn_id: 't-1', working: true, session_id: 's-9', revision: 4, message_id: 'm-9' });
  assert.equal(interpret({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', prompt },
    { ...leaked, SIDEVOICE_HOOK_HARNESS: 'claude' }).thread, 'a-claude-session-on-this-machine');
  assert.equal(interpret({ hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', prompt },
    { ...leaked, SIDEVOICE_HOOK_HARNESS: 'nosuchharness' }), null);
  assert.equal(declaredHarness(['node', 'hook.mjs', '--harness', 'codex'], {}), 'codex');
  assert.equal(declaredHarness(['node', 'hook.mjs', '--harness=codex'], {}), 'codex');
  assert.equal(declaredHarness(['node', 'hook.mjs'], { SIDEVOICE_HOOK_HARNESS: 'claude' }), 'claude');
  assert.equal(declaredHarness(['node', 'hook.mjs'], {}), null);
  assert.match(nudge(claude), /voice_say/); assert.match(nudge(claude), /s-9/); assert.match(nudge(claude), /revision 4/);
});

test('connector: the conversation\'s state is said again on a clock, not only when it changes', async () => {
  // A room that restarts has forgotten what it was told. Waiting for the next change means a conversation
  // that was already working shows nothing at all until it stops (2026-09-20).
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const claudeHome = mkdtempSync(path.join(os.tmpdir(), 'sv-claude-'));
  mkdirSync(path.join(claudeHome, 'sessions'));
  writeFileSync(path.join(claudeHome, 'sessions', '4242.json'), JSON.stringify({ sessionId: 'busy-session', pid: 4242, status: 'busy' }));
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-1', thread: frame.thread }));
  };
  const { child, socketPath } = startConnector(room, dataDir,
    { CLAUDE_CONFIG_DIR: claudeHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '120' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'busy-session', harness: 'claude', thread: 'busy-session',
      delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'busy-session' } });
    await until(() => room.frames.filter(f => f.type === 'input.working' && f.working === true).length >= 3,
      4000);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: Codex lifecycle reports are correlated, idempotent, and a stale stop cannot clear newer work', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-' + frame.client_ref, thread: frame.thread }));
  };
  const { child, socketPath } = startConnector(room, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'codex-thread', harness: 'codex', thread: 'codex-thread', delivery: { kind: 'codex-queue', thread: 'codex-thread' } });
    for (const payload of [
      { hook_event_name: 'UserPromptSubmit', session_id: 'codex-thread', turn_id: 'hook-turn', prompt: 'typed directly' },
      { hook_event_name: 'Stop', session_id: 'codex-thread', turn_id: 'hook-turn' },
    ]) {
      const hook = spawn(process.execPath, [hookPath, 'hook', '--harness', 'codex'],
        { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CODEX_THREAD_ID: '', CLAUDE_CODE_SESSION_ID: 'a-claude-session-on-this-machine' },
          stdio: ['pipe', 'pipe', 'pipe'] });
      hook.stdin.end(JSON.stringify(payload));
      assert.equal(await new Promise(r => hook.on('exit', r)), 0);
    }
    await until(() => room.frames.some(f => f.type === 'input.working' && f.turn_id === 'hook-turn' && f.working === false));
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'old', working: true, session_id: 's', revision: 3 }), { status: 'sent' });
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'old', working: true, session_id: 's', revision: 3 }), { status: 'already_reported' });
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'new', working: true, session_id: 's', revision: 4 }), { status: 'sent' });
    const workingFrames = room.frames.filter(f => f.type === 'input.working').length;
    room.conn.send(JSON.stringify({ type: 'binding.registered', client_ref: 'codex-thread', binding_id: 'b-codex-thread', thread: 'codex-thread' }));
    await until(() => room.frames.filter(f => f.type === 'input.working').length > workingFrames);
    const resync = room.frames.filter(f => f.type === 'input.working').at(-1);
    assert.deepEqual({ working: resync.working, turn_id: resync.turn_id, turn_phase: resync.turn_phase },
      { working: true, turn_id: undefined, turn_phase: undefined });
    const falseBeforeStaleStop = room.frames.filter(f => f.type === 'input.working' && f.working === false).length;
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'old', working: false }), { status: 'sent' });
    assert.equal(room.frames.filter(f => f.type === 'input.working' && f.working === false).length, falseBeforeStaleStop);
    assert.ok(room.frames.some(f => f.type === 'input.working' && f.turn_id === 'old'
      && f.turn_phase === 'end' && f.working === true));
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'new', working: false }), { status: 'sent' });
    await until(() => room.frames.some(f => f.type === 'input.working' && f.working === false && f.turn_id === 'new'));
    const ended = room.frames.find(f => f.type === 'input.working' && f.working === false && f.turn_id === 'new');
    assert.equal(ended.turn_phase, 'end');
    assert.deepEqual({ session_id: ended.session_id, revision: ended.revision }, { session_id: 's', revision: 4 });
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'new', working: false }), { status: 'already_reported' });
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'finished-first', working: false }), { status: 'stale' });
    assert.deepEqual(await facade.call('working', { thread: 'codex-thread', turn_id: 'finished-first', working: true }), { status: 'stale' });
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: a read receipt from the hook reaches the room once per message, and the hook command hands context back', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1, heartbeat_seconds: 15 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-' + frame.client_ref, thread: frame.thread }));
  };
  const { child, socketPath } = startConnector(room, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-1', harness: 'claude', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });
    assert.deepEqual(await facade.call('read', { thread: 'thread-1', message_id: 'm-1', session_id: 's', revision: 2 }), { status: 'sent' });
    await until(() => room.frames.some(f => f.type === 'input.read' && f.message_id === 'm-1' && f.binding_id === 'b-thread-1' && f.revision === 2));
    assert.deepEqual(await facade.call('read', { thread: 'thread-1', message_id: 'm-1', session_id: 's', revision: 2 }), { status: 'already_reported' });
    assert.deepEqual(await facade.call('read', { thread: 'nobody', message_id: 'm-2', session_id: 's', revision: 2 }), { status: 'no_binding' });
    // The hook command itself: harness payload on stdin, read receipt to the connector, context on stdout, exit 0.
    const prompt = envelope({ channel: 'voice', session_id: 's', revision: 3, message_id: 'm-3', text: 'hola' });
    const hook = spawn(process.execPath, [hookPath, 'hook'], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CLAUDE_CODE_SESSION_ID: 'thread-1' }, stdio: ['pipe', 'pipe', 'pipe'] });
    let out = '', err = ''; hook.stdout.on('data', d => { out += d; }); hook.stderr.on('data', d => { err += d; });
    hook.stdin.end(JSON.stringify({ hook_event_name: 'UserPromptSubmit', session_id: 'x', prompt }));
    const code = await new Promise(r => hook.on('exit', r));
    assert.equal(code, 0, err);
    const output = JSON.parse(out.trim());
    assert.equal(output.hookSpecificOutput.hookEventName, 'UserPromptSubmit');
    assert.match(output.hookSpecificOutput.additionalContext, /revision 3/);
    await until(() => room.frames.some(f => f.type === 'input.read' && f.message_id === 'm-3'));
    // The same hook executable normalizes the harness's Stop event into an end-of-turn report.
    const stopped = spawn(process.execPath, [hookPath, 'hook'], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CLAUDE_CODE_SESSION_ID: 'thread-1' }, stdio: ['pipe', 'pipe', 'pipe'] });
    let stopOut = ''; stopped.stdout.on('data', d => { stopOut += d; });
    stopped.stdin.end(JSON.stringify({ hook_event_name: 'Stop', session_id: 'thread-1', turn_id: 'turn-3' }));
    assert.equal(await new Promise(r => stopped.on('exit', r)), 0); assert.equal(stopOut, '');
    await until(() => room.frames.some(f => f.type === 'input.working' && f.binding_id === 'b-thread-1' && f.working === false));
    // A prompt that is not ours produces nothing and touches nothing.
    const quiet = spawn(process.execPath, [hookPath, 'hook'], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CLAUDE_CODE_SESSION_ID: 'thread-1' }, stdio: ['pipe', 'pipe', 'pipe'] });
    let quietOut = ''; quiet.stdout.on('data', d => { quietOut += d; });
    quiet.stdin.end(JSON.stringify({ hook_event_name: 'UserPromptSubmit', prompt: 'escrito a mano' }));
    assert.equal(await new Promise(r => quiet.on('exit', r)), 0); assert.equal(quietOut, '');
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('mcp façade: an unpaired machine is told what to ask the user, and pairing is done with the code the user gave', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const redeemed = [];
  const room = http.createServer((request, response) => {
    let body = ''; request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      redeemed.push({ url: request.url, body: JSON.parse(body || '{}') });
      response.setHeader('content-type', 'application/json');
      if (request.url === '/api/connectors/pair' && JSON.parse(body).code === 'ABCD1234') return response.end(JSON.stringify({ connector_id: 'c-9', token: 't-9', protocol: 1 }));
      response.statusCode = 403; response.end(JSON.stringify({ detail: 'Código de emparejamiento inválido o caducado.' }));
    });
  });
  await new Promise(resolve => room.listen(0, '127.0.0.1', resolve));
  const roomUrl = `http://127.0.0.1:${room.address().port}`;
  const child = spawn(process.execPath, [mcpPath], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, CLAUDE_CODE_SESSION_ID: 'sess-abc', CLAUDE_CODE_MESSAGING_SOCKET: '/tmp/x.sock', CLAUDE_CODE_MESSAGING_TOKEN: 'tok' }, stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = []; let out = ''; child.stdout.on('data', d => { out += d; let i; while ((i = out.indexOf('\n')) >= 0) { replies.push(JSON.parse(out.slice(0, i))); out = out.slice(i + 1); } });
  const ask = (id, method, params) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  try {
    // Not paired: no connector is started, no code is fetched; the model is told to ask the person.
    ask(1, 'tools/call', { name: 'voice_connect', arguments: { title: 'Prueba' } });
    await until(() => replies.length === 1);
    assert.match(replies[0].error.message, /not paired with any room/);
    assert.match(replies[0].error.message, /Emparejar conector/);
    assert.match(replies[0].error.message, /voice_pair/);
    assert.equal(redeemed.length, 0, 'nothing was asked of any room');
    // A wrong code is the room's refusal, verbatim.
    ask(2, 'tools/call', { name: 'voice_pair', arguments: { room: roomUrl, code: 'NOPE0000' } });
    await until(() => replies.length === 2);
    assert.match(replies[1].error.message, /inválido o caducado/);
    assert.ok(!existsSync(path.join(dataDir, 'credentials.json')));
    // The code the user read from the room, redeemed once; the credential lands on this machine only.
    ask(3, 'tools/call', { name: 'voice_pair', arguments: { room: roomUrl + '/voice/', code: ' abcd1234 ' } });
    await until(() => replies.length === 3);
    const paired = JSON.parse(replies[2].result.content[0].text);
    assert.equal(paired.status, 'paired'); assert.equal(paired.room, roomUrl);
    assert.deepEqual(redeemed.map(r => r.url), ['/api/connectors/pair', '/api/connectors/pair']);
    assert.equal(redeemed[1].body.code, 'ABCD1234', 'trimmed and upper-cased, as the room shows it');
    const credential = JSON.parse(readFileSync(path.join(dataDir, 'credentials.json'), 'utf8'));
    assert.equal(credential.token, 't-9'); assert.match(credential.url, /^ws:\/\/127\.0\.0\.1:\d+\/api\/connectors\/ws$/);
    // Naming a different room does not silently re-pair: one room per machine, the switch is the user's.
    ask(4, 'tools/call', { name: 'voice_connect', arguments: { room: 'https://other.example' } });
    await until(() => replies.length === 4);
    assert.match(replies[3].error.message, new RegExp(`paired with ${roomUrl}, not https://other.example`));
    assert.equal(redeemed.length, 2);
  } finally { child.kill(); await new Promise(resolve => room.close(resolve)); }
});

test('install: puts this version in front of the harness, re-pins an older registration, and pairs with nothing', async () => {
  // Installing is mechanical and repeatable; pairing is a person's act and is not part of it. A machine-wide
  // Codex file and Claude Code's inbound safeguard are printed, never written.
  const { install, codexInstructions } = await import('../install.mjs');
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
  // A stand-in for `claude`: records every call, answers `mcp get` with whatever the test says is registered.
  const log = path.join(home, 'claude.log'), registered = path.join(home, 'registered.txt'), bin = path.join(home, 'claude');
  writeFileSync(bin, `#!/bin/sh\necho "$@" >> "${log}"\nif [ "$2" = get ]; then [ -s "${registered}" ] && cat "${registered}" || exit 1; fi\n`, { mode: 0o755 });
  const calls = () => existsSync(log) ? readFileSync(log, 'utf8').trim().split('\n') : [];
  const env = { ...process.env, SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), CLAUDE_CONFIG_DIR: path.join(home, '.claude'), SIDEVOICE_CLAUDE_BIN: bin };
  mkdirSync(env.CLAUDE_CONFIG_DIR);
  const { command, args } = (await import('../install.mjs')).serverCommand(env);
  const wanted = [command, ...args].join(' ');

  assert.rejects(install(['https://room.example', '--harness', 'claude'], env), /Pairing is not part of installing/, 'a room address is refused, with where pairing lives');

  const first = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice', `mcp add --scope user sidevoice -- ${wanted}`], 'nothing registered: it registers this version');
  assert.match(first.done.join('\n'), /Registered the MCP server/);
  assert.match(first.done.join('\n'), /not paired with any room yet/);
  assert.match(first.next.join('\n'), /Emparejar conector/, 'and it says the conversation will ask for the code');
  assert.ok(existsSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room', 'hook.mjs')), 'the skill and its hook runtime are in place');
  assert.ok(!existsSync(path.join(env.SIDEVOICE_DATA_DIR, 'credentials.json')), 'no pairing happened');

  // Registered at this version already: nothing to change, and it says so.
  writeFileSync(registered, `sidevoice:\n  Scope: User config (available in all your projects)\n  Type: stdio\n  Command: ${command}\n  Args: ${args.join(' ')}\n`);
  writeFileSync(log, '');
  const again = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice']);
  assert.match(again.done.join('\n'), /already runs this version/);
  assert.match(again.done.join('\n'), /Skill updated/);

  // An older pin: after an upgrade, running install again moves the harness to the new version.
  writeFileSync(registered, `sidevoice:\n  Scope: User config (available in all your projects)\n  Type: stdio\n  Command: npx\n  Args: -y @sidevoice/uplink@0.1.0 mcp\n`);
  writeFileSync(log, '');
  const upgraded = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice', 'mcp remove --scope user sidevoice', `mcp add --scope user sidevoice -- ${wanted}`]);
  assert.match(upgraded.done.join('\n'), /Re-pointed .* \(was: npx -y @sidevoice\/uplink@0\.1\.0 mcp\)/);

  // Registered somewhere that is not ours to move: left alone, with the command to move it.
  writeFileSync(registered, `sidevoice:\n  Scope: Project config (shared via .mcp.json)\n  Type: stdio\n  Command: npx\n  Args: -y @sidevoice/uplink@0.1.0 mcp\n`);
  writeFileSync(log, '');
  const elsewhere = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice']);
  assert.match(elsewhere.done.join('\n'), /outside user scope .* not touched/);

  // A paired machine is reported as such, never re-paired.
  mkdirSync(env.SIDEVOICE_DATA_DIR, { recursive: true });
  writeFileSync(path.join(env.SIDEVOICE_DATA_DIR, 'credentials.json'), JSON.stringify({ url: 'wss://room.example/api/connectors/ws', connector_id: 'c-1', token: 't-1' }));
  const paired = await install(['--harness', 'claude'], env);
  assert.match(paired.done.join('\n'), /paired with https:\/\/room\.example \(connector c-1\)/);
  assert.ok(!paired.next.join('\n').includes('Emparejar conector'));

  // Codex is instructions, not edits: its configuration is machine-wide and not ours to rewrite.
  const codex = codexInstructions(env);
  assert.match(codex, /\[mcp_servers\.sidevoice\]/);
  assert.match(codex, /--harness codex/);
  assert.match(codex, /does not rewrite it/);
});

test('skill: install copies the voice skill and a standalone hook, repairs itself, and never touches a foreign skill', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-skills-'));
  assert.equal(skillStatus(dir).state, 'absent');
  const first = installSkill(dir);
  assert.equal(first.action, 'installed'); assert.equal(first.hook, true);
  const manifest = readFileSync(path.join(dir, 'voice-room', 'SKILL.md'), 'utf8');
  assert.match(manifest, /^name: voice-room$/m); assert.match(manifest, /UserPromptSubmit:\n    - hooks:/, 'settings.json shape: a list of hook groups'); assert.ok(manifest.includes(`node "${path.join(dir, 'voice-room')}/hook.mjs"`), 'the hook command names the installed copy by absolute path'); assert.ok(!manifest.includes('__SIDEVOICE_SKILL_DIR__'));
  assert.match(manifest, /Stop:\n    - hooks:/, 'the harness reports the end of a turn through the same hook');
  writeFileSync(path.join(dir, 'voice-room', 'hook.mjs'), 'broken');
  assert.equal(installSkill(dir).action, 'updated');
  assert.equal(readFileSync(path.join(dir, 'voice-room', 'hook.mjs'), 'utf8'), readFileSync(path.join(here, '..', 'hook.mjs'), 'utf8'));
  assert.equal(existsSync(path.join(dir, 'voice-room', 'harness-contract.mjs')), true);
  // The installed hook runs on its own, from the skill directory, with no connector around: silent, exit 0.
  const child = spawn(process.execPath, [path.join(dir, 'voice-room', 'hook.mjs')], { env: { ...process.env, SIDEVOICE_DATA_DIR: path.join(dir, 'nowhere'), CLAUDE_CODE_SESSION_ID: 's' }, stdio: ['pipe', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', d => { out += d; });
  child.stdin.end(JSON.stringify({ hook_event_name: 'UserPromptSubmit', prompt: envelope({ channel: 'voice', session_id: 's', revision: 1, message_id: 'm', text: 'hola' }) }));
  assert.equal(await new Promise(r => child.on('exit', r)), 0);
  assert.match(out, /additionalContext/);
  assert.equal(removeSkill(dir).action, 'removed'); assert.equal(skillStatus(dir).state, 'absent');
  mkdirSync(path.join(dir, 'voice-room')); writeFileSync(path.join(dir, 'voice-room', 'SKILL.md'), '---\nname: voice-room\n---\nsomeone else\'s');
  assert.equal(skillStatus(dir).state, 'foreign');
  assert.throws(() => installSkill(dir), /not Sidevoice/); assert.throws(() => removeSkill(dir), /not Sidevoice/);
  assert.equal(readFileSync(path.join(dir, 'voice-room', 'SKILL.md'), 'utf8').includes('someone else'), true);
});

test('The harness says whether it is working: Claude Code publishes it per session, and an unknown state is not a guess', () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-claude-'));
  const registry = path.join(home, 'sessions');
  mkdirSync(registry);
  const write = (name, record) => writeFileSync(path.join(registry, name + '.json'), JSON.stringify(record));
  const previous = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = home;
  try {
    write('1', { sessionId: 'busy-one', status: 'busy', pid: 1 });
    write('2', { sessionId: 'idle-one', status: 'idle', pid: 2 });
    write('3', { sessionId: 'odd-one', status: 'something-new', pid: 3 });
    write('4', { sessionId: 'quiet-one', pid: 4 });
    assert.equal(sessionWorking('busy-one'), true);
    assert.equal(sessionWorking('idle-one'), false);
    // A status this code does not know, and a session that publishes none, are not evidence either way:
    // the room falls back to what the conversation says about its own replies.
    assert.equal(sessionWorking('odd-one'), null);
    assert.equal(sessionWorking('quiet-one'), null);
    assert.equal(sessionWorking('nobody'), null);
  } finally { if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR; else process.env.CLAUDE_CONFIG_DIR = previous; }
});
