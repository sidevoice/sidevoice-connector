#!/usr/bin/env node
/** One ephemeral, multiplexed outbound connector per host. Node 22+, no deps. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdir, rm } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';

const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const controlUrl = process.env.SIDEVOICE_CONTROL_URL;
const token = process.env.SIDEVOICE_CONNECTOR_TOKEN;
const hostId = process.env.SIDEVOICE_HOST_ID || createHash('sha256').update(os.hostname()).digest('hex').slice(0, 24);
const idleMs = Number(process.env.SIDEVOICE_CONNECTOR_IDLE_MS || 15_000);
if (!controlUrl || !token) throw new Error('SIDEVOICE_CONTROL_URL and SIDEVOICE_CONNECTOR_TOKEN are required');

const bindings = new Map();
let ws, reconnectTimer, idleTimer, closed = false;
let reconnectAttempt = 0;

function send(message) {
  if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message));
}
function announceBindings() {
  send({ type: 'connector.hello', host_id: hostId, token, connector_id: connectorId });
  for (const binding of bindings.values()) send({ type: 'binding.register', ...binding });
}
const connectorId = randomUUID();
function reconnect() {
  if (closed || reconnectTimer) return;
  const delay = Math.min(10_000, 250 * 2 ** Math.min(reconnectAttempt++, 6));
  reconnectTimer = setTimeout(() => { reconnectTimer = null; open(); }, delay);
}
function open() {
  if (closed || ws?.readyState === WebSocket.OPEN || ws?.readyState === WebSocket.CONNECTING) return;
  ws = new WebSocket(controlUrl);
  ws.addEventListener('open', () => { reconnectAttempt = 0; announceBindings(); });
  ws.addEventListener('message', event => receive(JSON.parse(String(event.data))).catch(error => {
    send({ type: 'connector.error', error: error.message });
  }));
  ws.addEventListener('close', reconnect);
  ws.addEventListener('error', () => {});
}
async function receive(message) {
  if (message.type === 'heartbeat') { send({ type: 'heartbeat.ack', nonce: message.nonce }); return; }
  if (message.type !== 'input.deliver') return;
  const binding = bindings.get(message.binding_id);
  if (!binding) { send({ type: 'input.ack', event_id: message.event_id, status: 'unknown_binding' }); return; }
  const response = await fetch(binding.delivery_url, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ thread_id: binding.conversation_id, text: message.text,
      message_id: message.event_id, session_id: message.session_id, revision: message.revision,
      channel: 'voice' }), signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) throw new Error(`Harness delivery failed (${response.status})`);
  send({ type: 'input.ack', event_id: message.event_id, status: 'accepted' });
}
function scheduleExit() {
  if (bindings.size || idleTimer) return;
  idleTimer = setTimeout(() => shutdown(), idleMs);
}
function snapshot() { return { host_id: hostId, connected: ws?.readyState === WebSocket.OPEN, bindings: [...bindings.values()] }; }
function shutdown() {
  closed = true; clearTimeout(reconnectTimer); clearTimeout(idleTimer); ws?.close(); server.close();
}
async function command(input) {
  switch (input.method) {
    case 'register': {
      const binding = input.binding;
      if (!binding?.binding_id || !binding.room_id || !binding.conversation_id || !binding.delivery_url) throw new Error('Invalid binding');
      clearTimeout(idleTimer); idleTimer = null; bindings.set(binding.binding_id, binding); open(); send({ type: 'binding.register', ...binding }); return snapshot();
    }
    case 'unregister': bindings.delete(input.binding_id); send({ type: 'binding.unregister', binding_id: input.binding_id }); scheduleExit(); return snapshot();
    case 'publish': {
      const binding = bindings.get(input.binding_id);
      if (!binding) throw new Error('Unknown binding');
      send({ type: 'speech.publish', binding_id: input.binding_id, event_id: input.event_id || randomUUID(), text: input.text, language: input.language });
      return { status: ws?.readyState === WebSocket.OPEN ? 'sent' : 'queued_for_reconnect' };
    }
    case 'status': return snapshot();
    default: throw new Error('Unknown connector command');
  }
}
function serve(socket) {
  let buffer = '';
  socket.on('data', chunk => {
    buffer += chunk;
    for (;;) {
      const index = buffer.indexOf('\n'); if (index < 0) break;
      const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
      Promise.resolve().then(() => command(JSON.parse(line))).then(result => socket.write(JSON.stringify({ ok: true, result }) + '\n'))
        .catch(error => socket.write(JSON.stringify({ ok: false, error: error.message }) + '\n'));
    }
  });
}
await mkdir(dataDir, { recursive: true, mode: 0o700 });
if (process.platform !== 'win32') await rm(socketPath, { force: true });
const server = net.createServer(serve);
await new Promise((resolve, reject) => server.once('error', reject).listen(socketPath, resolve));
process.on('SIGTERM', shutdown); process.on('SIGINT', shutdown); open();
