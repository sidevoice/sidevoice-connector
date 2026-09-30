import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { startRoom, PROTOCOL } from './room.mjs';
import { envelope, nudge, voiceEnvelope } from '../harness-contract.mjs';
import { sessionWorking, transcriptPath, userMessageText } from '../harness-claude.mjs';
import { interpretRollout, rolloutPath } from '../harness-codex.mjs';
import { remove as removeSkill, status as skillStatus } from '../skill.mjs';
import './test_harness_contract.mjs';
import './test_harness_claude.mjs';
import { chatStore, fakePersist, runView, TMUX } from './test_harness_cursor.mjs';

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

/** The credential names the room, and nothing about how to reach it: the path and the namespace
 *  are the connector's own knowledge, and a test that wrote them would be asserting its own copy. */
function startConnector(origin, dataDir, extraEnv = {}) {
  const socketPath = path.join(dataDir, 'connector.sock');
  writeFileSync(path.join(dataDir, 'credentials.json'), JSON.stringify({ url: origin, connector_id: 'c-1', token: 't-1' }));
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

test('connector: the handshake authenticates, registrations and speech are answered on the event that asked, deliveries stay in order, and a dead façade unregisters', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  // A local http receiver stands in for the harness.
  const received = []; let release = null;
  const harness = http.createServer(async (req, res) => { let body = ''; for await (const c of req) body += c; received.push(JSON.parse(body)); await new Promise(r => { release = r; }); res.writeHead(200); res.end('{}'); });
  await new Promise(r => harness.listen(0, '127.0.0.1', r));
  const deliveryUrl = `http://127.0.0.1:${harness.address().port}/presentation/message`;
  room.handle = (event, data) => {
    if (event === 'binding.register') {
      // First registration carries no server binding id; a re-registration does.
      assert.ok(data.binding_id === undefined || data.binding_id === 'b-' + data.client_ref);
      return { client_ref: data.client_ref, binding_id: 'b-' + data.client_ref, thread: data.thread };
    }
    if (event === 'speech.publish') return { status: 'queued', text_saved: true, utterance_id: data.utterance_id };
  };
  const { child, socketPath, stderr } = startConnector(room.origin, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const registered = await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: deliveryUrl, thread: 'thread-1' } });
    assert.equal(registered.binding_id, 'b-thread-1');
    assert.equal(room.sent('binding.register').length, 1);
    // Who this connector is travels in the handshake, before any event could.
    assert.equal(room.auth.connector_id, 'c-1'); assert.equal(room.auth.token, 't-1');
    assert.equal(room.auth.protocol, PROTOCOL);
    // And so does what this machine is, so the room's list reads as a machine and not as a UUID.
    assert.equal(room.auth.host, os.hostname());
    assert.match(room.auth.platform, new RegExp(`^(macOS|Windows|Linux|${os.platform()}) ${os.arch()}$`), room.auth.platform);
    assert.equal(room.auth.version, JSON.parse(readFileSync(path.join(here, '..', 'package.json'), 'utf8')).version);
    assert.ok(Array.isArray(room.auth.harnesses), JSON.stringify(room.auth.harnesses));
    // Two deliveries: the second must wait for the first to be acknowledged by the harness.
    const first = room.ask('input.deliver', { event_id: 'e1', binding_id: 'b-thread-1', thread: 'thread-1', text: 'uno', channel: 'voice', session_id: 's', revision: 1, message_id: 'm1' });
    const second = room.ask('input.deliver', { event_id: 'e2', binding_id: 'b-thread-1', thread: 'thread-1', text: 'dos', channel: 'voice', session_id: 's', revision: 1, message_id: 'm2' });
    await until(() => received.length === 1); await wait(100);
    assert.equal(received.length, 1); assert.equal(received[0].text, 'uno');
    release();
    assert.equal((await first).status, 'accepted', 'the answer comes back on the event that asked');
    await until(() => received.length === 2); assert.equal(received[1].text, 'dos'); release();
    await second;
    // Unknown binding is acknowledged as such, never silently dropped.
    assert.equal((await room.ask('input.deliver', { event_id: 'e3', binding_id: 'nope', text: 'x' })).status, 'unknown_binding');
    // Speech goes out and comes back confirmed; the durable outbox is empty afterwards.
    const said = await facade.call('publish', { binding_id: 'b-thread-1', session_id: 's', revision: 1, text: 'hola', language: 'es' });
    assert.equal(said.status, 'queued'); assert.equal(said.text_saved, true);
    assert.deepEqual(JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')), []);
    // The façade dies: its binding is unregistered and the connector exits once idle.
    facade.end();
    await until(() => room.sent('binding.unregister').some(d => d.binding_id === 'b-thread-1'));
    const code = await until(() => child.exitCode !== null ? child.exitCode + 1 : null, 5000);
    assert.equal(code - 1, 0, stderr());
  } finally { if (child.exitCode === null) child.kill(); await room.close(); harness.close(); }
});

test('connector: a room that comes back re-registers everything, and leaving works by conversation even after the id was re-minted', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  let mint = 'b-first';
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: mint, thread: data.thread };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const joined = await facade.call('register', { client_ref: 'thread-z', harness: 'claude', thread: 'thread-z', title: 'Z', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-z' } });
    assert.equal(joined.binding_id, 'b-first');
    // The room restarts, forgets its bindings and mints a new id. Nobody tells the connector to
    // come back: that is the library's, and re-registering what it holds is this connector's.
    const port = room.port;
    mint = 'b-second';
    await room.stop();
    await until(async () => (await facade.call('status', {})).connected === false, 10000);
    await room.start(port);
    await until(async () => (await facade.call('status', {})).bindings[0]?.binding_id === 'b-second', 20000);
    assert.ok(room.connections >= 2, 'it came back by itself');
    // Leaving with the id the façade remembers still leaves, because it names the conversation too.
    const left = await facade.call('unregister', { binding_id: 'b-first', client_ref: 'thread-z' });
    assert.equal(left.left, true);
    await until(() => room.sent('binding.unregister').some(d => d.binding_id === 'b-second'));
    assert.equal((await facade.call('status', {})).bindings.length, 0);
    // And leaving what was never joined says so instead of pretending.
    assert.equal((await facade.call('unregister', { binding_id: 'nope', client_ref: 'nobody' })).left, false);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: speech while offline is queued durably and replayed on reconnect', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const port = room.port;
  // The room is not there when the connector starts, so the first attempts are genuinely refused.
  await room.stop();
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-1', thread: data.thread };
    if (event === 'speech.publish') return { status: 'queued', text_saved: true };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const registration = facade.call('register', { client_ref: 'r', harness: 'test', thread: 'thread-1', delivery: { kind: 'http', url: 'http://127.0.0.1:1/', thread: 'thread-1' } });
    const said = await facade.call('publish', { client_ref: 'r', session_id: 's', revision: 0, text: 'sin sala' });
    assert.equal(said.status, 'queued');
    assert.equal(JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')).length, 1,
      'it waits on disk, not in the library');
    await room.start(port);
    await until(() => room.sent('speech.publish').some(d => d.text === 'sin sala'), 20000);
    await until(() => JSON.parse(readFileSync(path.join(dataDir, 'outbox.json'), 'utf8')).length === 0);
    const result = await registration; assert.equal(result.binding_id, 'b-1');
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: keeps retrying while the room is down and connects once it appears', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const room = await startRoom();
  const port = room.port;
  await room.stop();
  const { child, socketPath } = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  try {
    await until(() => existsSync(socketPath));
    await wait(1500); // several refused attempts happen in here
    // While the room is down, status says why — not just "not connected".
    const facade = ipcClient(socketPath); await facade.ready;
    const down = await facade.call('status', {});
    assert.equal(down.connected, false);
    assert.ok(down.socket_error && (down.socket_error.error || down.socket_error.close_reason), JSON.stringify(down.socket_error));
    assert.ok(down.socket_error.at);
    assert.equal(down.socket_error.retrying, true, 'a room that is not there yet is still worth asking');
    await room.start(port);
    await until(async () => (await facade.call('status', {})).connected, 20000);
    assert.equal((await facade.call('status', {})).socket_error, null, 'cleared once the room answers');
    facade.end();
  } finally { child.kill(); await room.close(); }
});

test('connector: a credential the room refuses is said out loud and is not asked again', async () => {
  // The room is up and says no. That is not a connection problem, and hammering it would neither
  // fix it nor tell anyone: the library stops, and what a person has to do is in the log.
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const room = await startRoom();
  room.admit = () => 'Esta máquina no está emparejada con la sala: vuelve a emparejarla con el código que la sala muestra en "Emparejar máquina".';
  const { child, socketPath, stderr } = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    const refused = await until(async () => {
      const status = await facade.call('status', {});
      return status.socket_error?.retrying === false ? status : null;
    }, 10000);
    assert.equal(refused.connected, false);
    assert.match(refused.socket_error.error, /Emparejar máquina/);
    assert.match(stderr(), /will not be asked again/);
    assert.match(stderr(), /Pair this machine again/);
    const attempts = refused.socket_error.attempt;
    await wait(1500);
    assert.equal((await facade.call('status', {})).socket_error.attempt, attempts, 'it stopped, rather than retrying for ever');
    facade.end();
  } finally { child.kill(); await room.close(); }
});

test('connector: a second instance defers to the live one', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const first = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '5000' });
  try {
    await until(() => existsSync(first.socketPath));
    const second = startConnector(room.origin, dataDir);
    const code = await until(() => second.child.exitCode !== null ? second.child.exitCode + 1 : null);
    assert.equal(code - 1, 0);
    assert.match(second.stderr(), /a connector is already running \(pid \d+/, 'it says whom it defers to, never silently');
    assert.equal(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8'), String(first.child.pid));
    const facade = ipcClient(first.socketPath); await facade.ready; assert.equal((await facade.call('status', {})).host.length > 0, true); facade.end();
  } finally { if (first.child.exitCode === null) first.child.kill(); await room.close(); }
});

test('connector: a lock left by a pid that is now something else is stale, not a live connector', async () => {
  // Pids are reused; on macOS a reused pid of another user even answers EPERM. The lock names this test's own
  // node process — alive, but not a connector — so a connector must take over instead of exiting.
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  writeFileSync(path.join(dataDir, 'connector.sock.lock'), String(process.pid));
  const only = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '5000' });
  try {
    await until(() => existsSync(only.socketPath));
    assert.match(only.stderr(), /stale lock .* taking over/);
    assert.equal(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8'), String(only.child.pid));
  } finally { if (only.child.exitCode === null) only.child.kill(); await room.close(); }
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
    ask(5, 'prompts/list', {}); ask(6, 'prompts/get', { name: 'voice-room', arguments: { title: 'Mi sesión' } });
    await until(() => replies.length === 5);
    // The join shortcut is the server's own prompt: nothing is copied into a harness for it.
    assert.equal(replies[0].result.capabilities.prompts !== undefined, true);
    const listed = replies.find(x => x.id === 5).result.prompts; assert.deepEqual(listed.map(p => p.name), ['voice-room']);
    const prompt = replies.find(x => x.id === 6).result.messages[0].content.text;
    assert.match(prompt, /voice_status/); assert.match(prompt, /voice_connect with the title "Mi sesión"/); assert.match(prompt, /voice_pair/); assert.match(prompt, /Emparejar máquina/);
    const instructions = replies[0].result.instructions;
    assert.ok(instructions.length <= 2048, `Claude Code keeps 2048 characters of instructions; these are ${instructions.length}`);
    assert.match(instructions, /voice_say/);
    assert.match(instructions, /short acknowledgement/);
    assert.match(instructions, /meaningful checkpoints/);
    assert.match(instructions, /take in newly arrived user input before starting the next step/);
    assert.match(instructions, /session_id and revision for every publication/);
    assert.match(instructions, /\[Sidevoice\] line that is not the user's/);
    assert.deepEqual(replies[1].result.tools.map(t => t.name), ['voice_connect', 'voice_pair', 'voice_say', 'voice_disconnect', 'voice_status']);
    assert.match(replies[0].result.instructions, /Never try to obtain a code from the room yourself/);
    assert.equal(replies[0].result.serverInfo.version, JSON.parse(readFileSync(path.join(here, '..', 'package.json'), 'utf8')).version, 'the façade says which version it is');
    const joined = JSON.parse(replies[2].result.content[0].text);
    assert.equal(joined.status, 'joined'); assert.equal(joined.harness, 'claude'); assert.equal(joined.conversation, 'sess-abc');
    assert.deepEqual(commands[0].params.delivery, { kind: 'claude-uds', socket: '/tmp/x.sock', token: 'tok' });
    assert.deepEqual(commands[0].params.capabilities, {
      deliver: 'supported', inspectInbound: 'supported', working: 'supported', endOfTurn: 'supported', sessionIdentity: 'supported',
    });
    ask(4, 'tools/call', { name: 'voice_say', arguments: { text: 'hola', session_id: 's', revision: 1 } });
    await until(() => replies.length === 6);
    assert.equal(JSON.parse(replies.find(x => x.id === 4).result.content[0].text).status, 'published');
    assert.equal(commands.find(c => c.method === 'publish').params.binding_id, 'b-9');
  } finally { child.kill(); fake.close(); }
});

test('connector: the room closing a conversation\'s voice removes the binding and the façade is told on its next call', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-' + data.client_ref, thread: data.thread };
    if (event === 'speech.publish') return { status: 'queued', text_saved: true, utterance_id: data.utterance_id };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir);
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });
    room.tell('binding.close', { binding_id: 'b-thread-1', thread: 'thread-1', reason: 'closed_from_room' });
    await until(async () => (await facade.call('status', {})).closed_by_room.includes('thread-1'));
    await assert.rejects(facade.call('publish', { binding_id: 'b-thread-1', client_ref: 'thread-1', session_id: 's', revision: 1, text: 'tarde' }), /CLOSED_BY_ROOM/);
    assert.equal((await facade.call('status', {})).bindings.length, 0);
    // The room never hears an unregister for a binding it closed itself.
    assert.equal(room.sent('binding.unregister').length, 0);
    // Joining again is the user's explicit request and clears the closure.
    const again = await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });
    assert.equal(again.binding_id, 'b-thread-1');
    assert.deepEqual((await facade.call('status', {})).closed_by_room, []);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: a pairing revoked from the room takes the voice now, says why, and is not asked again', async () => {
  // The room does this while the machine is connected: it says why, drops the bindings and closes the
  // socket, and refuses the next handshake with the same words. Neither the conversations nor whoever
  // reads status should be left with "the room disconnected me".
  const revoked = 'La sala revocó el emparejamiento de esta máquina: vuelve a emparejarla con el código que la sala muestra en "Emparejar máquina".';
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-' + data.client_ref, thread: data.thread };
  };
  const { child, socketPath, stderr } = startConnector(room.origin, dataDir, { SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } });

    // What the room does on a revocation, in the order it does it.
    room.admit = () => revoked;
    room.tell('binding.close', { binding_id: 'b-thread-1', thread: 'thread-1', reason: 'connector_revoked' });
    room.tell('connector.revoked', { reason: revoked });
    room.socket.disconnect(true);

    const status = await until(async () => {
      const seen = await facade.call('status', {});
      return seen.refused ? seen : null;
    });
    assert.equal(status.refused, revoked);
    assert.equal(status.room_error, revoked, 'voice_status reads why without having to parse a socket');
    assert.deepEqual(status.bindings, [], 'the conversation lost its voice at once');
    assert.equal(status.closed_reasons['thread-1'], 'connector_revoked', 'and can say which of the two happened');
    await assert.rejects(facade.call('publish', { binding_id: 'b-thread-1', client_ref: 'thread-1', session_id: 's', revision: 1, text: 'tarde' }),
      /CLOSED_BY_ROOM:connector_revoked/);

    // Joining again does not quietly wait for a room that will not have this machine: it says so.
    await assert.rejects(facade.call('register', { client_ref: 'thread-1', harness: 'test', thread: 'thread-1', title: 'T', delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'thread-1' } }),
      /revoc/);
    assert.match(stderr(), /will not be asked again/);
    const attempts = (await facade.call('status', {})).socket_error?.attempt ?? 0;
    await wait(1200);
    assert.equal((await facade.call('status', {})).socket_error?.attempt ?? 0, attempts, 'it stopped, rather than retrying for ever');
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
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-1', thread: data.thread };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir,
    { CLAUDE_CONFIG_DIR: claudeHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '120' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'busy-session', harness: 'claude', thread: 'busy-session',
      delivery: { kind: 'http', url: 'http://127.0.0.1:1/never', thread: 'busy-session' } });
    await until(() => room.sent('input.working').filter(d => d.working === true).length >= 3, 4000);
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
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-1', thread: data.thread };
  };
  const { child, socketPath, stderr } = startConnector(room.origin, dataDir, { CLAUDE_CONFIG_DIR: claudeHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'sess-1', harness: 'claude', thread: 'sess-1', title: 'T', delivery: { kind: 'claude-uds', socket: inbox.socketPath, token: 'tok' } });
    await until(() => room.socket);
    let acknowledged = null;
    const delivery = room.ask('input.deliver', { event_id: 'e-1', binding_id: 'b-1', channel: 'voice', session_id: 's', revision: 3, message_id: 'm-1', text: 'hola desde la sala' })
      .then(answer => { acknowledged = answer; });
    await until(() => inbox.received.some(f => f.type === 'user'));
    const posted = inbox.received.find(f => f.type === 'user').message.content;
    assert.match(posted, /^\{"channel":"voice"/); assert.match(posted, /hola desde la sala/); assert.match(posted, /\[Sidevoice\] .*voice_say/, 'the speak-first note travels inside the message');
    // Nothing is claimed until the transcript shows the message — and the inbox has not even answered yet
    // (Claude Code keeps the socket open; the message is taken long before that call settles).
    await wait(150);
    assert.equal(room.sent('input.read').length, 0, 'no receipt before the session takes the message');
    assert.equal(acknowledged, null, 'the delivery call is still open');
    // A prompt typed by hand is not ours.
    line({ type: 'user', promptId: 'p-0', message: { role: 'user', content: [{ type: 'text', text: 'escrito a mano' }] } });
    await wait(120);
    assert.equal(room.sent('input.read').length, 0);
    // The session takes the message: it lands in the transcript as Claude Code writes it, and goes busy.
    line({ type: 'user', promptId: 'p-1', message: { role: 'user', content: 'Another Claude session sent a message:\n' + posted } });
    status('busy');
    await until(() => room.sent('input.read').some(d => d.message_id === 'm-1'));
    const read = room.sent('input.read')[0];
    assert.deepEqual({ binding_id: read.binding_id, session_id: read.session_id, revision: read.revision, turn_id: read.turn_id }, { binding_id: 'b-1', session_id: 's', revision: 3, turn_id: 'p-1' });
    await until(() => room.sent('input.working').some(d => d.working === true && d.turn_phase === 'start'));
    const started = room.sent('input.working').find(d => d.turn_phase === 'start');
    assert.deepEqual({ turn_id: started.turn_id, session_id: started.session_id, revision: started.revision }, { turn_id: 'p-1', session_id: 's', revision: 3 });
    // The turn ends: the registry goes idle, and the end carries the same correlation.
    status('idle');
    await until(() => room.sent('input.working').some(d => d.working === false && d.turn_phase === 'end'));
    const ended = room.sent('input.working').find(d => d.working === false && d.turn_phase === 'end');
    assert.deepEqual({ turn_phase: ended.turn_phase, turn_id: ended.turn_id, session_id: ended.session_id, revision: ended.revision }, { turn_phase: 'end', turn_id: 'p-1', session_id: 's', revision: 3 });
    // The same transcript line read again (a rewrite, a restart) is not a second receipt.
    assert.equal(room.sent('input.read').length, 1);
    // Nobody launched this session with --model, and the room still learns what it thinks with: the
    // transcript says it on every answer, and only a change is worth a frame.
    assert.equal(room.sent('input.engine').length, 0, 'nothing is said before the session has answered once');
    line({ type: 'assistant', message: { model: 'claude-fable-5-1', role: 'assistant', content: [{ type: 'text', text: 'ya voy' }] }, uuid: 'a-1' });
    await until(() => room.sent('input.engine').length === 1);
    assert.deepEqual(room.sent('input.engine')[0], { binding_id: 'b-1', engine: { model: 'claude-fable-5-1', effort: null, thinking: null } });
    line({ type: 'assistant', message: { model: 'claude-fable-5-1', role: 'assistant', content: [{ type: 'text', text: 'sigo' }] }, uuid: 'a-2' });
    await wait(120);
    assert.equal(room.sent('input.engine').length, 1, 'the same model again is not news');
    await delivery;
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
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-7', thread: data.thread };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir, { CODEX_HOME: codexHome, SIDEVOICE_CODEX_BIN: bin, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'thread-7', harness: 'codex', thread: 'thread-7', title: 'T', delivery: { kind: 'codex-queue', thread: 'thread-7' } });
    await until(() => room.socket);
    const delivered = await room.ask('input.deliver', { event_id: 'e-7', binding_id: 'b-7', channel: 'voice', session_id: 's', revision: 5, message_id: 'm-7', text: 'hola codex' });
    assert.equal(delivered.status, 'accepted');
    const message = readFileSync(queued, 'utf8');
    assert.match(message, /hola codex/); assert.match(message, /\[Sidevoice\]/);
    // Codex takes it on the next turn: task_started, then the user message, later task_complete.
    line('event_msg', { type: 'task_started', turn_id: 'turn-a' });
    await until(() => room.sent('input.working').some(d => d.working === true));
    assert.equal(room.sent('input.read').length, 0);
    line('response_item', { type: 'message', role: 'user', content: [{ type: 'input_text', text: message }] });
    await until(() => room.sent('input.read').some(d => d.message_id === 'm-7'));
    const read = room.sent('input.read')[0];
    assert.deepEqual({ turn_id: read.turn_id, session_id: read.session_id, revision: read.revision }, { turn_id: 'turn-a', session_id: 's', revision: 5 });
    await until(() => room.sent('input.working').some(d => d.turn_phase === 'start' && d.turn_id === 'turn-a'));
    line('event_msg', { type: 'task_complete', turn_id: 'turn-a' });
    await until(() => room.sent('input.working').some(d => d.working === false));
    const ended = room.sent('input.working').find(d => d.working === false);
    assert.deepEqual({ turn_phase: ended.turn_phase, turn_id: ended.turn_id, session_id: ended.session_id, revision: ended.revision }, { turn_phase: 'end', turn_id: 'turn-a', session_id: 's', revision: 5 });
    // A later turn of its own: working, uncorrelated, and its end clears nothing of ours.
    line('event_msg', { type: 'task_started', turn_id: 'turn-b' });
    line('response_item', { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'typed in codex' }] });
    line('event_msg', { type: 'task_complete', turn_id: 'turn-b' });
    await until(() => room.sent('input.working').filter(d => d.working === false).length >= 2);
    assert.equal(room.sent('input.read').length, 1);
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: Cursor — the room hears the chat work, and a voice message is refused as undeliverable rather than tried', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const cursorHome = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const dir = path.join(cursorHome, 'projects', 'work-app', 'agent-transcripts', 'chat-7'); mkdirSync(dir, { recursive: true });
  const file = path.join(dir, 'chat-7.jsonl');
  const turn = text => JSON.stringify({ role: 'user', message: { content: [{ type: 'text', text }] } }) + '\n';
  writeFileSync(file, '');
  room.handle = (event, data) => {
    if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-c7', thread: data.thread };
  };
  const { child, socketPath } = startConnector(room.origin, dataDir, { CURSOR_DATA_DIR: cursorHome, CURSOR_CONFIG_DIR: cursorHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: 'chat-7', harness: 'cursor', thread: 'chat-7', title: 'C', delivery: { kind: 'none', chat: 'chat-7' }, capabilities: { deliver: 'unsupported' } });
    await until(() => room.socket);
    const refused = await room.ask('input.deliver', { event_id: 'e-c7', binding_id: 'b-c7', channel: 'voice', session_id: 's', revision: 1, message_id: 'm-c7', text: 'hola cursor' });
    assert.equal(refused.status, 'unsupported');
    assert.match(refused.error, /cursor offers no way to put a message into this conversation/);
    // The person types in Cursor: the turn is seen starting and ending.
    appendFileSync(file, turn('typed in cursor'));
    await until(() => room.sent('input.working').some(d => d.working === true));
    appendFileSync(file, JSON.stringify({ role: 'assistant', message: { content: [{ type: 'text', text: 'ok' }] } }) + '\n' + JSON.stringify({ type: 'turn_ended', status: 'success' }) + '\n');
    await until(() => room.sent('input.working').some(d => d.working === false));
    assert.equal(room.sent('input.read').length, 0, 'nothing was delivered, so nothing is read');
    facade.end();
  } finally { if (child.exitCode === null) child.kill(); await room.close(); }
});

test('connector: Cursor under persist — a voice message is typed into the chat\'s pane, and its transcript gives the second tick', { skip: !TMUX && 'no tmux on this machine' }, async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const cursorHome = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const chat = '0d3f6c1e-2b4a-4c5d-8e9f-a1b2c3d4e5f6';
  const dir = path.join(cursorHome, 'projects', 'work-app', 'agent-transcripts', chat); mkdirSync(dir, { recursive: true });
  const file = path.join(dir, chat + '.jsonl'); writeFileSync(file, '');
  const fake = await fakePersist(chat);
  room.handle = (event, data) => { if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-p', thread: data.thread }; };
  const { child, socketPath } = startConnector(room.origin, dataDir, { ...fake.env, CURSOR_DATA_DIR: cursorHome, CURSOR_CONFIG_DIR: cursorHome, SIDEVOICE_WORK_POLL_MS: '30', SIDEVOICE_WORK_ANNOUNCE_MS: '5000' });
  try {
    await until(() => existsSync(socketPath));
    const facade = ipcClient(socketPath); await facade.ready;
    await facade.call('register', { client_ref: chat, harness: 'cursor', thread: chat, title: 'P', delivery: { kind: 'cursor-tmux', chat },
      capabilities: { deliver: 'supported', inspectInbound: 'unsupported', working: 'supported', endOfTurn: 'supported', sessionIdentity: 'supported' }, experimental: ['deliver'] });
    await until(() => room.socket);
    assert.deepEqual(room.sent('binding.register')[0].experimental, ['deliver'], 'the room is told delivery is experimental');
    const typed = await room.ask('input.deliver', { event_id: 'e-p', binding_id: 'b-p', channel: 'voice', session_id: 's', revision: 2, message_id: 'm-p', text: 'hola\ncursor' });
    assert.equal(typed.status, 'unknown'); assert.match(typed.detail, /typed into persistent session cursor-test/);
    await until(() => existsSync(fake.received) && readFileSync(fake.received, 'utf8').endsWith('\r'));
    const pasted = readFileSync(fake.received, 'utf8').slice('\x1b[200~'.length, -('\x1b[201~\r'.length));
    assert.match(pasted, /hola\ncursor/);
    // Cursor takes it: the transcript's user line holds it (inside its own <user_query> wrapper).
    appendFileSync(file, JSON.stringify({ role: 'user', message: { content: [{ type: 'text', text: '<user_query>\n' + pasted + '\n</user_query>' }] } }) + '\n');
    await until(() => room.sent('input.read').some(d => d.message_id === 'm-p'));
    await until(() => room.sent('input.working').some(d => d.working === true));
    facade.end();
  } finally { fake.stop(); if (child.exitCode === null) child.kill(); await room.close(); }
});

test('façade: in the Cursor CLI the chat is the store its parent holds open, and voice_connect says the room cannot talk to it', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const socketPath = path.join(dataDir, 'connector.sock');
  const cursorHome = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  // This test process plays cursor-agent: it spawns the server and holds the chat's store open.
  const chat = chatStore(cursorHome, 'chat-e2e', { model: 'composer-2' });
  const commands = [];
  const fake = net.createServer(socket => { let buffer = ''; socket.on('data', chunk => { buffer += chunk; let i; while ((i = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, i); buffer = buffer.slice(i + 1); if (!line) continue; const input = JSON.parse(line); commands.push(input); const result = input.method === 'register' ? { binding_id: 'b-e2e', thread: input.params.thread, connected: true } : { connected: true, bindings: [], version: null }; socket.write(JSON.stringify({ id: input.id, ok: true, result }) + '\n'); } }); });
  await new Promise(r => fake.listen(socketPath, r));
  writeFileSync(path.join(dataDir, 'credentials.json'), JSON.stringify({ url: 'wss://room.example/api/connectors/ws', connector_id: 'c-1', token: 't-1' }));
  // What Cursor gives an MCP server: a scrubbed environment, and nothing about the chat.
  const env = { HOME: process.env.HOME, PATH: process.env.PATH, SHELL: process.env.SHELL || '/bin/sh', SIDEVOICE_DATA_DIR: dataDir, CURSOR_CONFIG_DIR: cursorHome };
  const child = spawn(process.execPath, [mcpPath], { env, stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = []; let out = ''; child.stdout.on('data', d => { out += d; let i; while ((i = out.indexOf('\n')) >= 0) { replies.push(JSON.parse(out.slice(0, i))); out = out.slice(i + 1); } });
  const ask = (id, method, params) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  try {
    ask(1, 'initialize', { protocolVersion: '2025-11-25', capabilities: { elicitation: { form: {} } }, clientInfo: { name: 'Cursor', version: '1.0.0' } });
    ask(2, 'tools/call', { name: 'voice_connect', arguments: { title: 'Cursor' } });
    await until(() => replies.length === 2);
    const reply = replies.find(r => r.id === 2);
    assert.ok(reply.result, JSON.stringify(reply.error));
    const joined = JSON.parse(reply.result.content[0].text);
    assert.equal(joined.harness, 'cursor');
    assert.equal(joined.conversation, 'chat-e2e');
    assert.equal(joined.delivery, 'none');
    assert.equal(joined.capabilities.deliver, 'unsupported');
    assert.deepEqual(joined.voice_in.speak_with, { session_id: 'typed:chat-e2e', revision: 0 });
    assert.match(joined.voice_in.reason, /cannot reach this conversation/);
    const register = commands.find(c => c.method === 'register').params;
    assert.deepEqual({ harness: register.harness, thread: register.thread, delivery: register.delivery, engine: register.engine },
      { harness: 'cursor', thread: 'chat-e2e', delivery: { kind: 'none', chat: 'chat-e2e' }, engine: { model: 'composer-2', effort: null, thinking: null } });
    // /new in cursor-agent: the same server, another chat. Joining from it leaves the first.
    chat.db.close();
    const next = chatStore(cursorHome, 'chat-next');
    try {
      ask(3, 'tools/call', { name: 'voice_connect', arguments: { title: 'Otra' } });
      await until(() => replies.some(r => r.id === 3));
      assert.equal(JSON.parse(replies.find(r => r.id === 3).result.content[0].text).conversation, 'chat-next');
      const after = commands.slice(commands.indexOf(commands.find(c => c.method === 'register')) + 1).filter(c => c.method !== 'status');
      assert.deepEqual(after.map(c => [c.method, c.params.client_ref || c.params.thread]), [['unregister', 'chat-e2e'], ['register', 'chat-next']]);
    } finally { next.db.close(); }
  } finally { child.kill(); fake.close(); try { chat.db.close(); } catch {} }
});

test('façade + connector: a chat of the Cursor editor joins through the view voice_connect carries, and the room\'s voice reaches it', async () => {
  const room = await startRoom();
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
  const port = String(40000 + Math.floor(Math.random() * 20000));
  room.handle = (event, data) => { if (event === 'binding.register') return { client_ref: data.client_ref, binding_id: 'b-ed', thread: data.thread }; };
  // A real connector, started as a façade would start it; then the façade, spawned as the editor spawns it.
  const { child: connector, socketPath } = startConnector(room.origin, dataDir, { SIDEVOICE_CURSOR_APP_PORT: port, SIDEVOICE_CONNECTOR_IDLE_MS: '20000' });
  await until(() => existsSync(socketPath));
  const child = spawn(process.execPath, [mcpPath], { env: { ...process.env, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CURSOR_APP_PORT: port }, stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = []; let out = ''; child.stdout.on('data', d => { out += d; let i; while ((i = out.indexOf('\n')) >= 0) { replies.push(JSON.parse(out.slice(0, i))); out = out.slice(i + 1); } });
  const ask = (id, method, params) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  let view = null;
  try {
    // What Cursor 3.22.12 declares: its name, and the MCP Apps extension.
    ask(1, 'initialize', { protocolVersion: '2025-11-25', clientInfo: { name: 'cursor-vscode', version: '1.0.0' },
      capabilities: { elicitation: { form: {} }, roots: { listChanged: false }, extensions: { 'io.modelcontextprotocol/ui': { mimeTypes: ['text/html;profile=mcp-app'] } } } });
    ask(2, 'tools/list', {}); ask(3, 'resources/read', { uri: 'ui://sidevoice/voice-link' });
    ask(4, 'tools/call', { name: 'voice_connect', arguments: { title: 'Editor' }, _meta: { progressToken: 'toolu_ed' } });
    await until(() => replies.length === 4, 8000);
    const byId = id => replies.find(r => r.id === id);
    assert.ok(byId(1).result.capabilities.resources, 'resources offered to a client that draws views');
    assert.deepEqual(byId(2).result.tools.find(t => t.name === 'voice_connect')._meta, { ui: { resourceUri: 'ui://sidevoice/voice-link' } });
    assert.equal(byId(2).result.tools.find(t => t.name === 'voice_say')._meta, undefined);
    const html = byId(3).result.contents[0];
    assert.equal(html.mimeType, 'text/html;profile=mcp-app');
    const joined = JSON.parse(byId(4).result.content[0].text);
    assert.match(joined.conversation, /^cursor-editor-/);
    assert.deepEqual(joined.experimental, ['deliver', 'sessionIdentity']);
    assert.equal(joined.delivery, 'push');
    assert.match(joined.view, /card/); assert.match(joined.watch_note, /not observed/);
    await until(() => room.sent('binding.register').length);
    assert.deepEqual(room.sent('binding.register')[0].experimental, ['deliver', 'sessionIdentity']);

    // Cursor draws the view in that chat and hands it the call's result.
    view = runView({ html: html.text, toolResult: byId(4).result });
    await wait(300);
    const delivered = await room.ask('input.deliver', { event_id: 'e-ed', binding_id: 'b-ed', channel: 'voice', session_id: 's', revision: 4, message_id: 'm-ed', text: 'hola editor' });
    assert.equal(delivered.status, 'accepted', JSON.stringify(delivered));
    assert.match(view.submitted[0].content[0].text, /hola editor/);
    assert.match(view.submitted[0].content[0].text, /"message_id":"m-ed"/);
  } finally { view?.stop(); child.kill(); if (connector.exitCode === null) connector.kill(); await room.close(); }
});

test('pairing: plaintext only where the token cannot leave the machine or the cluster', async () => {
  const { privateNetwork, pair } = await import('../pair.mjs');
  for (const ok of ['127.0.0.1', 'localhost', 'room.voice.svc', 'room.voice.svc.cluster.local', 'room.voice.svc.k8s.example']) assert.equal(privateNetwork(ok), true, ok);
  for (const no of ['sidevoice.dev.example.invalid', '10.0.0.5', 'sidevoice', 'svc.cluster.local', 'evil.svc.example.com.attacker.net']) assert.equal(privateNetwork(no), false, no);
  // Refused at pairing, before any code is spent: a plain http room on a name we cannot place.
  await assert.rejects(pair('http://sidevoice.example', 'ABCD1234', { SIDEVOICE_DATA_DIR: mkdtempSync(path.join(os.tmpdir(), 'sv-')) }), /must be https/);

  // A machine paired and never yet connected still reads as a machine: it says what it is here too.
  const asked = [];
  const rooms = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    asked.push({ url: req.url, body: JSON.parse(body) });
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ connector_id: 'c-9', token: 't-9', protocol: PROTOCOL }));
  });
  await new Promise(resolve => rooms.listen(0, '127.0.0.1', resolve));
  try {
    const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-'));
    await pair(`http://127.0.0.1:${rooms.address().port}`, 'ABCD1234', { SIDEVOICE_DATA_DIR: dataDir });
    assert.equal(asked[0].url, '/api/connectors/pair');
    assert.equal(asked[0].body.code, 'ABCD1234');
    assert.equal(asked[0].body.host, os.hostname());
    assert.match(asked[0].body.platform, new RegExp(`^(macOS|Windows|Linux|${os.platform()}) ${os.arch()}$`));
    assert.equal(asked[0].body.version, JSON.parse(readFileSync(path.join(here, '..', 'package.json'), 'utf8')).version);
    assert.ok(Array.isArray(asked[0].body.harnesses));
  } finally { rooms.close(); }
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
  assert.equal(userMessageText({ type: 'attachment', attachment: { type: 'queued_command', prompt: '{"channel":"voice"}\n\nmid-turn', commandMode: 'prompt' } }), '{"channel":"voice"}\n\nmid-turn',
    'a message fed to a running turn is recorded as a queued-command attachment, and it is the user');
  assert.equal(userMessageText({ type: 'attachment', attachment: { type: 'file', content: 'x' } }), null, 'other attachments are not the user');
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
  // Not from a checkout: the package is copied under the XDG data home and the harness runs that copy with node.
  // HOME too: Cursor's mcp.json lives under it, and a test must never touch the real one.
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), CLAUDE_CONFIG_DIR: path.join(home, '.claude'), SIDEVOICE_CLAUDE_BIN: bin,
                SIDEVOICE_INSTALL_FROM_SOURCE: '0', XDG_DATA_HOME: path.join(home, 'xdg') };
  mkdirSync(env.CLAUDE_CONFIG_DIR);
  mkdirSync(path.join(env.XDG_DATA_HOME, 'sidevoice', '0.0.1'), { recursive: true });   // a copy an older install left
  const { command, args } = (await import('../install.mjs')).serverCommand(env);
  const wanted = [command, ...args].join(' ');
  const version = JSON.parse(readFileSync(path.join(here, '..', 'package.json'), 'utf8')).version;
  const manifest = JSON.parse(readFileSync(path.join(here, '..', 'package.json'), 'utf8'));
  assert.equal(wanted, `node ${path.join(env.XDG_DATA_HOME, 'sidevoice', version, 'dist', 'cli.mjs')} mcp`, 'never npx at session start');
  assert.deepEqual(manifest.dependencies, undefined, 'the published package resolves nothing at install time');

  assert.rejects(install(['https://room.example', '--harness', 'claude'], env), /Pairing is not part of installing/, 'a room address is refused, with where pairing lives');

  const first = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice', `mcp add --scope user sidevoice -- ${wanted}`], 'nothing registered: it registers this version');
  assert.match(first.done.join('\n'), /Registered the MCP server/);
  assert.match(first.done.join('\n'), /Copied this version to .*\(removed: 0\.0\.1\)/);
  // The copy is what the package ships and nothing more: the bundle, the manifest beside it, and
  // no step of its own — nothing is fetched, built or resolved on the machine being installed on.
  // That the bundle then runs is proved where it is run for real, in the room's interop test.
  for (const file of manifest.files.concat('package.json')) {
    assert.ok(existsSync(path.join(env.XDG_DATA_HOME, 'sidevoice', version, file)),
      `${file} is in the copy (the bundle is built: npm run build -w @sidevoice/uplink)`);
  }
  assert.equal(existsSync(path.join(env.XDG_DATA_HOME, 'sidevoice', version, 'node_modules')), false);
  assert.ok(!existsSync(path.join(env.XDG_DATA_HOME, 'sidevoice', '0.0.1')), 'the older copy is gone');
  assert.match(first.done.join('\n'), /not paired with any room yet/);
  assert.match(first.next.join('\n'), /Emparejar máquina/, 'and it says the conversation will ask for the code');
  assert.ok(!existsSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room')), 'no skill is installed: the server carries the prompt');
  assert.ok(!existsSync(path.join(env.SIDEVOICE_DATA_DIR, 'credentials.json')), 'no pairing happened');

  // Registered at this version already: nothing to change, and it says so.
  writeFileSync(registered, `sidevoice:\n  Scope: User config (available in all your projects)\n  Type: stdio\n  Command: ${command}\n  Args: ${args.join(' ')}\n`);
  writeFileSync(log, '');
  const again = await install(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice']);
  assert.match(again.done.join('\n'), /already runs this version/);
  // A skill copy left by an earlier version is taken away; someone else's voice-room is not.
  mkdirSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room'), { recursive: true });
  writeFileSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room', 'SKILL.md'), '---\nname: voice-room\nmetadata:\n  sidevoice: installed copy\n---\nold');
  const cleaned = await install(['--harness', 'claude'], env);
  assert.match(cleaned.done.join('\n'), /Removed the voice-room skill copy/);
  assert.ok(!existsSync(path.join(env.CLAUDE_CONFIG_DIR, 'skills', 'voice-room')));

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
  assert.ok(!paired.next.join('\n').includes('Emparejar máquina'));

  // Uninstall is the reverse, for this machine: unregister, remove the copies and the credential, and say
  // what the room still remembers. The claude stand-in keeps answering "registered" so the removal is asked for.
  writeFileSync(registered, `sidevoice:\n  Scope: User config (available in all your projects)\n  Type: stdio\n  Command: ${command}\n  Args: ${args.join(' ')}\n`);
  writeFileSync(log, '');
  const { uninstall } = await import('../install.mjs');
  const gone = await uninstall(['--harness', 'claude'], env);
  assert.deepEqual(calls(), ['mcp get sidevoice', 'mcp remove --scope user sidevoice']);
  assert.ok(!existsSync(path.join(env.XDG_DATA_HOME, 'sidevoice')), 'installed copies are gone');
  assert.ok(!existsSync(env.SIDEVOICE_DATA_DIR), 'credential, socket and log are gone');
  assert.match(gone.next.join('\n'), /still lists this machine as paired .* revoke it under "Máquinas" on the room/);
  assert.match(gone.done.join('\n'), /Unregistered the MCP server/);

  // Codex is instructions, not edits: its configuration is machine-wide and not ours to rewrite.
  const codex = codexInstructions(env);
  assert.match(codex, /\[mcp_servers\.sidevoice\]/);
  assert.match(codex, /command = "node"/);
  assert.ok(!codex.includes('hooks'), 'nothing but the MCP server is asked of Codex');
  assert.match(codex, /does not rewrite it/);
});

test('skill: nothing is installed any more; a copy of ours is removed and a foreign one is never touched', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-skills-'));
  assert.equal(skillStatus(dir).state, 'absent');
  assert.equal(removeSkill(dir).action, 'nothing to remove');
  mkdirSync(path.join(dir, 'voice-room')); writeFileSync(path.join(dir, 'voice-room', 'SKILL.md'), '---\nname: voice-room\nmetadata:\n  sidevoice: installed copy\n---\nold');
  assert.equal(skillStatus(dir).state, 'installed');
  assert.equal(removeSkill(dir).action, 'removed'); assert.equal(skillStatus(dir).state, 'absent');
  mkdirSync(path.join(dir, 'voice-room')); writeFileSync(path.join(dir, 'voice-room', 'SKILL.md'), '---\nname: voice-room\n---\nsomeone else\'s');
  assert.equal(skillStatus(dir).state, 'foreign');
  assert.throws(() => removeSkill(dir), /not Sidevoice/);
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

test('harness modules: the model a conversation thinks with is observed where its harness records it — first seen, a change, and nothing on a repeat', async () => {
  const previous = { claude: process.env.CLAUDE_CONFIG_DIR, codex: process.env.CODEX_HOME, poll: process.env.SIDEVOICE_WORK_POLL_MS };
  process.env.SIDEVOICE_WORK_POLL_MS = '20';
  try {
    // Claude Code: every assistant entry of the transcript names the model that wrote it. The shape is
    // the real one, from `~/.claude/projects/<slug>/<session id>.jsonl` on Claude Code 2.1.278.
    const claudeHome = mkdtempSync(path.join(os.tmpdir(), 'sv-claude-'));
    mkdirSync(path.join(claudeHome, 'projects', '-home-someone-project'), { recursive: true });
    const transcript = path.join(claudeHome, 'projects', '-home-someone-project', 'sess-e.jsonl');
    writeFileSync(transcript, '');
    const assistant = model => appendFileSync(transcript, JSON.stringify({ parentUuid: 'u-0', isSidechain: false,
      message: { model, id: 'msg_1', type: 'message', role: 'assistant', content: [{ type: 'text', text: 'hola' }],
                 stop_reason: 'end_turn', usage: { input_tokens: 2, output_tokens: 9 } },
      requestId: 'req_1', type: 'assistant', uuid: 'u-1', timestamp: new Date().toISOString() }) + '\n');
    process.env.CLAUDE_CONFIG_DIR = claudeHome;
    const claude = await import('../harness-claude.mjs?' + Math.random());
    assert.equal(claude.assistantModel({ type: 'assistant', message: { model: 'claude-fable-5-1' } }), 'claude-fable-5-1');
    assert.equal(claude.assistantModel({ type: 'user', message: { model: 'claude-fable-5-1' } }), null, 'only an assistant entry says what answered');
    assert.equal(claude.assistantModel({ type: 'assistant', message: { role: 'assistant' } }), null);
    const said = [];
    const stop = claude.observe('sess-e', { userMessage() {}, working() {}, engine: seen => said.push(seen) });
    try {
      await wait(80);                                         // the watcher is in place before the session answers
      assistant('claude-fable-5-1');
      await until(() => said.length === 1);
      assert.deepEqual(said[0], { model: 'claude-fable-5-1', effort: null, thinking: null },
        'no launch flag to read, so effort and thinking are absent rather than guessed');
      assistant('claude-fable-5-1');
      assistant('claude-fable-5-1');
      await wait(120);
      assert.equal(said.length, 1, 'the same model, said again by the harness, is not news');
      assistant('claude-opus-5');
      await until(() => said.length === 2);
      assert.equal(said[1].model, 'claude-opus-5');
    } finally { stop(); }

    // Codex: the turn_context that opens every turn names the model. Shape from a real rollout on
    // Codex CLI 0.153.2 — its session_meta does not carry one, so the first turn is where it is seen.
    const codexHome = mkdtempSync(path.join(os.tmpdir(), 'sv-codex-'));
    const day = path.join(codexHome, 'sessions', '2026', '09', '22'); mkdirSync(day, { recursive: true });
    const rollout = path.join(day, 'rollout-2026-09-22T10-00-00-thread-e.jsonl');
    const line = (type, payload) => appendFileSync(rollout, JSON.stringify({ timestamp: new Date().toISOString(), ordinal: 0, type, payload }) + '\n');
    const context = (model, turn_id) => line('turn_context', { cwd: '/tmp/work', model, effort: 'medium', summary: 'auto',
      approval_policy: 'never', sandbox_policy: {}, turn_id });
    line('session_meta', { session_id: 'thread-e', id: 'thread-e', cwd: '/tmp/work', originator: 'test',
      cli_version: '0.153.2', source: 'vscode', model_provider: 'openai' });
    context('gpt-5.6-terra', 'turn-a');                       // already in the file when we start watching
    process.env.CODEX_HOME = codexHome;
    const codex = await import('../harness-codex.mjs?' + Math.random());
    assert.equal(codex.rolloutModel({ type: 'turn_context', payload: { model: 'gpt-5.6-terra' } }), 'gpt-5.6-terra');
    assert.equal(codex.rolloutModel({ type: 'session_meta', payload: { model: 'gpt-5.6-terra' } }), 'gpt-5.6-terra', 'session_meta says it when that build writes it');
    assert.equal(codex.rolloutModel({ type: 'session_meta', payload: { model_provider: 'openai' } }), null, 'a provider is not a model');
    assert.equal(codex.rolloutModel({ type: 'event_msg', payload: { type: 'task_started' } }), null);
    const heard = [];
    const stopCodex = codex.observe('thread-e', { userMessage() {}, working() {}, engine: seen => heard.push(seen) }, {});
    try {
      // What the rollout already held is read silently; the model it named is reported once the replay is over.
      await until(() => heard.length === 1);
      assert.deepEqual(heard[0], { model: 'gpt-5.6-terra', effort: null, thinking: null });
      context('gpt-5.6-terra', 'turn-b');
      await wait(120);
      assert.equal(heard.length, 1, 'every turn names the model; only a change is news');
      context('gpt-5.6-mini', 'turn-c');
      await until(() => heard.length === 2);
      assert.equal(heard[1].model, 'gpt-5.6-mini');
    } finally { stopCodex(); }
  } finally {
    for (const [name, value] of [['CLAUDE_CONFIG_DIR', previous.claude], ['CODEX_HOME', previous.codex], ['SIDEVOICE_WORK_POLL_MS', previous.poll]])
      if (value === undefined) delete process.env[name]; else process.env[name] = value;
  }
});
