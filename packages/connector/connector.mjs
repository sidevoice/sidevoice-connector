#!/usr/bin/env node
/** One connector per host: an outbound WebSocket to the room, every binding multiplexed over it,
 *  and the last mile chosen per binding. Node 22+, no dependencies. Façades talk to it over a
 *  local socket; a binding lives exactly as long as the façade connection that registered it. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, mkdirSync, openSync, closeSync, statSync, writeFileSync, readFileSync, unlinkSync, renameSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { capabilityState, SUPPORTED, voiceEnvelope } from './harness-contract.mjs';
import { harnessFor } from './harnesses.mjs';
import { privateNetwork } from './pair.mjs';
import { fileURLToPath } from 'node:url';

const VERSION = JSON.parse(readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), 'package.json'), 'utf8')).version;

export const PROTOCOL = 1;
const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const lockPath = socketPath + '.lock';
const outboxPath = path.join(dataDir, 'outbox.json');
const credentialsPath = process.env.SIDEVOICE_CREDENTIALS || path.join(dataDir, 'credentials.json');
const idleMs = Number(process.env.SIDEVOICE_CONNECTOR_IDLE_MS || 15_000);
const hostId = process.env.SIDEVOICE_HOST_ID || os.hostname();
const logPath = process.env.SIDEVOICE_CONNECTOR_LOG || path.join(dataDir, 'connector.log');
const LOG_MAX = 1 << 20;

/** One line per event, to stderr and to `connector.log` in the data dir: the façade starts this process
 *  with its output discarded, so the file is the only record of a connector nobody ran by hand. Rolls
 *  over once, at 1 MB. */
function log(line) {
  const stamped = `${new Date().toISOString()} [sidevoice] ${line}`;
  console.error(stamped);
  try {
    let size = 0; try { size = statSync(logPath).size; } catch {}
    if (size > LOG_MAX) renameSync(logPath, logPath + '.1');
    appendFileSync(logPath, stamped + '\n', { mode: 0o600 });
  } catch {}
}

function credentials() {
  let saved = {};
  try { saved = JSON.parse(readFileSync(credentialsPath, 'utf8')); } catch {}
  const url = process.env.SIDEVOICE_URL || saved.url;
  const connector_id = process.env.SIDEVOICE_CONNECTOR_ID || saved.connector_id;
  const token = process.env.SIDEVOICE_CONNECTOR_TOKEN || saved.token;
  if (!url || !connector_id || !token) throw new Error(`Not paired with any room (looked in ${credentialsPath}): the conversation's voice_pair, or sidevoice pair <room-url> <code>, with the code the room shows`);
  const parsed = new URL(url);
  if (parsed.protocol !== 'wss:' && !privateNetwork(parsed.hostname)) throw new Error('The room URL must be wss:// unless it stays on this machine or inside its cluster');
  const room = new URL(url); room.protocol = room.protocol === 'wss:' ? 'https:' : 'http:';
  return { url, connector_id, token, room: room.origin };
}

/** Whether the pid in the lock is a live Sidevoice connector — not merely a live pid. Pids are reused,
 *  and on macOS a pid that now belongs to another user answers EPERM, which used to count as alive: a
 *  stale lock then made every new connector exit at once, silently (a laptop, 2026-09-21). */
function connectorAlive(pid) {
  try { process.kill(pid, 0); } catch (error) { if (error.code !== 'EPERM') return false; }
  try {
    const args = execFileSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8', timeout: 3000 }).trim();
    return /(^|[\s/])connector(\.mjs)?(\s|$)/.test(args);   // `…/connector.mjs` from a checkout, `sidevoice connector` from a package; not test_connector.mjs
  } catch { return true; }                          // No ps to ask: a live pid is taken at its word.
}
function acquireLock() {
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  for (let attempt = 0; attempt < 2; attempt++) {
    try { const fd = openSync(lockPath, 'wx', 0o600); writeFileSync(fd, String(process.pid)); closeSync(fd); return true; }
    catch (error) {
      if (error.code !== 'EEXIST') throw error;
      let pid = 0; try { pid = Number(readFileSync(lockPath, 'utf8')); } catch {}
      if (pid && connectorAlive(pid)) {              // A live connector holds it: we are redundant, and we say so.
        log(`a connector is already running (pid ${pid}, lock ${lockPath}); this one exits`);
        return false;
      }
      log(`stale lock ${lockPath} (pid ${pid || '?'} is not a connector); taking over`);
      try { unlinkSync(lockPath); } catch {}
    }
  }
  return false;
}

const bindings = new Map();        // binding_id -> { binding_id, client_ref, harness, thread, title, delivery, capabilities, owner, chain }
const registering = new Map();     // client_ref -> { resolve, reject, timer }
const publishing = new Map();      // event_id -> { resolve, timer }
const clients = new Set();         // façade IPC connections
const closedByRoom = new Map();    // client_ref -> reason: the user closed that conversation's voice from the room
const readReported = new Set();    // message ids already reported as read, so a transcript read twice is harmless
let outbox = [];                   // speech frames not yet confirmed by the room
let ws = null, connected = false, closed = false, reconnectTimer = null, idleTimer = null, reconnectAttempt = 0, lastError = null;
let socketError = null;            // why the last attempt to reach the room failed, for whoever asks status
let creds;

function loadOutbox() { try { outbox = JSON.parse(readFileSync(outboxPath, 'utf8')); if (!Array.isArray(outbox)) outbox = []; } catch { outbox = []; } }
function saveOutbox() {
  const temporary = outboxPath + '.' + process.pid + '.tmp';
  writeFileSync(temporary, JSON.stringify(outbox), { mode: 0o600 }); renameSync(temporary, outboxPath);
}
function send(frame) { if (ws?.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(frame)); return true; } return false; }

/* Whether a conversation is working, and whether it has read what the room sent, are the harness's own
 * state — and every harness writes that state down somewhere of its own: Claude Code in a session registry
 * and a transcript, Codex in the thread's rollout. Each module watches what its harness writes (`observe`)
 * and calls back; nothing is installed in the harness for it. Unknown or unsupported is skipped rather
 * than rendered as false.
 *
 * The connector does not assume the room still knows what it was told. A transition is sent the moment it
 * is seen, and the same state is said again every few seconds regardless: a room that restarted, or a
 * socket that dropped, learns what is true within one interval instead of waiting for the next change that
 * may never come (a conversation that was already working when the room came back showed nothing at all,
 * 2026-09-20). Saying it again is one small frame; not saying it is a light that never comes on. */
const WORK_ANNOUNCE_MS = Number(process.env.SIDEVOICE_WORK_ANNOUNCE_MS || 2000);
const PENDING_MAX = 64;
let announceTimer = null;
function announceWork(binding, working, extra = {}) {
  binding.working = working;
  binding.workingSentAt = Date.now();
  return send({ type: 'input.working', binding_id: binding.binding_id, working, ...extra });
}
function keepAnnouncing() {
  if (announceTimer) return;
  announceTimer = setInterval(() => {
    if (!bindings.size) { clearInterval(announceTimer); announceTimer = null; return; }
    for (const binding of bindings.values()) {
      if (typeof binding.working !== 'boolean') continue;
      if (Date.now() - (binding.workingSentAt || 0) >= WORK_ANNOUNCE_MS) announceWork(binding, binding.working);
    }
  }, Math.max(50, WORK_ANNOUNCE_MS / 4));
  announceTimer.unref?.();
}
/** Start watching a binding's conversation through its harness. What we delivered and it has not yet taken
 *  waits in `pending`; the moment its transcript shows the message, the room gets the second tick and a
 *  correlated start of turn; the end of that turn carries the same correlation. */
function watch(binding) {
  const harness = harnessFor(binding.harness);
  if (binding.stop || capabilityState(harness, 'working') !== SUPPORTED || typeof harness.observe !== 'function') return;
  binding.pending ||= new Map();
  const correlation = () => binding.turn ? { turn_id: binding.turn.turn_id, session_id: binding.turn.session_id, revision: binding.turn.revision } : {};
  binding.stop = harness.observe(binding.thread, {
    userMessage({ text, turn_id }) {
      const header = voiceEnvelope(text);
      if (!header || !binding.pending.has(header.message_id)) return;
      binding.pending.delete(header.message_id);
      if (!readReported.has(header.message_id)) {
        readReported.add(header.message_id); if (readReported.size > 512) readReported.delete(readReported.values().next().value);
        send({ type: 'input.read', binding_id: binding.binding_id, message_id: header.message_id, session_id: header.session_id, revision: header.revision, turn_id: turn_id || null });
        log(`${binding.thread} read ${header.message_id} (session ${header.session_id} rev ${header.revision}, turn ${turn_id || '?'})`);
      }
      if (header.channel !== 'voice') return;
      binding.turn = { turn_id: turn_id || null, session_id: header.session_id, revision: header.revision };
      announceWork(binding, true, turn_id ? { turn_phase: 'start', ...correlation() } : {});
    },
    working(working, { turn_id } = {}) {
      if (working) {
        // A start we can name is said with its name; the correlated start, if any, comes with the message itself.
        if (binding.turn && turn_id && binding.turn.turn_id === turn_id) return announceWork(binding, true, { turn_phase: 'start', ...correlation() });
        return announceWork(binding, true, {});
      }
      const ours = binding.turn && (!turn_id || !binding.turn.turn_id || binding.turn.turn_id === turn_id);
      if (ours) { const extra = binding.turn.turn_id ? { turn_phase: 'end', ...correlation() } : {}; binding.turn = null; return announceWork(binding, false, extra); }
      if (turn_id && binding.turn) return;     // some other turn ended: ours is still running
      return announceWork(binding, false, {});
    },
  });
  keepAnnouncing();
}
function unwatch(binding) { try { binding.stop?.(); } catch {} binding.stop = null; }

function open() {
  if (closed || ws?.readyState === WebSocket.OPEN || ws?.readyState === WebSocket.CONNECTING) return;
  const socket = ws = new WebSocket(creds.url);
  socket.addEventListener('open', () => {
    send({ type: 'connector.hello', protocol: PROTOCOL, connector_id: creds.connector_id, token: creds.token, host: hostId });
  });
  socket.addEventListener('message', event => { receive(JSON.parse(String(event.data))).catch(error => send({ type: 'connector.error', error: error.message })); });
  const lost = why => { if (ws === socket) { ws = null; connected = false; } if (why) { socketError = { ...why, at: new Date().toISOString(), attempt: reconnectAttempt }; log('room unreachable: ' + JSON.stringify(why)); } reconnect(); };
  // A refused connection surfaces as 'error' with no 'close', and the dead socket stays
  // CONNECTING forever: forget it, or open() would never make another one. Whatever the runtime
  // says about it is kept: "not reachable" alone told a person nothing (2026-09-21).
  socket.addEventListener('close', event => lost(connected ? null : { close_code: event.code, reason: event.reason || null }));
  socket.addEventListener('error', event => lost({ error: event.error?.message || event.message || 'connection failed' }));
}
function reconnect() {
  if (closed || reconnectTimer) return;
  const delay = Math.min(10_000, 250 * 2 ** Math.min(reconnectAttempt++, 6));
  if (reconnectAttempt <= 3 || reconnectAttempt % 10 === 0) log(`room not connected; retrying in ${delay} ms (attempt ${reconnectAttempt})`);
  reconnectTimer = setTimeout(() => { reconnectTimer = null; open(); }, delay);
}

async function receive(frame) {
  switch (frame.type) {
    case 'connector.welcome':
      connected = true; reconnectAttempt = 0; lastError = null; socketError = null;
      log(`connected to ${creds.room} as ${creds.connector_id} (protocol ${frame.protocol ?? PROTOCOL}); ${bindings.size} binding(s) to re-register, ${outbox.length} queued speech`);
      for (const binding of bindings.values()) {
        // `local-*` is only a connector-side placeholder while the first
        // registration waits for the room to mint its durable binding id.
        // Sending it back makes the room correctly reject it as foreign.
        const frame = { type: 'binding.register', client_ref: binding.client_ref,
          harness: binding.harness, thread: binding.thread, title: binding.title,
          inbound: binding.inbound, capabilities: binding.capabilities, focus: false };
        if (!binding.binding_id.startsWith('local-')) frame.binding_id = binding.binding_id;
        send(frame);
      }
      for (const speech of outbox) send(speech);
      return;
    case 'heartbeat': send({ type: 'heartbeat.ack', nonce: frame.nonce }); return;
    case 'binding.registered': {
      const binding = [...bindings.values()].find(b => b.client_ref === frame.client_ref);
      if (binding && binding.binding_id !== frame.binding_id) { bindings.delete(binding.binding_id); binding.binding_id = frame.binding_id; bindings.set(frame.binding_id, binding); }
      if (binding && typeof binding.working === 'boolean') announceWork(binding, binding.working);
      if (binding) log(`room registered ${binding.thread} as ${frame.binding_id} ("${binding.title || ''}")`);
      registering.get(frame.client_ref)?.resolve(frame); return;
    }
    case 'binding.rejected': log(`room rejected ${frame.client_ref}: ${frame.error || 'no reason'}`); registering.get(frame.client_ref)?.reject(new Error(frame.error || 'Binding rejected')); return;
    case 'speech.published': {
      log(`speech ${frame.utterance_id || frame.event_id} ${frame.status || 'published'}${frame.reason ? ' (' + frame.reason + ')' : ''}`);
      outbox = outbox.filter(speech => speech.event_id !== frame.event_id); saveOutbox();
      publishing.get(frame.event_id)?.resolve(frame); return;
    }
    case 'input.deliver': {
      const binding = bindings.get(frame.binding_id);
      if (!binding) { send({ type: 'input.ack', event_id: frame.event_id, status: 'unknown_binding' }); return; }
      // One delivery at a time per binding keeps the user's turns in order.
      binding.chain = (binding.chain || Promise.resolve()).then(async () => {
        // Expected before it is sent: the harness can take the message, and its transcript show it, before the
        // delivery call has even settled (Claude Code admitted one 9 ms after the write; the socket answered
        // 1.5 s later, 2026-09-21). A message expected and never taken costs a map entry.
        if (binding.pending && frame.message_id) {
          binding.pending.set(frame.message_id, { session_id: frame.session_id, revision: frame.revision, at: Date.now() });
          while (binding.pending.size > PENDING_MAX) binding.pending.delete(binding.pending.keys().next().value);
        }
        try {
          const harness = harnessFor(binding.harness);
          const outcome = await harness.deliver(binding.delivery, frame);
          log(`delivered ${frame.event_id} (${frame.message_id}) to ${binding.thread} via ${binding.delivery.kind}: ${outcome.status} (${outcome.detail})`);
          send({ type: 'input.ack', event_id: frame.event_id, status: outcome.status, detail: outcome.detail });
        } catch (error) {
          binding.pending?.delete(frame.message_id);
          log(`delivery of ${frame.event_id} to ${binding.thread} failed: ${error.message}`);
          send({ type: 'input.ack', event_id: frame.event_id, status: 'failed', error: String(error.message || error).slice(0, 400) });
        }
      });
      return;
    }
    case 'binding.close': {
      // The user closed this conversation's voice in the room. Forget the binding; the façade learns it on its next call.
      const binding = bindings.get(frame.binding_id);
      if (!binding) return;
      bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding); unwatch(binding);
      closedByRoom.set(binding.client_ref, frame.reason || 'closed_from_room');
      log(`room closed voice for ${binding.thread} (${frame.reason || 'closed_from_room'})`);
      scheduleExit(); return;
    }
    case 'connector.error': lastError = frame.error; log('room says: ' + frame.error); return;
  }
}

function snapshot() {
  return { host: hostId, version: VERSION, room: creds.room, connected, protocol: PROTOCOL, outbox: outbox.length, room_error: lastError, socket_error: socketError, closed_by_room: [...closedByRoom.keys()],
    bindings: [...bindings.values()].map(({ binding_id, client_ref, harness, thread, title, delivery, capabilities }) =>
      ({ binding_id, client_ref, harness, thread, title, delivery: delivery.kind, capabilities })) };
}
function scheduleExit() {
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => { if (clients.size === 0 && bindings.size === 0) { log(`idle for ${idleMs} ms with no conversation; exiting`); shutdown(); } }, idleMs);
}
function shutdown() {
  if (!closed) log(`shutting down (${bindings.size} binding(s), ${clients.size} façade(s))`);
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
      const { client_ref, harness, thread, title, delivery, inbound, capabilities, engine } = params;
      if (!client_ref || !thread || !delivery?.kind) throw new Error('client_ref, thread and delivery are required');
      closedByRoom.delete(client_ref);   // joining again is the user's explicit request
      const existing = [...bindings.values()].find(b => b.client_ref === client_ref);
      if (existing) {
        Object.assign(existing, { owner: client, delivery, inbound, capabilities });
        client.bindings.add(existing);
        return { binding_id: existing.binding_id, thread, connected };
      }
      const local_id = 'local-' + randomUUID();
      log(`${harness} ${thread} joins ("${title || ''}", delivery ${delivery.kind}, inbound ${inbound ? (inbound.ok ? 'ok' : 'held') : 'n/a'})`);
      const binding = { binding_id: local_id, client_ref, harness, thread, title, delivery, inbound, capabilities, owner: client };
      bindings.set(local_id, binding); client.bindings.add(binding); clearTimeout(idleTimer); open(); watch(binding);
      const frame = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => { registering.delete(client_ref); reject(new Error(connected ? 'The room did not confirm the binding' : 'The room is unreachable; retrying in the background')); }, 10_000);
        registering.set(client_ref, { resolve: f => { clearTimeout(timer); registering.delete(client_ref); resolve(f); }, reject: e => { clearTimeout(timer); registering.delete(client_ref); reject(e); } });
        if (!send({ type: 'binding.register', client_ref, harness, thread, title, inbound, capabilities, engine })) { /* sent on welcome */ }
      }).catch(error => { if (!connected) return null; bindings.delete(binding.binding_id); client.bindings.delete(binding); unwatch(binding); throw error; });
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
    case 'unregister': {
      // By the id the façade remembers, or by the conversation it speaks for: the room may have minted a
      // new id since (a reconnect re-registers), and a façade that only knew the old one "left" nothing —
      // the room kept listening to a conversation that believed it was gone (2026-09-21).
      const binding = bindings.get(params.binding_id) || [...bindings.values()].find(b => b.client_ref === params.client_ref);
      if (binding) { log(`${binding.thread} leaves`); bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send({ type: 'binding.unregister', binding_id: binding.binding_id }); }
      else log(`unregister for ${params.client_ref || params.binding_id || '?'} matched no binding`);
      scheduleExit(); return { ...snapshot(), left: !!binding };
    }
    case 'status': return snapshot();
    default: throw new Error('Unknown connector command');
  }
}

function serve(socket) {
  const client = { socket, bindings: new Set() };
  clients.add(client); clearTimeout(idleTimer);
  log(`façade attached (${clients.size} now)`);
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
    log(`façade detached (${clients.size} left); dropping ${client.bindings.size} binding(s)`);
    // The façade is gone: so is every conversation it spoke for.
    for (const binding of client.bindings) { bindings.delete(binding.binding_id); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send({ type: 'binding.unregister', binding_id: binding.binding_id }); }
    scheduleExit();
  });
}

creds = credentials();
if (!acquireLock()) process.exit(0);
log(`connector ${VERSION} starting: pid ${process.pid}, host ${hostId}, room ${creds.room}, socket ${socketPath}, log ${logPath}`);
loadOutbox();
if (outbox.length) log(`${outbox.length} speech frame(s) waiting in the outbox`);
try { unlinkSync(socketPath); } catch {}
const server = net.createServer(serve);
await new Promise((resolve, reject) => server.once('error', reject).listen(socketPath, resolve));
process.on('SIGTERM', shutdown); process.on('SIGINT', shutdown);
scheduleExit();
open();
