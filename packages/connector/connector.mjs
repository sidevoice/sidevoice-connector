#!/usr/bin/env node
/** One connector per host: an outbound WebSocket to the room, every binding multiplexed over it,
 *  and the last mile chosen per binding. Node 22+, no dependencies. Façades talk to it over a
 *  local socket; a binding lives exactly as long as the façade connection that registered it. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, openSync, closeSync, writeFileSync, readFileSync, unlinkSync, renameSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { deliver } from './adapters.mjs';

export const PROTOCOL = 1;
const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const lockPath = socketPath + '.lock';
const outboxPath = path.join(dataDir, 'outbox.json');
const credentialsPath = process.env.SIDEVOICE_CREDENTIALS || path.join(dataDir, 'credentials.json');
const idleMs = Number(process.env.SIDEVOICE_CONNECTOR_IDLE_MS || 15_000);
const hostId = process.env.SIDEVOICE_HOST_ID || os.hostname();

function credentials() {
  let saved = {};
  try { saved = JSON.parse(readFileSync(credentialsPath, 'utf8')); } catch {}
  const url = process.env.SIDEVOICE_URL || saved.url;
  const connector_id = process.env.SIDEVOICE_CONNECTOR_ID || saved.connector_id;
  const token = process.env.SIDEVOICE_CONNECTOR_TOKEN || saved.token;
  if (!url || !connector_id || !token) throw new Error(`Not paired: run pair.mjs first (looked in ${credentialsPath})`);
  const parsed = new URL(url);
  const loopback = ['127.0.0.1', 'localhost', '::1'].includes(parsed.hostname);
  if (parsed.protocol !== 'wss:' && !loopback) throw new Error('The room URL must be wss:// unless it is loopback');
  return { url, connector_id, token };
}

function alive(pid) { try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; } }
function acquireLock() {
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  for (let attempt = 0; attempt < 2; attempt++) {
    try { const fd = openSync(lockPath, 'wx', 0o600); writeFileSync(fd, String(process.pid)); closeSync(fd); return true; }
    catch (error) {
      if (error.code !== 'EEXIST') throw error;
      let pid = 0; try { pid = Number(readFileSync(lockPath, 'utf8')); } catch {}
      if (pid && alive(pid)) return false;          // A live connector holds it: we are redundant.
      try { unlinkSync(lockPath); } catch {}         // Stale lock from a dead process.
    }
  }
  return false;
}

const bindings = new Map();        // binding_id -> { binding_id, client_ref, harness, thread, title, delivery, owner, chain }
const registering = new Map();     // client_ref -> { resolve, reject, timer }
const publishing = new Map();      // event_id -> { resolve, timer }
const clients = new Set();         // façade IPC connections
const closedByRoom = new Map();    // client_ref -> reason: the user closed that conversation's voice from the room
const readReported = new Set();    // message ids already reported as read, so a hook that fires twice is harmless
let outbox = [];                   // speech frames not yet confirmed by the room
let ws = null, connected = false, closed = false, reconnectTimer = null, idleTimer = null, reconnectAttempt = 0, lastError = null;
let creds;

function loadOutbox() { try { outbox = JSON.parse(readFileSync(outboxPath, 'utf8')); if (!Array.isArray(outbox)) outbox = []; } catch { outbox = []; } }
function saveOutbox() {
  const temporary = outboxPath + '.' + process.pid + '.tmp';
  writeFileSync(temporary, JSON.stringify(outbox), { mode: 0o600 }); renameSync(temporary, outboxPath);
}
function send(frame) { if (ws?.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(frame)); return true; } return false; }

function open() {
  if (closed || ws?.readyState === WebSocket.OPEN || ws?.readyState === WebSocket.CONNECTING) return;
  const socket = ws = new WebSocket(creds.url);
  socket.addEventListener('open', () => {
    send({ type: 'connector.hello', protocol: PROTOCOL, connector_id: creds.connector_id, token: creds.token, host: hostId });
  });
  socket.addEventListener('message', event => { receive(JSON.parse(String(event.data))).catch(error => send({ type: 'connector.error', error: error.message })); });
  const lost = () => { if (ws === socket) { ws = null; connected = false; } reconnect(); };
  // A refused connection surfaces as 'error' with no 'close', and the dead socket stays
  // CONNECTING forever: forget it, or open() would never make another one.
  socket.addEventListener('close', lost);
  socket.addEventListener('error', lost);
}
function reconnect() {
  if (closed || reconnectTimer) return;
  const delay = Math.min(10_000, 250 * 2 ** Math.min(reconnectAttempt++, 6));
  reconnectTimer = setTimeout(() => { reconnectTimer = null; open(); }, delay);
}

async function receive(frame) {
  switch (frame.type) {
    case 'connector.welcome':
      connected = true; reconnectAttempt = 0; lastError = null;
      for (const binding of bindings.values()) {
        // `local-*` is only a connector-side placeholder while the first
        // registration waits for the room to mint its durable binding id.
        // Sending it back makes the room correctly reject it as foreign.
        const frame = { type: 'binding.register', client_ref: binding.client_ref,
          harness: binding.harness, thread: binding.thread, title: binding.title,
          inbound: binding.inbound, focus: false };
        if (!binding.binding_id.startsWith('local-')) frame.binding_id = binding.binding_id;
        send(frame);
      }
      for (const speech of outbox) send(speech);
      return;
    case 'heartbeat': send({ type: 'heartbeat.ack', nonce: frame.nonce }); return;
    case 'binding.registered': {
      const binding = [...bindings.values()].find(b => b.client_ref === frame.client_ref);
      if (binding && binding.binding_id !== frame.binding_id) { bindings.delete(binding.binding_id); binding.binding_id = frame.binding_id; bindings.set(frame.binding_id, binding); }
      registering.get(frame.client_ref)?.resolve(frame); return;
    }
    case 'binding.rejected': registering.get(frame.client_ref)?.reject(new Error(frame.error || 'Binding rejected')); return;
    case 'speech.published': {
      outbox = outbox.filter(speech => speech.event_id !== frame.event_id); saveOutbox();
      publishing.get(frame.event_id)?.resolve(frame); return;
    }
    case 'input.deliver': {
      const binding = bindings.get(frame.binding_id);
      if (!binding) { send({ type: 'input.ack', event_id: frame.event_id, status: 'unknown_binding' }); return; }
      // One delivery at a time per binding keeps the user's turns in order.
      binding.chain = (binding.chain || Promise.resolve()).then(async () => {
        try {
          const outcome = await deliver(binding.delivery, frame);
          console.error(`[sidevoice] delivered ${frame.event_id} to ${binding.thread} via ${binding.delivery.kind}: ${outcome.status} (${outcome.detail})`);
          send({ type: 'input.ack', event_id: frame.event_id, status: outcome.status, detail: outcome.detail });
        } catch (error) {
          console.error(`[sidevoice] delivery of ${frame.event_id} failed: ${error.message}`);
          send({ type: 'input.ack', event_id: frame.event_id, status: 'failed', error: String(error.message || error).slice(0, 400) });
        }
      });
      return;
    }
    case 'binding.close': {
      // The user closed this conversation's voice in the room. Forget the binding; the façade learns it on its next call.
      const binding = bindings.get(frame.binding_id);
      if (!binding) return;
      bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding);
      closedByRoom.set(binding.client_ref, frame.reason || 'closed_from_room');
      console.error(`[sidevoice] room closed voice for ${binding.thread}`);
      scheduleExit(); return;
    }
    case 'connector.error': lastError = frame.error; console.error('[sidevoice] room: ' + frame.error); return;
  }
}

function snapshot() {
  return { host: hostId, connected, protocol: PROTOCOL, outbox: outbox.length, room_error: lastError, closed_by_room: [...closedByRoom.keys()],
    bindings: [...bindings.values()].map(({ binding_id, client_ref, harness, thread, title, delivery }) => ({ binding_id, client_ref, harness, thread, title, delivery: delivery.kind })) };
}
function scheduleExit() {
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => { if (clients.size === 0 && bindings.size === 0) shutdown(); }, idleMs);
}
function shutdown() {
  closed = true; clearTimeout(reconnectTimer); clearTimeout(idleTimer);
  try { ws?.close(); } catch {}
  server.close();
  try { if (Number(readFileSync(lockPath, 'utf8')) === process.pid) { unlinkSync(socketPath); unlinkSync(lockPath); } } catch {}
  process.exit(0);
}

async function command(client, input) {
  const params = input.params || {};
  switch (input.method) {
    case 'register': {
      const { client_ref, harness, thread, title, delivery, inbound } = params;
      if (!client_ref || !thread || !delivery?.kind) throw new Error('client_ref, thread and delivery are required');
      closedByRoom.delete(client_ref);   // joining again is the user's explicit request
      const existing = [...bindings.values()].find(b => b.client_ref === client_ref);
      if (existing) { existing.owner = client; existing.delivery = delivery; client.bindings.add(existing); return { binding_id: existing.binding_id, thread, connected }; }
      const local_id = 'local-' + randomUUID();
      const binding = { binding_id: local_id, client_ref, harness, thread, title, delivery, inbound, owner: client };
      bindings.set(local_id, binding); client.bindings.add(binding); clearTimeout(idleTimer); open();
      const frame = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => { registering.delete(client_ref); reject(new Error(connected ? 'The room did not confirm the binding' : 'The room is unreachable; retrying in the background')); }, 10_000);
        registering.set(client_ref, { resolve: f => { clearTimeout(timer); registering.delete(client_ref); resolve(f); }, reject: e => { clearTimeout(timer); registering.delete(client_ref); reject(e); } });
        if (!send({ type: 'binding.register', client_ref, harness, thread, title, inbound })) { /* sent on welcome */ }
      }).catch(error => { if (!connected) return null; bindings.delete(binding.binding_id); client.bindings.delete(binding); throw error; });
      return { binding_id: frame?.binding_id || binding.binding_id, thread, connected, pending: !frame };
    }
    case 'publish': {
      const binding = bindings.get(params.binding_id) || [...bindings.values()].find(b => b.client_ref === params.client_ref);
      if (!binding) throw new Error(closedByRoom.has(params.client_ref) ? 'CLOSED_BY_ROOM' : 'Unknown binding');
      const speech = { type: 'speech.publish', event_id: params.event_id || randomUUID(), binding_id: binding.binding_id,
        session_id: params.session_id, revision: params.revision, utterance_id: params.utterance_id || randomUUID(), text: params.text, language: params.language };
      outbox.push(speech); saveOutbox();
      if (!send(speech)) return { status: 'queued', utterance_id: speech.utterance_id };
      const reply = await new Promise(resolve => {
        const timer = setTimeout(() => { publishing.delete(speech.event_id); resolve(null); }, 15_000);
        publishing.set(speech.event_id, { resolve: f => { clearTimeout(timer); publishing.delete(speech.event_id); resolve(f); } });
      });
      if (!reply) return { status: 'queued', utterance_id: speech.utterance_id };
      const { type, event_id, ...result } = reply;
      return result;
    }
    case 'read': {
      // A harness hook says the conversation admitted this voice message: the room learns it was read.
      const { thread, message_id, session_id, revision, turn_id } = params;
      if (!thread || !message_id) throw new Error('thread and message_id are required');
      const binding = [...bindings.values()].find(b => b.thread === thread || b.client_ref === thread);
      if (!binding) return { status: 'no_binding' };
      if (readReported.has(message_id)) return { status: 'already_reported' };
      readReported.add(message_id); if (readReported.size > 512) readReported.delete(readReported.values().next().value);
      const sent = send({ type: 'input.read', binding_id: binding.binding_id, message_id, session_id, revision, turn_id: turn_id || null });
      return { status: sent ? 'sent' : 'offline' };
    }
    case 'unregister': {
      const binding = bindings.get(params.binding_id);
      if (binding) { bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding); if (!binding.binding_id.startsWith('local-')) send({ type: 'binding.unregister', binding_id: binding.binding_id }); }
      scheduleExit(); return snapshot();
    }
    case 'status': return snapshot();
    default: throw new Error('Unknown connector command');
  }
}

function serve(socket) {
  const client = { socket, bindings: new Set() };
  clients.add(client); clearTimeout(idleTimer);
  let buffer = '';
  socket.on('data', chunk => {
    buffer += chunk;
    if (buffer.length > 1 << 20) { socket.destroy(); return; }
    let index;
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
      if (!line.trim()) continue;
      let input; try { input = JSON.parse(line); } catch { socket.write(JSON.stringify({ ok: false, error: 'Invalid JSON' }) + '\n'); continue; }
      command(client, input).then(result => socket.write(JSON.stringify({ id: input.id, ok: true, result }) + '\n'))
        .catch(error => socket.write(JSON.stringify({ id: input.id, ok: false, error: error.message }) + '\n'));
    }
  });
  socket.on('error', () => {});
  socket.on('close', () => {
    clients.delete(client);
    // The façade is gone: so is every conversation it spoke for.
    for (const binding of client.bindings) { bindings.delete(binding.binding_id); if (!binding.binding_id.startsWith('local-')) send({ type: 'binding.unregister', binding_id: binding.binding_id }); }
    scheduleExit();
  });
}

creds = credentials();
if (!acquireLock()) process.exit(0);
loadOutbox();
try { unlinkSync(socketPath); } catch {}
const server = net.createServer(serve);
await new Promise((resolve, reject) => server.once('error', reject).listen(socketPath, resolve));
process.on('SIGTERM', shutdown); process.on('SIGINT', shutdown);
scheduleExit();
open();
