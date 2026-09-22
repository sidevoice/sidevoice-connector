#!/usr/bin/env node
/** One connector per host: an outbound link to the room, every binding multiplexed over it, and
 *  the last mile chosen per binding. Node 22+. Façades talk to it over a local socket; a binding
 *  lives exactly as long as the façade connection that registered it.
 *
 *  What carries the link, and how it comes back when it drops, is `link.mjs`'s business; nothing
 *  below this line knows what is underneath. What does not live there is the outbox: a library's
 *  buffer dies with this process, and speech the user was promised must not. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, mkdirSync, openSync, closeSync, statSync, writeFileSync, readFileSync, unlinkSync, renameSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { capabilityState, SUPPORTED, voiceEnvelope } from './harness-contract.mjs';
import { harnessFor } from './harnesses.mjs';
import { machineIdentity, VERSION } from './identity.mjs';
import { privateNetwork, roomOrigin } from './pair.mjs';
import { roomLink, UNREACHABLE } from './link.mjs';

export const PROTOCOL = 2;
const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const lockPath = socketPath + '.lock';
const outboxPath = path.join(dataDir, 'outbox.json');
const credentialsPath = process.env.SIDEVOICE_CREDENTIALS || path.join(dataDir, 'credentials.json');
const idleMs = Number(process.env.SIDEVOICE_CONNECTOR_IDLE_MS || 15_000);
// Who this machine is, said at every connection: the room keeps the latest and lists it.
const identity = machineIdentity();
const hostId = identity.host;
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
  // The credential stores where the room is, not how to reach it: which path and namespace carry
  // the link is this package's knowledge and moves with its version.
  const room = roomOrigin(url);
  if (new URL(room).protocol !== 'https:' && !privateNetwork(new URL(room).hostname)) throw new Error('The room URL must be https:// unless it stays on this machine or inside its cluster');
  return { connector_id, token, room };
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
const clients = new Set();         // façade IPC connections
const closedByRoom = new Map();    // client_ref -> reason: the user closed that conversation's voice from the room
const readReported = new Set();    // message ids already reported as read, so a transcript read twice is harmless
let outbox = [];                   // speech not yet confirmed by the room
let link = null;                   // the one link to the room; it comes back on its own when it drops
let connected = false, closed = false, idleTimer = null, lastError = null;
let socketError = null;            // why the last attempt to reach the room failed, for whoever asks status
let refusal = null;                // the room will not have this connector, and no retry will change that
let waking = [];                   // whoever is waiting for the room to welcome this connector again
let creds;

function loadOutbox() { try { outbox = JSON.parse(readFileSync(outboxPath, 'utf8')); if (!Array.isArray(outbox)) outbox = []; } catch { outbox = []; } }
function saveOutbox() {
  const temporary = outboxPath + '.' + process.pid + '.tmp';
  writeFileSync(temporary, JSON.stringify(outbox), { mode: 0o600 }); renameSync(temporary, outboxPath);
}
/** Say it and move on. False when there is no room to say it to; the caller decides whether that
 *  matters, and what is worth saying again once the room comes back. */
function send(event, data) { return link?.connected ? link.send(event, data) : false; }
/** Ask, and wait for the room's answer to this and nothing else. */
function request(event, data, options) {
  if (!link?.connected) return Promise.reject(new Error(UNREACHABLE));
  return link.request(event, data, options);
}

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
  return send('input.working', { binding_id: binding.binding_id, working, ...extra });
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
        send('input.read', { binding_id: binding.binding_id, message_id: header.message_id, session_id: header.session_id, revision: header.revision, turn_id: turn_id || null });
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

/** The room will not have this connector, in its own words, and nothing this process does will
 *  change that: the pairing was taken away from the room's page. Every conversation it served is
 *  let go the way the room closing a channel lets one go — the next `voice_say` fails saying so —
 *  and the reason waits where status is read, instead of being a silence and a retry for ever. */
function refuse(reason) {
  if (refusal === reason) return;
  refusal = lastError = reason;
  log(`the room refused this connector and will not be asked again: ${reason} Pair this machine again with the code the room shows under "Emparejar máquina".`);
  for (const binding of [...bindings.values()]) {
    bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding); unwatch(binding);
    closedByRoom.set(binding.client_ref, 'connector_revoked');
  }
  for (const wake of waking.splice(0)) wake();   // nobody waits for a welcome that will not come
  scheduleExit();
}

/** Open the one link to this machine's room. Idempotent: it is called whenever a conversation
 *  joins, and a link that exists already — connected, or on its way back — is the answer. */
function open() {
  if (closed || link) return;
  link = roomLink({
    origin: creds.room, connector_id: creds.connector_id, token: creds.token,
    protocol: PROTOCOL, identity,
    onConnected: welcome => {
      connected = true; lastError = null; socketError = null;
      log(`connected to ${creds.room} as ${creds.connector_id} (protocol ${welcome.protocol ?? PROTOCOL}); ${bindings.size} binding(s) to re-register, ${outbox.length} queued speech`);
      // The room keeps no bindings across a restart and no outbox at all: both are said again.
      for (const binding of bindings.values()) enrol(binding).catch(error => log(`re-registering ${binding.thread} failed: ${error.message}`));
      for (const speech of [...outbox]) publish(speech).catch(() => {});
      const waiting = waking; waking = [];
      for (const wake of waiting) wake();
    },
    onEvent: asked,
    onLost: reason => {
      connected = false;
      socketError = { ...reason, at: new Date().toISOString() };
      // A refusal carries the room's own words; a socket the room merely closed does not. Only the
      // first is this credential's problem — the second is what a connector that lost its place to a
      // newer one of its own sees, and dropping that one's conversations would take a voice nobody
      // took away.
      if (reason.retrying === false && !closed && reason.error) refuse(reason.error);
      else if (reason.retrying === false && !closed) log(`the room closed this connection and will not be asked again: ${reason.close_reason}`);
      else if (reason.attempt <= 3 || reason.attempt % 10 === 0) log('room unreachable: ' + JSON.stringify(reason));
    },
  });
  link.open();
}

/** Tell the room about a binding. Said on joining and again after every reconnect, because the
 *  room mints the durable id and keeps no bindings across a restart. The promise is kept on the
 *  binding so that a façade waiting to join and the welcome that registers everything are waiting
 *  on the same registration, never making two. */
function enrol(binding) {
  binding.registration = announce(binding);
  return binding.registration;
}

/** The room's answer for one binding, however long it takes to be able to ask: a conversation that
 *  joins while the room is down is registered by the next welcome, and that is this call's answer too. */
async function joinRoom(binding, timeout = 10_000) {
  if (refusal) throw new Error(refusal);
  if (connected) return enrol(binding);
  const welcomed = await new Promise(resolve => {
    const timer = setTimeout(() => { waking = waking.filter(wake => wake !== wakeup); resolve(false); }, timeout);
    const wakeup = () => { clearTimeout(timer); resolve(true); };
    waking.push(wakeup);
  });
  // A room that is not there yet is worth waiting for; one that will not have this machine is not.
  if (refusal) throw new Error(refusal);
  if (!welcomed) throw new Error(UNREACHABLE);
  return binding.registration ?? enrol(binding);
}

async function announce(binding) {
  // `local-*` is only a connector-side placeholder while the first registration waits for the
  // room to mint its durable binding id. Sending it back makes the room correctly reject it as foreign.
  const frame = { client_ref: binding.client_ref, harness: binding.harness, thread: binding.thread,
    title: binding.title, inbound: binding.inbound, capabilities: binding.capabilities,
    engine: binding.engine, focus: false };
  if (!binding.binding_id.startsWith('local-')) frame.binding_id = binding.binding_id;
  const reply = await request('binding.register', frame).catch(error => {
    log(`room rejected ${binding.client_ref}: ${error.message}`);
    throw error;
  });
  if (binding.binding_id !== reply.binding_id) { bindings.delete(binding.binding_id); binding.binding_id = reply.binding_id; bindings.set(reply.binding_id, binding); }
  if (typeof binding.working === 'boolean') announceWork(binding, binding.working);
  log(`room registered ${binding.thread} as ${reply.binding_id} ("${binding.title || ''}")`);
  return reply;
}

/** Speech leaves the durable outbox only when the room says it has it — or says it never will. */
async function publish(speech) {
  const { type, ...frame } = speech;
  const reply = await request('speech.publish', frame, { timeout: 15_000 });
  log(`speech ${reply.utterance_id || speech.utterance_id} ${reply.status || 'published'}${reply.reason ? ' (' + reply.reason + ')' : ''}`);
  outbox = outbox.filter(queued => queued.event_id !== speech.event_id); saveOutbox();
  return reply;
}

/** What the room asks of this connector. What it returns is the answer, when the room waits for one. */
async function asked(route, frame) {
  switch (route) {
    case 'input.deliver': {
      const binding = bindings.get(frame.binding_id);
      if (!binding) return { status: 'unknown_binding' };
      // One delivery at a time per binding keeps the user's turns in order.
      const answer = (binding.chain || Promise.resolve()).then(() => handOver(binding, frame));
      binding.chain = answer.catch(() => {});
      return answer;
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
    case 'connector.revoked':
      // The person took this machine's pairing away from the room's page. The socket is about to
      // go and will not be welcomed back; the conversations learn it from their next call.
      refuse(frame.reason || 'The room revoked this machine\'s pairing.');
      return;
    case 'connector.error': lastError = frame.error; log('room says: ' + frame.error); return;
  }
}

/** One message into the conversation, through the adapter its binding was registered with. */
async function handOver(binding, frame) {
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
    return { status: outcome.status, detail: outcome.detail };
  } catch (error) {
    binding.pending?.delete(frame.message_id);
    log(`delivery of ${frame.event_id} to ${binding.thread} failed: ${error.message}`);
    return { status: 'failed', error: String(error.message || error).slice(0, 400) };
  }
}

function snapshot() {
  return { host: hostId, version: VERSION, room: creds.room, connected, protocol: PROTOCOL,
    outbox: outbox.length, room_error: lastError, socket_error: socketError, refused: refusal,
    // Which conversations lost their voice from the room, and why each one did: the same map read
    // twice, because "it is gone" and "this is what happened" are two different questions.
    closed_by_room: [...closedByRoom.keys()], closed_reasons: Object.fromEntries(closedByRoom),
    bindings: [...bindings.values()].map(({ binding_id, client_ref, harness, thread, title, delivery, capabilities }) =>
      ({ binding_id, client_ref, harness, thread, title, delivery: delivery.kind, capabilities })) };
}
function scheduleExit() {
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => { if (clients.size === 0 && bindings.size === 0) { log(`idle for ${idleMs} ms with no conversation; exiting`); shutdown(); } }, idleMs);
}
function shutdown() {
  if (!closed) log(`shutting down (${bindings.size} binding(s), ${clients.size} façade(s))`);
  closed = true; clearTimeout(idleTimer);
  try { link?.close(); } catch {}
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
      const binding = { binding_id: local_id, client_ref, harness, thread, title, delivery, inbound, capabilities, engine, owner: client };
      bindings.set(local_id, binding); client.bindings.add(binding); clearTimeout(idleTimer); open(); watch(binding);
      // A room that is not there yet is not a failure: the binding is registered on the next welcome.
      const reply = await joinRoom(binding).catch(error => { if (!connected && !refusal) return null; bindings.delete(binding.binding_id); client.bindings.delete(binding); unwatch(binding); throw error; });
      return { binding_id: reply?.binding_id || binding.binding_id, thread, connected, pending: !reply };
    }
    case 'publish': {
      const binding = bindings.get(params.binding_id) || [...bindings.values()].find(b => b.client_ref === params.client_ref);
      // Why it is gone travels with the refusal: a pairing revoked and a channel closed are not the
      // same news for the conversation, and only it can say the right one to the person.
      if (!binding) throw new Error(closedByRoom.has(params.client_ref) ? 'CLOSED_BY_ROOM:' + closedByRoom.get(params.client_ref) : 'Unknown binding');
      const speech = { event_id: params.event_id || randomUUID(), binding_id: binding.binding_id,
        session_id: params.session_id, revision: params.revision, utterance_id: params.utterance_id || randomUUID(), text: params.text, language: params.language };
      outbox.push(speech); saveOutbox();
      // What the room never confirmed stays in the outbox and goes again on the next welcome; the
      // conversation is told it is queued rather than left waiting on a room that is not there.
      const reply = await publish(speech).catch(() => null);
      if (!reply) return { status: 'queued', utterance_id: speech.utterance_id };
      const { type, event_id, ...result } = reply;
      return result;
    }
    case 'unregister': {
      // By the id the façade remembers, or by the conversation it speaks for: the room may have minted a
      // new id since (a reconnect re-registers), and a façade that only knew the old one "left" nothing —
      // the room kept listening to a conversation that believed it was gone (2026-09-21).
      const binding = bindings.get(params.binding_id) || [...bindings.values()].find(b => b.client_ref === params.client_ref);
      if (binding) { log(`${binding.thread} leaves`); bindings.delete(binding.binding_id); binding.owner?.bindings.delete(binding); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send('binding.unregister', { binding_id: binding.binding_id }); }
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
    for (const binding of client.bindings) { bindings.delete(binding.binding_id); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send('binding.unregister', { binding_id: binding.binding_id }); }
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
