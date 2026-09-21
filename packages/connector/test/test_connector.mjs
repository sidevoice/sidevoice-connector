import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createWsServer } from './ws-server.mjs';
import { envelope, nudge, voiceEnvelope } from '../harness-contract.mjs';
import { sessionWorking, transcriptPath, userMessageText } from '../harness-claude.mjs';
import { interpretRollout, rolloutPath } from '../harness-codex.mjs';
import { install as installSkill, remove as removeSkill, status as skillStatus } from '../skill.mjs';
import './test_harness_contract.mjs';
import './test_harness_claude.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
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

test('envelope: the header first, the user\'s words, then the speak-first note — and the header is found wherever a harness puts it', () => {
  const text = envelope({ channel: 'voice', session_id: 's', revision: 2, message_id: 'm', text: 'hola' });
  const [header, body, note] = text.split('\n\n');
  assert.deepEqual(JSON.parse(header), { channel: 'voice', session_id: 's', revision: 2, message_id: 'm' });
  assert.equal(body, 'hola');
  assert.equal(note, nudge({ session_id: 's', revision: 2 }));
  assert.match(note, /^\[Sidevoice\] /); assert.match(note, /voice_say/); assert.match(note, /"s"/); assert.match(note, /revision 2/);
  // Room control carries no note: nothing is asked of the conversation.
  assert.ok(!envelope({ channel: 'room-control', session_id: 's', revision: 2, message_id: 'm', text: 'x' }).includes('[Sidevoice]'));
  assert.deepEqual(voiceEnvelope(text), { channel: 'voice', message_id: 'm', session_id: 's', revision: 2 });
  assert.deepEqual(voiceEnvelope('Another Claude session sent a message:\n' + text), { channel: 'voice', message_id: 'm', session_id: 's', revision: 2 });
  assert.equal(voiceEnvelope('just a prompt'), null);
  assert.equal(voiceEnvelope('{"channel":"other","message_id":"x","session_id":"s","revision":1}'), null);
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

/** A fake Claude Code inbox: accepts the auth and user frames and keeps the connection open, as the real one does. */
function fakeInbox(dir) {
  const socketPath = path.join(dir, 'inbox.sock'); const received = [];
  const server = net.createServer(socket => { let buffer = ''; socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (line) received.push(JSON.parse(line)); } }); socket.on('error', () => {}); });
  return { socketPath, received, ready: new Promise(r => server.listen(socketPath, r)), close: () => server.close() };
}

test('connector: Claude Code — the message is read when the session\'s transcript takes it, and the turn is correlated, with nothing installed in the session', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const claudeHome = mkdtempSync(path.join(os.tmpdir(), 'sv-claude-'));
  mkdirSync(path.join(claudeHome, 'sessions')); mkdirSync(path.join(claudeHome, 'projects', '-home-someone-project'), { recursive: true });
  const registry = path.join(claudeHome, 'sessions', '4242.json');
  const transcript = path.join(claudeHome, 'projects', '-home-someone-project', 'sess-1.jsonl');
  const status = state => writeFileSync(registry, JSON.stringify({ sessionId: 'sess-1', pid: 4242, status: state }));
  const line = entry => appendFileSync(transcript, JSON.stringify(entry) + '\n');
  status('idle'); line({ type: 'user', message: { role: 'user', content: 'an older prompt, before we started watching' } });
  const inbox = fakeInbox(dataDir); await inbox.ready;
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-1', thread: frame.thread }));
  };
  const { child, socketPath, stderr } = startConnector(room, dataDir, { CLAUDE_CONFIG_DIR: claudeHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'sess-1', harness: 'claude', thread: 'sess-1', title: 'T', delivery: { kind: 'claude-uds', socket: inbox.socketPath, token: 'tok' } });
    await until(() => room.conn);
    room.conn.send(JSON.stringify({ type: 'input.deliver', event_id: 'e-1', binding_id: 'b-1', channel: 'voice', session_id: 's', revision: 3, message_id: 'm-1', text: 'hola desde la sala' }));
    await until(() => inbox.received.some(f => f.type === 'user'));
    const posted = inbox.received.find(f => f.type === 'user').message.content;
    assert.match(posted, /^\{"channel":"voice"/); assert.match(posted, /hola desde la sala/); assert.match(posted, /\[Sidevoice\] .*voice_say/, 'the speak-first note travels inside the message');
    // Nothing is claimed until the transcript shows the message — and the inbox has not even answered yet
    // (Claude Code keeps the socket open; the message is taken long before that call settles).
    await wait(150);
    assert.ok(!room.frames.some(f => f.type === 'input.read'), 'no receipt before the session takes the message');
    assert.ok(!room.frames.some(f => f.type === 'input.ack' && f.event_id === 'e-1'), 'the delivery call is still open');
    // A prompt typed by hand is not ours.
    line({ type: 'user', promptId: 'p-0', message: { role: 'user', content: [{ type: 'text', text: 'escrito a mano' }] } });
    await wait(120);
    assert.ok(!room.frames.some(f => f.type === 'input.read'));
    // The session takes the message: it lands in the transcript as Claude Code writes it, and goes busy.
    line({ type: 'user', promptId: 'p-1', message: { role: 'user', content: 'Another Claude session sent a message:\n' + posted } });
    status('busy');
    await until(() => room.frames.some(f => f.type === 'input.read' && f.message_id === 'm-1'));
    const read = room.frames.find(f => f.type === 'input.read');
    assert.deepEqual({ binding_id: read.binding_id, session_id: read.session_id, revision: read.revision, turn_id: read.turn_id }, { binding_id: 'b-1', session_id: 's', revision: 3, turn_id: 'p-1' });
    await until(() => room.frames.some(f => f.type === 'input.working' && f.working === true && f.turn_phase === 'start'));
    const started = room.frames.find(f => f.type === 'input.working' && f.turn_phase === 'start');
    assert.deepEqual({ turn_id: started.turn_id, session_id: started.session_id, revision: started.revision }, { turn_id: 'p-1', session_id: 's', revision: 3 });
    // The turn ends: the registry goes idle, and the end carries the same correlation.
    status('idle');
    await until(() => room.frames.some(f => f.type === 'input.working' && f.working === false && f.turn_phase === 'end'));
    const ended = room.frames.find(f => f.type === 'input.working' && f.working === false && f.turn_phase === 'end');
    assert.deepEqual({ turn_phase: ended.turn_phase, turn_id: ended.turn_id, session_id: ended.session_id, revision: ended.revision }, { turn_phase: 'end', turn_id: 'p-1', session_id: 's', revision: 3 });
    // The same transcript line read again (a rewrite, a restart) is not a second receipt.
    assert.equal(room.frames.filter(f => f.type === 'input.read').length, 1);
    assert.match(stderr(), /sess-1 read m-1/);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); inbox.close(); await room.close(); }
});

test('connector: Codex — the thread\'s rollout says when it took the message and when the turn ended; nothing is configured in Codex', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const codexHome = mkdtempSync(path.join(os.tmpdir(), 'sv-codex-'));
  const day = path.join(codexHome, 'sessions', '2026', '09', '21'); mkdirSync(day, { recursive: true });
  const rollout = path.join(day, 'rollout-2026-09-21T10-00-00-thread-7.jsonl');
  const line = (type, payload) => appendFileSync(rollout, JSON.stringify({ timestamp: new Date().toISOString(), type, payload }) + '\n');
  line('session_meta', { id: 'thread-7' });
  // A stand-in for `codex queue`: records the message it was given and confirms.
  const queued = path.join(codexHome, 'queued.txt');
  const bin = path.join(codexHome, 'codex'); writeFileSync(bin, `#!/bin/sh\nprintf '%s' "$5" > "${queued}"\n`, { mode: 0o755 });
  room.handle = (frame, c) => {
    if (frame.type === 'connector.hello') c.send(JSON.stringify({ type: 'connector.welcome', protocol: 1 }));
    if (frame.type === 'binding.register') c.send(JSON.stringify({ type: 'binding.registered', client_ref: frame.client_ref, binding_id: 'b-7', thread: frame.thread }));
  };
  const { child, socketPath } = startConnector(room, dataDir, { CODEX_HOME: codexHome, SIDEVOICE_CODEX_BIN: bin, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-7', harness: 'codex', thread: 'thread-7', title: 'T', delivery: { kind: 'codex-queue', thread: 'thread-7' } });
    await until(() => room.conn);
    room.conn.send(JSON.stringify({ type: 'input.deliver', event_id: 'e-7', binding_id: 'b-7', channel: 'voice', session_id: 's', revision: 5, message_id: 'm-7', text: 'hola codex' }));
    await until(() => room.frames.some(f => f.type === 'input.ack' && f.event_id === 'e-7' && f.status === 'accepted'));
    const message = readFileSync(queued, 'utf8');
    assert.match(message, /hola codex/); assert.match(message, /\[Sidevoice\]/);
    // Codex takes it on the next turn: task_started, then the user message, later task_complete.
    line('event_msg', { type: 'task_started', turn_id: 'turn-a' });
    await until(() => room.frames.some(f => f.type === 'input.working' && f.working === true));
    assert.ok(!room.frames.some(f => f.type === 'input.read'));
    line('response_item', { type: 'message', role: 'user', content: [{ type: 'input_text', text: message }] });
    await until(() => room.frames.some(f => f.type === 'input.read' && f.message_id === 'm-7'));
    const read = room.frames.find(f => f.type === 'input.read');
    assert.deepEqual({ turn_id: read.turn_id, session_id: read.session_id, revision: read.revision }, { turn_id: 'turn-a', session_id: 's', revision: 5 });
    await until(() => room.frames.some(f => f.type === 'input.working' && f.turn_phase === 'start' && f.turn_id === 'turn-a'));
    line('event_msg', { type: 'task_complete', turn_id: 'turn-a' });
    await until(() => room.frames.some(f => f.type === 'input.working' && f.working === false));
    const ended = room.frames.find(f => f.type === 'input.working' && f.working === false);
    assert.deepEqual({ turn_phase: ended.turn_phase, turn_id: ended.turn_id, session_id: ended.session_id, revision: ended.revision }, { turn_phase: 'end', turn_id: 'turn-a', session_id: 's', revision: 5 });
    // A later turn of its own: working, uncorrelated, and its end clears nothing of ours.
    line('event_msg', { type: 'task_started', turn_id: 'turn-b' });
    line('response_item', { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'typed in codex' }] });
    line('event_msg', { type: 'task_complete', turn_id: 'turn-b' });
    await until(() => room.frames.filter(f => f.type === 'input.working' && f.working === false).length >= 2);
    assert.equal(room.frames.filter(f => f.type === 'input.read').length, 1);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('harness modules: the files each harness writes are found by name, and a rollout line reads as the contract', () => {
  const claudeHome = mkdtempSync(path.join(os.tmpdir(), 'sv-claude-'));
  mkdirSync(path.join(claudeHome, 'projects', 'a'), { recursive: true }); mkdirSync(path.join(claudeHome, 'projects', 'b'));
  writeFileSync(path.join(claudeHome, 'projects', 'b', 'sess-9.jsonl'), '');
  const codexHome = mkdtempSync(path.join(os.tmpdir(), 'sv-codex-'));
  mkdirSync(path.join(codexHome, 'sessions', '2026', '08', '30'), { recursive: true }); mkdirSync(path.join(codexHome, 'sessions', '2026', '09', '02'), { recursive: true });
  writeFileSync(path.join(codexHome, 'sessions', '2026', '08', '30', 'rollout-2026-08-30T01-02-03-thread-9.jsonl'), '');
  const previous = { claude: process.env.CLAUDE_CONFIG_DIR, codex: process.env.CODEX_HOME };
  process.env.CLAUDE_CONFIG_DIR = claudeHome; process.env.CODEX_HOME = codexHome;
  try {
    assert.equal(transcriptPath('sess-9'), path.join(claudeHome, 'projects', 'b', 'sess-9.jsonl'));
    assert.equal(transcriptPath('nobody'), null);
    assert.equal(rolloutPath('thread-9'), path.join(codexHome, 'sessions', '2026', '08', '30', 'rollout-2026-08-30T01-02-03-thread-9.jsonl'));
    assert.equal(rolloutPath('nobody'), null);
  } finally {
    if (previous.claude === undefined) delete process.env.CLAUDE_CONFIG_DIR; else process.env.CLAUDE_CONFIG_DIR = previous.claude;
    if (previous.codex === undefined) delete process.env.CODEX_HOME; else process.env.CODEX_HOME = previous.codex;
  }
  assert.equal(userMessageText({ type: 'user', message: { role: 'user', content: 'plain' } }), 'plain');
  assert.equal(userMessageText({ type: 'user', message: { role: 'user', content: [{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }] } }), 'a\nb');
  assert.equal(userMessageText({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', content: 'x' }] } }), null, 'a tool result is not the user');
  assert.equal(userMessageText({ type: 'assistant', message: { role: 'assistant', content: 'x' } }), null);
  assert.equal(userMessageText({ type: 'queue-operation', operation: 'enqueue', content: '{"channel":"voice"}' }), null, 'queued is not admitted');
  const state = {};
  assert.deepEqual(interpretRollout({ type: 'event_msg', payload: { type: 'task_started', turn_id: 't1' } }, state), { working: true, turn_id: 't1' });
  assert.deepEqual(interpretRollout({ type: 'response_item', payload: { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'hi' }] } }, state), { text: 'hi', turn_id: 't1' });
  assert.equal(interpretRollout({ type: 'response_item', payload: { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'yo' }] } }, state), null);
  assert.equal(interpretRollout({ type: 'event_msg', payload: { type: 'token_count' } }, state), null);
  assert.deepEqual(interpretRollout({ type: 'event_msg', payload: { type: 'task_complete', turn_id: 't1' } }, state), { working: false, turn_id: 't1' });
  assert.equal(state.turn_id, null);
  assert.deepEqual(interpretRollout({ type: 'event_msg', payload: { type: 'turn_aborted', turn_id: 't2' } }, state), { working: false, turn_id: 't2' });
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
  assert.ok(existsSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room', 'SKILL.md')), 'the skill is in place');
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
  assert.ok(!codex.includes('hooks'), 'nothing but the MCP server is asked of Codex');
  assert.match(codex, /does not rewrite it/);
});

test('skill: install copies the voice skill, repairs itself, clears an older hook runtime, and never touches a foreign skill', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-skills-'));
  assert.equal(skillStatus(dir).state, 'absent');
  const first = installSkill(dir);
  assert.equal(first.action, 'installed');
  const manifest = readFileSync(path.join(dir, 'voice-room', 'SKILL.md'), 'utf8');
  assert.match(manifest, /^name: voice-room$/m); assert.ok(!manifest.includes('hooks:'), 'nothing is registered in the session');
  assert.match(manifest, /voice_pair/);
  // A copy from a version that shipped a hook runtime: reinstalling leaves only the skill.
  writeFileSync(path.join(dir, 'voice-room', 'hook.mjs'), 'old'); writeFileSync(path.join(dir, 'voice-room', 'harness-claude.mjs'), 'old');
  assert.equal(installSkill(dir).action, 'updated');
  assert.equal(existsSync(path.join(dir, 'voice-room', 'hook.mjs')), false); assert.equal(existsSync(path.join(dir, 'voice-room', 'harness-claude.mjs')), false);
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
