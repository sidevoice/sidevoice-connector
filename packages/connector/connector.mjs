/** One connector per host: a link to this machine's core, every binding multiplexed over it, and
 *  the last mile chosen per binding. Node 22+. Façades talk to it over a local socket; a binding
 *  lives exactly as long as the façade connection that registered it.
 *
 *  The core (`sidevoice-core`, Python) holds the conversations and runs voice; this connector installs
 *  it, starts it and links to it over the core's own Unix socket (`core.mjs`, `core-socket.mjs`). The
 *  hosted room is the core's business: the core dials it with this machine's pairing and tells this
 *  connector how that link is doing.
 *
 *  Two ways to run (§4.2). `connector --supervise` is the node service: started by launchd or systemd at
 *  login (or detached where there is no user service manager), it runs the core as its child, never leaves
 *  on its own, and answers `node.status`/`node.ensure`/`node.restart`. Plain `connector` is what the
 *  launcher starts where no service is installed: the core detached, and gone itself 15 s after its last
 *  conversation. A supervisor that finds a plain connector holding the socket takes over from it
 *  (`handover`): every binding moves, with everything `register` was told, and the room never sees one leave.
 *
 *  What carries the link, and how it comes back when it drops, is `link.mjs`'s business; nothing
 *  below this line knows what is underneath. What does not live there is the outbox: a library's
 *  buffer dies with this process, and speech the user was promised must not. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, chmodSync, existsSync, lstatSync, mkdirSync, rmSync, statSync, writeFileSync, readFileSync, unlinkSync, renameSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { capabilityState, SUPPORTED, voiceEnvelope } from './harness-contract.mjs';
import { harnessFor } from './harnesses.mjs';
import { machineIdentity, VERSION } from './identity.mjs';
import { pair, roomOrigin } from './pair.mjs';
import { roomLink, UNREACHABLE } from './link.mjs';
import { API_RANGE, CORE_VERSION, NO_UV, awaitReady, coreArgs, coreRunning, runsSelected, coreData, ensureInstalled, ensureRunning, examineRunning, failureCause,
  failurePath, installInProgress, roomCredentialPath, spawnCore, takeInstallLock, terminateCore, unlinkSocket } from './core.mjs';
import { ensureCoreDirectory, localHealth, socketAgent } from './core-socket.mjs';
import { appendLine } from './logfile.mjs';
import { Supervisor } from './supervisor.mjs';
import { pruneInstallations, recover, sameCommand, settle } from './install-txn.mjs';
import { connectorSocketOf, dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';
import { readLock, releaseLock as giveUp, stillHeld, tryLock } from './lockfile.mjs';
import { isProcess, signalVerified } from './proc.mjs';
import { verifyPrivateDir } from './secure-fs.mjs';

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

export const PROTOCOL = 2;
// Set by `run`: nothing here reads the environment, or does anything, on import.
let env = process.env, dataDir, socketPath, lockPath, outboxPath, credentialsPath, idleMs, identity, hostId, logPath, files;
/** `--supervise`: this process is the node service, and the core is its child. */
let supervised = false, supervisor = null, serviceKind = 'none';
const LOG_MAX = 1 << 20;

/** One line per event. A plain connector writes to stderr and to `connector.log` in the data dir: the façade
 *  starts it with its output discarded, so the file is the only record of a connector nobody ran by hand
 *  (rolls over once, at 1 MB). The supervisor writes `node-service.log` only — the service manager already
 *  puts its stderr in that same file, so writing both would say everything twice. */
/** An editor conversation's id is what lets a chat speak as it: the log names it by a hash. */
const redact = line => String(line).replace(/cursor-editor-[0-9a-f-]{36}/g, id => 'cursor-editor-h:' + createHash('sha256').update(id).digest('hex').slice(0, 10));
function log(line) {
  line = redact(line);
  const stamped = `${new Date().toISOString()} [sidevoice] ${line}`;
  if (supervised) return appendLine(logPath, stamped);
  console.error(stamped);
  try {
    let size = 0; try { size = statSync(logPath).size; } catch {}
    if (size > LOG_MAX) renameSync(logPath, logPath + '.1');
    appendFileSync(logPath, stamped + '\n', { mode: 0o600 });
  } catch {}
}

/** A core somebody else runs — a checkout's, a test's — named whole in the environment: this connector
 *  then links to it over TCP and supervises nothing. Otherwise null, and the connector runs its own. */
function externalCore() {
  const { SIDEVOICE_URL: url, SIDEVOICE_CONNECTOR_ID: connector_id, SIDEVOICE_CONNECTOR_TOKEN: token } = env;
  return url && connector_id && token ? { room: roomOrigin(url), connector_id, token } : null;
}

/** Where the link to this machine's own core goes: its socket, whatever the URL says (`core-socket.mjs`). */
function coreLink(ready) {
  return { room: 'http://localhost', connector_id: ready.connector_id, token: ready.token, core: ready, agent: socketAgent(ready.socket) };
}

/** The singleton lock (`lockfile.mjs`): taken atomically, taken over only from an owner proven gone. Its record —
 *  pid and process start time — is what identifies the connector holding the socket; a pid alone never does. */
let lockRecord = null;
function acquireLock() {
  const taken = tryLock(lockPath, { kind: 'connector' });
  if (taken.held) {
    lockRecord = taken.record;
    if (taken.reclaimed) log(`stale lock ${lockPath} (pid ${taken.reclaimed.pid} is gone, or is another process now); taking over`);
    return true;
  }
  if (!supervised) log(`a connector is already running (pid ${taken.owner?.pid ?? '?'}, lock ${lockPath}); this one exits`);
  return false;
}
/** Whether the connector a lock record names is still that process. */
const connectorAlive = owner => !!owner && !owner.unreadable && isProcess(owner.pid, { start: owner.start ?? null });

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
let creds = null;                  // where the link to the core goes, and with which credential
let rendezvous = null;             // the core's link with the hosted room, as the core last reported it
let external = null;               // a core somebody else runs, named in the environment
let coreStarting = null;           // the core being installed or started, while it is (plain connector)
let coreError = null;              // why the local core could not be had, for whoever asks
let coreFailure = null;            // the same, as the keyed failure `node.status` carries
let coreCheckedAt = 0;
let handingOver = false;           // a supervisor is taking over: no new command is accepted
const inFlight = new Set();        // commands being answered, which a handover lets settle
const deliveries = new Set();      // voice turns being delivered to a harness, which a handover lets settle too

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
/** Which conversation each delivered voice turn went to, by the (session_id, revision) it carried: one
 *  façade can speak for several conversations (Cursor's editor, one MCP process per window), and a
 *  voice_say names the turn it answers, never the conversation. */
const turnsDelivered = new Map();
const TURNS_MAX = 512;
const turnKey = (session_id, revision) => `${session_id}\n${revision}`;
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
/** The second tick: the conversation took this message. Said once per message, however many signals say it. */
function reportRead(binding, header, turn_id = null) {
  binding.pending?.delete(header.message_id);
  if (readReported.has(header.message_id)) return;
  readReported.add(header.message_id); if (readReported.size > 512) readReported.delete(readReported.values().next().value);
  send('input.read', { binding_id: binding.binding_id, message_id: header.message_id, session_id: header.session_id, revision: header.revision, turn_id: turn_id || null });
  log(`${binding.thread} read ${header.message_id} (session ${header.session_id} rev ${header.revision}, turn ${turn_id || '?'})`);
}
function watch(binding) {
  const harness = harnessFor(binding.harness);
  // A conversation that declared it cannot be watched is not, though its harness can watch others.
  if (binding.stop || capabilityState(harness, 'working') !== SUPPORTED || capabilityState(binding, 'working') === 'unsupported' || typeof harness.observe !== 'function') return;
  binding.pending ||= new Map();
  const correlation = () => binding.turn ? { turn_id: binding.turn.turn_id, session_id: binding.turn.session_id, revision: binding.turn.revision } : {};
  binding.stop = harness.observe(binding.thread, {
    userMessage({ text, turn_id }) {
      const header = voiceEnvelope(text);
      if (!header || !binding.pending.has(header.message_id)) return;
      reportRead(binding, header, turn_id);
      if (header.channel !== 'voice') return;
      binding.turn = { turn_id: turn_id || null, session_id: header.session_id, revision: header.revision };
      announceWork(binding, true, turn_id ? { turn_phase: 'start', ...correlation() } : {});
    },
    /** What was delivered and not yet seen taken: a harness that must first find where its conversation is
     *  recorded (Cursor's editor chats) looks for these. */
    expecting() { return [...binding.pending.keys()]; },
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
    /** Which model the conversation thinks with, as its harness records it. It replaces whatever the
     *  launch line said, and stays on the binding: a room that restarts learns it from the next
     *  registration, and one that is not there yet from the first. */
    engine(observed) {
      if (!observed?.model) return;
      const engine = { model: observed.model, effort: observed.effort ?? null, thinking: observed.thinking ?? null };
      const previous = binding.engine || {};
      if (previous.model === engine.model && previous.effort === engine.effort && previous.thinking === engine.thinking) return;
      binding.engine = engine;
      log(`${binding.thread} thinks with ${engine.model}${engine.effort ? ' (effort ' + engine.effort + ')' : ''}`);
      // Until the room has minted the durable id there is nothing to name: the registration carries it.
      if (!binding.binding_id.startsWith('local-')) send('input.engine', { binding_id: binding.binding_id, engine });
    },
  });
  keepAnnouncing();
}
function unwatch(binding) { try { binding.stop?.(); } catch {} binding.stop = null; try { binding.release?.(); } catch {} binding.release = null; }

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

/** The local core, installed and started if need be, and the credential its ready file names. Waiters
 *  are woken on failure too, so a conversation asking to join hears why rather than timing out. */
function startCore() {
  if (supervised) return supervisor.ensure().then(status => (status.state === 'running' ? creds?.core : null));
  if (coreStarting) return coreStarting;
  coreError = null; coreFailure = null;
  coreStarting = ensureRunning({ dataDir, env, log, roomCredential: credentialsPath })
    .then(ready => {
      coreError = null;
      creds = coreLink(ready);
      log(`linking to this machine's core ${ready.version || '?'} on ${ready.socket} (pid ${ready.pid}, launch ${ready.launch_id})`);
      return ready;
    })
    .catch(error => {
      coreError = lastError = error.message;
      coreFailure = error.failure || { key: error.key || 'launch.exited', message: error.message, at: new Date().toISOString() };
      log('the local core is not available: ' + error.message);
      for (const wake of waking.splice(0)) wake();
      return null;
    })
    .finally(() => { coreStarting = null; });
  return coreStarting;
}

/** Whether a core is on its way: a plain connector starting one, or the supervisor starting or retrying. */
function coreComing() {
  return supervised ? ['starting', 'backoff'].includes(supervisor.state) : !!coreStarting;
}

/** The supervisor's core, as the state machine needs it: real processes, real files (`core.mjs`). */
function superviseWith(restored) {
  const install = message => /certificate|UnknownIssuer|proxy/i.test(message) ? 'install.proxy'
    : /network|reach|dns|connect/i.test(message) ? 'install.network' : null;
  const number = name => (env[name] ? Number(env[name]) : undefined);
  const timing = Object.fromEntries(Object.entries({ probe: number('SIDEVOICE_PROBE_MS'), healthy: number('SIDEVOICE_HEALTHY_MS'),
    backoff: env.SIDEVOICE_BACKOFF_MS ? env.SIDEVOICE_BACKOFF_MS.split(',').map(Number) : undefined }).filter(([, value]) => value !== undefined));
  return new Supervisor({
    service: serviceKind, restored, log, timing,
    async launch(launchId) {
      ensureCoreDirectory(coreData(dataDir));
      const bin = await ensureInstalled({ dataDir, env, log });
      unlinkSocket(dataDir);
      try { rmSync(failurePath(dataDir), { force: true }); } catch {}
      return spawnCore(bin, coreArgs({ dataDir, env, launchId, idleExit: 0, roomCredential: credentialsPath }), { dataDir, env });
    },
    awaitReady: (handle, launchId, signal) => awaitReady(handle, { dataDir, launchId, signal }),
    probe: async core => { const health = await localHealth(core.socket, 2000); return health?.status === 200 && health.body?.launch_id === core.launch_id ? health.body : null; },
    async terminate(handle) { await (handle.terminate ? handle.terminate() : terminateCore(handle, { log })); unlinkSocket(dataDir); },
    cause({ launchId, exit, key }) {
      // Installing the core is part of starting it: a uv that is missing or cannot reach its index says so.
      // A refusal made before spawning keeps its own key (`identity.unsafe-directory`…): it is the cause.
      if (exit?.error?.key) return { key: exit.error.key, step: exit.error.key.split('.')[0], message: exit.error.message, detail: null, at: new Date().toISOString(), log_tail: [] };
      const message = exit?.error?.message || '';
      if (message === NO_UV) return { key: 'install.no-bundle', step: 'install', message, detail: null, at: new Date().toISOString(), log_tail: [] };
      if (exit?.error && !exit.error.path && install(message)) return { key: install(message), step: 'install', message, detail: null, at: new Date().toISOString(), log_tail: [] };
      return failureCause({ dataDir, launchId, exit, key });
    },
    /** A core left running — by a SIGKILLed supervisor, by a plain connector — is kept when it answers for its
     *  own launch and is the version pinned; anything else running is ended before this one starts its own. */
    async adopt() {
      const found = await examineRunning(dataDir);
      const fits = found.adopt && runsSelected(dataDir, env, found.adopt);
      const leftover = found.terminate || (found.adopt && !fits ? found.adopt : null);
      if (leftover) { log(`a core is running (pid ${leftover.pid}) that this supervisor does not keep: terminating it`); await terminateCore(leftover, { log }); unlinkSocket(dataDir); }
      if (!fits) return null;
      const adopted = found.adopt;
      // Not a child: watched by its identity, and signalled only while it is still that launch.
      const handle = { pid: adopted.pid, launch_id: adopted.launch_id, done: null };
      handle.terminate = () => terminateCore({ pid: adopted.pid, launch_id: adopted.launch_id }, { log });
      handle.exit = new Promise(resolve => {
        const timer = setInterval(() => { if (!coreRunning(adopted)) { clearInterval(timer); resolve(handle.done = { code: null, signal: null, gone: true }); } }, 200);
        timer.unref?.();
      });
      return { handle, core: found.adopt };
    },
    persist(snapshot) {
      try { writePrivate(files.status, { ...snapshot, installed: existsSync(files.install), at: new Date().toISOString() }); } catch {}
      coreChanged(snapshot);
    },
  });
}

/** What the link does when the supervisor's core changes: a new launch is linked to (the welcome then
 *  re-registers every binding); a failure is what a conversation asking to join is told. */
/** Five minutes running: the installation before this one is no longer what a rollback would need (§4.3 step 6). */
let pruneTimer = null;
function schedulePrune() {
  clearTimeout(pruneTimer);
  pruneTimer = setTimeout(() => { pruneInstallations(env, { log }).catch(error => log('removing previous installations failed: ' + error.message)); }, PRUNE_AFTER_MS);
  pruneTimer.unref?.();
}
const PRUNE_AFTER_MS = 5 * 60_000;

function coreChanged(snapshot) {
  coreFailure = snapshot.state === 'failed' || snapshot.state === 'backoff' ? snapshot.failure : null;
  coreError = snapshot.state === 'failed' ? (snapshot.failure?.message || snapshot.failure?.key || 'the core failed') : null;
  if (snapshot.state === 'failed') { lastError = coreError; for (const wake of waking.splice(0)) wake(); }
  if (snapshot.state !== 'running') clearTimeout(pruneTimer);
  if (pendingJournal && (snapshot.state === 'running' || snapshot.state === 'failed')) settleJournal(snapshot).catch(error => log('settling the install transaction failed: ' + error.message));
  if (snapshot.state !== 'running' || !supervisor?.core) return;
  if (creds?.core?.launch_id === supervisor.core.launch_id) return;
  schedulePrune();
  creds = coreLink(supervisor.core);
  log(`linking to this machine's core ${creds.core.version || '?'} on ${creds.core.socket} (pid ${creds.core.pid}, launch ${creds.core.launch_id})`);
  if (link) { const old = link; link = null; old.close(); }
  open();
}

/** A plain connector's core: a link that keeps failing to a core whose process is gone is a core to start
 *  again, and the link goes to that launch; the welcome re-registers every binding as after any reconnect.
 *  The supervisor's core is the state machine's business. */
async function superviseCore() {
  if (supervised || external || closed || coreStarting || Date.now() - coreCheckedAt < 2000) return;
  coreCheckedAt = Date.now();
  if (creds?.core && coreRunning(creds.core)) return;
  log('the local core is gone; starting it again');
  const previous = creds?.core?.launch_id;
  const ready = await startCore();
  // Another launch, perhaps another credential: the link is opened again to it.
  if (ready && ready.launch_id !== previous && link) { const old = link; link = null; old.close(); open(); }
}

/** Open the one link to this machine's room. Idempotent: it is called whenever a conversation
 *  joins, and a link that exists already — connected, or on its way back — is the answer. */
function open() {
  if (closed || link) return;
  if (!creds && external) creds = external;
  // The supervisor's core links when it is running (`coreChanged`); a plain connector starts its own.
  if (!creds) { if (!supervised) startCore().then(ready => { if (ready) open(); }); return; }
  link = roomLink({
    origin: creds.room, connector_id: creds.connector_id, token: creds.token, agent: creds.agent,
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
      if (reason.retrying !== false) superviseCore().catch(error => log('supervising the core failed: ' + error.message));
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

/** Until the link is up, or the timeout. True when something woke the wait — a welcome, or a failure
 *  whose reason the caller then reads — false when the time ran out. */
function welcomed(timeout) {
  if (connected) return Promise.resolve(true);
  return new Promise(resolve => {
    const timer = setTimeout(() => { waking = waking.filter(wake => wake !== wakeup); resolve(false); }, timeout);
    const wakeup = () => { clearTimeout(timer); resolve(true); };
    waking.push(wakeup);
  });
}

/** The room's answer for one binding, however long it takes to be able to ask: a conversation that
 *  joins while the room is down is registered by the next welcome, and that is this call's answer too. */
async function joinRoom(binding, timeout = 10_000) {
  if (refusal) throw new Error(refusal);
  if (connected) return enrol(binding);
  const woken = await welcomed(timeout);
  // A room that is not there yet is worth waiting for; one that will not have this machine is not.
  if (refusal) throw new Error(refusal);
  if (coreError) throw new Error(coreError);   // the local core could not be had, and says why
  if (!woken) throw new Error(UNREACHABLE);
  return binding.registration ?? enrol(binding);
}

/** A one-time code for pairing a device with this machine (sidevoice-core's `server/devices.py`). The core issues it
 *  and keeps the devices; this only asks, starting the core first as a join would. Whether the room will
 *  have this machine does not matter here: a device pairs with the machine, not with the room. */
const CORE_WAIT_MS = Number(process.env.SIDEVOICE_CORE_WAIT_MS || 30_000);
/** How long a code waits for a core that is being installed (the first time: Python and a few hundred MB). */
const CORE_INSTALL_WAIT_MS = Number(process.env.SIDEVOICE_CORE_INSTALL_WAIT_MS || 120_000);
const PAIRING_CODE_TIMEOUT_MS = 10_000;
async function devicePairingCode() {
  open();
  if (!connected) await welcomed(CORE_WAIT_MS);
  // A first install still running is waited for, up to a bound: the code comes as soon as the core does.
  const waitedSince = Date.now();
  while (!connected && !coreError && (coreComing() || installInProgress(dataDir)) && Date.now() - waitedSince < CORE_INSTALL_WAIT_MS) await welcomed(2000);
  if (!connected && !coreError) await welcomed(5000);   // started a moment ago: the link is on its way
  if (!connected) {
    if (coreError) throw new Error(coreError);
    const installing = installInProgress(dataDir);
    if (installing) throw new Error(`This machine's Sidevoice core is still being installed (${installing.seconds} s so far${installing.last ? '; last step: ' + installing.last : ''}). Ask again in a minute; progress is in ${installing.log}.`);
    throw new Error('This machine\'s core did not come up in time; ask again in a moment. Why is in ' + path.join(dataDir, 'core.log') + '.');
  }
  let answer;
  try { answer = await request('device.pairing_code', {}, { timeout: PAIRING_CODE_TIMEOUT_MS }); }
  catch (error) { throw new Error(`This machine's core did not issue a pairing code (${error.message}); it may be older than device pairing.`); }
  if (typeof answer?.code !== 'string' || !answer.code) throw new Error(answer?.error || answer?.detail || 'This machine\'s core answered without a pairing code.');
  log(`the core issued a device pairing code (valid ${answer.expires_in ?? '?'} s)`);
  return { code: answer.code, payload: answer.payload ?? null, expires_in: answer.expires_in ?? null };
}

async function announce(binding) {
  // `local-*` is only a connector-side placeholder while the first registration waits for the
  // room to mint its durable binding id. Sending it back makes the room correctly reject it as foreign.
  const frame = { client_ref: binding.client_ref, harness: binding.harness, thread: binding.thread,
    title: binding.title, inbound: binding.inbound, capabilities: binding.capabilities, experimental: binding.experimental || [],
    ...(binding.route ? { route: binding.route } : {}),
    engine: binding.engine, focus: false };
  if (!binding.binding_id.startsWith('local-')) frame.binding_id = binding.binding_id;
  const reply = await request('binding.register', frame).catch(error => {
    log(`room rejected ${binding.client_ref}: ${error.message}`);
    throw error;
  });
  // Gone while the core was answering — the pairing was refused, or its façade left: not put back, and the
  // core is told to let it go, or it would stay live there with nobody behind it.
  if (bindings.get(binding.binding_id) !== binding) {
    if (reply.binding_id) send('binding.unregister', { binding_id: reply.binding_id });
    throw new Error(refusal || 'This conversation left while it was being registered.');
  }
  if (binding.binding_id !== reply.binding_id) { bindings.delete(binding.binding_id); binding.binding_id = reply.binding_id; bindings.set(reply.binding_id, binding); }
  // An engine observed while the room was answering is not lost: the frame was built before it arrived.
  if (binding.engine !== frame.engine) send('input.engine', { binding_id: binding.binding_id, engine: binding.engine });
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
      // Counted: a handover waits for every delivery under way to settle before it lets the bindings go.
      deliveries.add(answer); answer.catch(() => {}).finally(() => deliveries.delete(answer));
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
    case 'node.rendezvous': {
      // How the core's link with the room is doing. A refusal — the person took this machine's pairing
      // away from the room's page — lets every conversation go, as the room closing their voice would;
      // a pairing made again clears it, and conversations can join again.
      const previous = rendezvous; rendezvous = frame;
      if (frame.refused) refuse(frame.refused);
      else if (refusal && previous?.refused === refusal) { refusal = lastError = null; log('the room accepts this machine again'); }
      if (!previous || previous.connected !== frame.connected) log(`the room at ${frame.room || '(not paired)'} is ${frame.connected ? 'reachable' : 'not reachable'}${frame.via ? ' (' + frame.via + ')' : ''}${frame.error ? ': ' + frame.error : ''}`);
      return;
    }
    case 'pair.request': {
      // A page talking to this machine's core directly (a desktop shell) pairs it with a room: the same
      // act as voice_pair, with the code the person read from that room. The core asks; pairing — and the
      // credentials file it writes, which the core then follows — stays this connector's.
      if (typeof frame.room !== 'string' || typeof frame.code !== 'string' || !frame.room || !frame.code)
        return { ok: false, detail: 'Hacen falta la dirección de la sala y el código.' };
      try {
        const paired = await pair(frame.room, frame.code);
        log(`paired with ${paired.origin} as ${paired.connector_id}, asked from a page through the core`);
        return { ok: true, origin: paired.origin, connector_id: paired.connector_id };
      } catch (error) { return { ok: false, detail: error.message }; }
    }
    case 'connector.error': lastError = frame.error; log('room says: ' + frame.error); return;
  }
}

/** One message into the conversation, through the adapter its binding was registered with. */
async function handOver(binding, frame) {
  // A harness that declares it cannot take input is said so, not tried: nothing reaches the conversation.
  const harness = harnessFor(binding.harness);
  // The conversation's own declaration counts: a harness can deliver and still not into this conversation.
  if (capabilityState(harness, 'deliver') !== SUPPORTED || capabilityState(binding, 'deliver') === 'unsupported') {
    log(`not delivering ${frame.event_id} (${frame.message_id}) to ${binding.thread}: ${binding.harness} cannot take input from the room`);
    return { status: 'unsupported', error: `${binding.harness} offers no way to put a message into this conversation` };
  }
  if (frame.session_id !== undefined && frame.revision !== undefined) {
    // Every conversation a turn went to: the room can send the same pair to two (revision 0 is reused for
    // catch-up input), and then a reply naming it names neither.
    const key = turnKey(frame.session_id, frame.revision);
    const went = turnsDelivered.get(key) || new Set();
    turnsDelivered.delete(key); went.add(binding.client_ref); turnsDelivered.set(key, went);
    while (turnsDelivered.size > TURNS_MAX) turnsDelivered.delete(turnsDelivered.keys().next().value);
  }
  // Expected before it is sent: the harness can take the message, and its transcript show it, before the
  // delivery call has even settled (Claude Code admitted one 9 ms after the write; the socket answered
  // 1.5 s later, 2026-09-21). A message expected and never taken costs a map entry.
  if (binding.pending && frame.message_id) {
    binding.pending.set(frame.message_id, { session_id: frame.session_id, revision: frame.revision, at: Date.now() });
    while (binding.pending.size > PENDING_MAX) binding.pending.delete(binding.pending.keys().next().value);
  }
  try {
    const outcome = await harness.deliver(binding.delivery, frame);
    log(`delivered ${frame.event_id} (${frame.message_id}) to ${binding.thread} via ${binding.delivery.kind}: ${outcome.status} (${outcome.detail})`);
    return { status: outcome.status, detail: outcome.detail };
  } catch (error) {
    binding.pending?.delete(frame.message_id);
    log(`delivery of ${frame.event_id} to ${binding.thread} failed: ${error.message}`);
    return { status: 'failed', error: String(error.message || error).slice(0, 400) };
  }
}

/** Whether what is said here reaches a room a person can be in: the core is linked and its link with the
 *  room is up. Until this machine's own core has said, the answer is no; a core somebody else runs may
 *  never say, and then its link is the answer. */
function reachable() { return connected && (rendezvous ? rendezvous.connected === true : !!external); }

function snapshot() {
  return { host: hostId, version: VERSION, room: rendezvous?.room || (external ? creds?.room : null) || null, connected: reachable(), protocol: PROTOCOL,
    core: creds?.core ? { url: creds.core.url, pid: creds.core.pid, version: creds.core.version, linked: connected } : (external ? { url: external.room, linked: connected } : null),
    core_error: coreError, rendezvous,
    outbox: outbox.length, room_error: lastError || rendezvous?.error || null, socket_error: socketError, refused: refusal,
    // Which conversations lost their voice from the room, and why each one did: the same map read
    // twice, because "it is gone" and "this is what happened" are two different questions.
    closed_by_room: [...closedByRoom.keys()], closed_reasons: Object.fromEntries(closedByRoom),
    bindings: [...bindings.values()].map(({ binding_id, client_ref, harness, thread, title, delivery, capabilities }) => {
      let state = null; try { state = harnessFor(harness).deliveryState?.(delivery) || null; } catch {}
      return { binding_id, client_ref, harness, thread, title, delivery: delivery.kind, capabilities, ...(state ? { delivery_state: state } : {}) };
    }) };
}
/** What only asks how things are: the app polls it every 2 s on a fresh connection. A client that asks nothing
 *  else does not keep a plain connector alive, and neither its arrival nor its leaving touches the idle timer —
 *  only conversations and façades that asked for real work do. */
const PROBES = new Set(['status', 'node.status']);
const engaged = () => [...clients].filter(client => client.engaged).length;

/** A plain connector leaves once nothing has used it for a while; the supervisor never does. */
function scheduleExit() {
  if (supervised || handingOver) return;
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => { if (engaged() === 0 && bindings.size === 0) { log(`idle for ${idleMs} ms with no conversation; exiting`); shutdown(); } }, idleMs);
}
let server = null;
async function shutdown(code = 0) {
  if (closed) return;
  log(`shutting down (${bindings.size} binding(s), ${clients.size} façade(s))`);
  closed = true; clearTimeout(idleTimer);
  try { link?.close(); } catch {}
  server?.close();
  // The supervisor's core is its child: it leaves with it (SIGTERM, 15 s, SIGKILL), and its socket after it.
  if (supervisor) await supervisor.stop().catch(error => log('stopping the core failed: ' + error.message));
  releaseLock({ socket: true });
  process.exit(code);
}

/* ----- an install transaction under way, seen from the supervisor (§4.3, `install-txn.mjs`) ----- */

/** The recovered transaction this supervisor runs, while it waits to be seen running: `{side, record}`. */
let pendingJournal = null;
/** Recover a journal an installer left (unless an installer holds the lock: then it is the installer's). The
 *  artifacts are made so for the selected installation; if that is not this program, this supervisor hands over
 *  to it; if it is, the journal stays until this supervisor's core runs compatibly (`settleJournal`). */
async function checkInstallJournal() {
  if (!supervisor || !existsSync(files.journal)) return;
  const release = await takeInstallLock(dataDir, log, { wait: false });
  if (!release) return;
  let outcome = null;
  try { outcome = await recover(env, { mode: 'supervisor', log }); }
  catch (error) { log('recovering the install transaction failed (it is kept): ' + error.message); }
  finally { release(); }
  if (!outcome) return;
  if (!outcome.record || !sameCommand(outcome.record.command, ownCommand())) return handOff(outcome.record);
  pendingJournal = outcome;
  if (supervisor.state === 'running' || supervisor.state === 'failed') await settleJournal(supervisor.status());
}

/** This supervisor's core ran (the journal goes) or cannot (back to the previous installation, handed over to). */
async function settleJournal(snapshot) {
  const running = snapshot.state === 'running' && Number.isInteger(snapshot.core?.api) && snapshot.core.api >= API_RANGE[0] && snapshot.core.api <= API_RANGE[1];
  const release = await takeInstallLock(dataDir, log, { wait: false });
  if (!release) return;
  let settled;
  try { settled = await settle(env, { running, failed: snapshot.state === 'failed' }); }
  finally { release(); }
  if (settled.outcome === 'done') { pendingJournal = null; log('the installation this supervisor runs is running: the install transaction is complete'); }
  if (settled.outcome === 'rollback') { pendingJournal = null; await handOff(settled.record); }
}

/** The selected installation is another program: it takes over. Under a service manager, the definition (already
 *  rewritten for it) is loaded again by `service reload`, run by a helper outside this job — launchd reads a plist
 *  only when it is bootstrapped, and booting the job out ends this process. Detached, the selected program is
 *  started to wait for this one to leave (`--after`). Nothing selected: this supervisor stops. */
async function handOff(record) {
  if (!record) { log('no installation is selected any more: stopping'); return shutdown(0); }
  log(`the selected installation is ${record.id} (${record.command.join(' ')}), not this program: handing over to it`);
  // Another program's start: what this one spent of the budget is not that one's (as after a person's restart).
  try { writePrivate(files.restart, { at: new Date().toISOString(), why: 'handover' }); } catch {}
  const args = serviceKind === 'none' ? ['connector', '--supervise', '--after', String(process.pid)] : ['service', 'reload', '--json'];
  const child = spawn(record.command[0], [...record.command.slice(1), ...args], { detached: true, stdio: 'ignore', env: { ...env, SIDEVOICE_SERVICE: serviceKind } });
  child.on('error', error => log('handing over failed: ' + error.message));
  child.unref();
  if (serviceKind === 'none') return shutdown(0);
}
function releaseLock({ socket }) {
  // The socket is ours to remove only while the lock still is.
  if (!lockRecord || !stillHeld(lockPath, lockRecord)) return;
  if (socket) { try { unlinkSync(socketPath); } catch {} }
  giveUp(lockPath, lockRecord);
}

/** `node.status` (SEAMS §4): the supervisor's state machine as it is, the calls fresh from the core; a plain
 *  connector says what it knows of the core it started or found. */
async function nodeStatus() {
  const installed = existsSync(files.install);
  if (supervisor) {
    if (supervisor.state === 'running' && supervisor.core) {
      const health = await localHealth(supervisor.core.socket, 1000);
      if (typeof health?.body?.calls === 'number') supervisor.calls = health.body.calls;
    }
    return { ...supervisor.status(), installed, supervisor: true, command: ownCommand() };
  }
  const core = creds?.core && !external && coreRunning(creds.core) ? creds.core : null;
  const health = core ? await localHealth(core.socket, 1000) : null;
  const state = core ? 'running' : coreStarting ? 'starting' : coreFailure ? 'failed' : 'stopped';
  return { ok: true, state, since: null, attempts: 0, window_started: null, next_retry_at: null,
    core: core ? { pid: core.pid, version: core.version ?? null, api: health?.body?.api ?? core.api ?? null, launch_id: core.launch_id ?? null } : null,
    calls: health?.body?.calls ?? 0, failure: core ? null : coreFailure, service: 'none', installed, supervisor: false, command: ownCommand() };
}

/** The program this connector is — what an installation's `command` names when it is this one. */
const ownCommand = () => [process.execPath, process.argv[1]];

/** `node.restart`: the core stopped and started again — a person's restart, which closes the budget window,
 *  or a pairing written (`sidevoice pair`) that the core must start with. */
async function nodeRestart() {
  // Answered once the restart has begun (`starting`): the launch takes up to a minute, and `node.status` follows it.
  if (supervisor) { supervisor.restart(); return nodeStatus(); }
  if (external) return nodeStatus();
  // A plain connector's restarts, one at a time: a second never terminates what the first just started.
  const run = plainRestarts.then(restartPlainCore, restartPlainCore);
  plainRestarts = run.catch(() => {});
  return run;
}
let plainRestarts = Promise.resolve();
async function restartPlainCore() {
  const running = creds?.core;
  if (running && coreRunning(running)) await terminateCore(running, { log });
  creds = null;
  const ready = await startCore();
  if (ready && link) { const old = link; link = null; old.close(); }
  open();
  return nodeStatus();
}

/* ----- handover (§4.2): a plain connector gives every binding, whole, to the supervisor ----- */

const HANDOVER_SETTLE_MS = 5000;
/** A restored binding whose façade does not come back for it within this long is let go — unless its
 *  delivery does not need a façade (an editor chat's card), which stays as any kept conversation does. */
const REATTACH_MS = Number(process.env.SIDEVOICE_HANDOVER_REATTACH_MS || 60_000);

/** The old connector's side: stop taking commands, let those in flight settle, flush the outbox, write every
 *  binding as `register` built it, tell each façade, close it without letting its bindings go, and leave. */
async function yieldToSupervisor(requester) {
  if (supervised) throw new Error('This connector is the node service; it does not hand over.');
  handingOver = true; clearTimeout(idleTimer);
  log(`a supervisor is taking over: settling ${inFlight.size} command(s), flushing ${outbox.length} queued speech`);
  server.close();   // no new façade; the connected ones are told below
  const settle = Promise.allSettled([...inFlight]);
  await Promise.race([settle, wait(HANDOVER_SETTLE_MS)]);
  if (connected && outbox.length) await Promise.race([Promise.allSettled([...outbox].map(speech => publish(speech))), wait(HANDOVER_SETTLE_MS)]);
  // No new turn from the core: the link is closed, and the turns it already sent are delivered (or have failed)
  // before anything is written — what a delivery records (the turn it carried, the message the harness is to
  // take) is then complete.
  closed = true;
  try { link?.close(); } catch {}
  await Promise.race([Promise.allSettled([...deliveries]), wait(HANDOVER_SETTLE_MS)]);
  if (deliveries.size) log(`${deliveries.size} delivery(ies) still under way after ${HANDOVER_SETTLE_MS} ms: the core delivers them again`);
  const record = { at: new Date().toISOString(), from_pid: process.pid,
    // Which conversation each turn went to (a reply names the turn, not the conversation), and which messages
    // were already reported read: the supervisor routes a reply to a turn delivered here, and reports nothing twice.
    turns: [...turnsDelivered].map(([key, refs]) => [key, [...refs]]),
    read: [...readReported],
    bindings: [...bindings.values()].map(binding => ({
      binding_id: binding.binding_id, client_ref: binding.client_ref, harness: binding.harness, thread: binding.thread, title: binding.title,
      delivery: binding.delivery, inbound: binding.inbound, capabilities: binding.capabilities, experimental: binding.experimental,
      engine: binding.engine, route: binding.route, owned: !!binding.owner, prepared: binding.prepared || null,
      // What the conversation was doing: messages delivered and not yet seen taken, the turn under way, working or not.
      pending: binding.pending ? [...binding.pending] : [], turn: binding.turn ?? null, working: binding.working ?? null })) };
  writePrivate(files.handover, record);
  log(`handover.json written: ${record.bindings.length} binding(s), ${outbox.length} speech frame(s) left in the outbox`);
  // After the answer has gone: façades told and closed (their bindings are the supervisor's now), the
  // watchers and the card bridge let go so the supervisor can take them, the lock released, gone.
  setImmediate(() => {
    for (const client of clients) {
      if (client === requester) continue;
      client.handedOver = true;
      client.socket.end(JSON.stringify({ type: 'handover' }) + '\n');
    }
    for (const binding of bindings.values()) unwatch(binding);
    releaseLock({ socket: false });
    setTimeout(() => process.exit(0), 100);
  });
  return { ok: true, bindings: record.bindings.length };
}

/** The supervisor's side, finding a plain connector on the socket: ask it to hand over, wait until it is
 *  gone. False when the one holding the socket is itself a supervisor (then this one is redundant). */
async function takeOver(owner) {
  const pid = owner?.pid;
  if (!connectorAlive(owner)) return true;   // gone meanwhile, or not provably anyone: the lock decides
  log(`a connector (pid ${pid}) holds ${socketPath}: asking it to hand over`);
  const answer = await new Promise(resolve => {
    const socket = net.createConnection(socketPath);
    let buffer = '';
    const timer = setTimeout(() => { socket.destroy(); resolve({ ok: false, error: 'no answer' }); }, 15_000);
    socket.on('error', error => { clearTimeout(timer); resolve({ ok: false, error: error.message }); });
    socket.on('connect', () => socket.write(JSON.stringify({ id: 1, method: 'handover', params: {} }) + '\n'));
    socket.on('data', chunk => {
      buffer += chunk; const index = buffer.indexOf('\n'); if (index < 0) return;
      clearTimeout(timer); socket.destroy();
      try { resolve(JSON.parse(buffer.slice(0, index))); } catch { resolve({ ok: false, error: 'unreadable answer' }); }
    });
  });
  if (!answer.ok && /node service/.test(answer.error || '')) return false;
  // Already handing over (to another supervisor that asked first): it leaves by itself.
  if (!answer.ok && !String(answer.error).startsWith('HANDOVER:')) {
    // It cannot hand over (it answers nothing, or does not know how): asked to leave like any stop.
    log(`the connector (pid ${pid}) did not hand over (${answer.error}); asking it to stop`);
    signalVerified(pid, 'SIGTERM', { start: owner.start ?? null });
  }
  const deadline = Date.now() + 10_000;
  while (connectorAlive(owner) && Date.now() < deadline) await wait(50);
  if (connectorAlive(owner)) { signalVerified(pid, 'SIGKILL', { start: owner.start ?? null }); await wait(200); }
  return true;
}

/** Every binding of `handover.json`, through the same code as `register`: watched, prepared (an editor chat's
 *  card bridge on the port its card knows), and — at the next welcome — joined to the core with the same
 *  `binding_id`, which the core takes as that binding coming back. Then the file goes. Before any façade. */
async function restoreHandover() {
  const record = readJson(files.handover);
  if (!record) return;
  for (const [key, refs] of record.turns || []) if (Array.isArray(refs)) turnsDelivered.set(key, new Set(refs));
  for (const id of record.read || []) readReported.add(id);
  for (const saved of record.bindings || []) {
    if (!saved?.binding_id || !saved.client_ref || !saved.delivery?.kind) continue;
    const { prepared, owned, pending, turn, working, ...fields } = saved;
    const binding = { ...fields, owner: null, pending: new Map(Array.isArray(pending) ? pending : []), turn: turn ?? null };
    if (typeof working === 'boolean') binding.working = working;
    bindings.set(binding.binding_id, binding);
    watch(binding);
    await prepareBinding(binding, { port: prepared?.port ?? null });
    if (binding.detachable) { binding.orphanedAt = Date.now(); keepOrphans(); }
    else {
      binding.ownerTimer = setTimeout(() => {
        if (binding.owner || bindings.get(binding.binding_id) !== binding) return;
        log(`${binding.thread} leaves: its façade did not come back after the handover`);
        bindings.delete(binding.binding_id); unwatch(binding); send('binding.unregister', { binding_id: binding.binding_id }); scheduleExit();
      }, REATTACH_MS);
      binding.ownerTimer.unref?.();
    }
    log(`${binding.thread} restored from the handover as ${binding.binding_id}${owned ? ', waiting for its façade' : ' (kept without a façade)'}`);
  }
  try { unlinkSync(files.handover); } catch {}
}

/** Conversations kept without a façade leave once their card has not been heard from for this long (or, never
 *  heard from, this long after their façade went): a chat whose window closed does not stay listed for ever. */
const ORPHAN_TTL_MS = Number(process.env.SIDEVOICE_ORPHAN_TTL_MS || 30 * 60_000);
let orphanTimer = null;
function keepOrphans() {
  if (orphanTimer) return;
  orphanTimer = setInterval(() => {
    let left = 0;
    for (const binding of [...bindings.values()]) {
      if (binding.owner || !binding.detachable) continue;
      let state = null; try { state = harnessFor(binding.harness).deliveryState?.(binding.delivery) || null; } catch {}
      const quiet = state?.card_connected ? state.card_last_seen_ms_ago : Date.now() - binding.orphanedAt;
      if (quiet <= ORPHAN_TTL_MS || Date.now() - binding.orphanedAt <= ORPHAN_TTL_MS) { left++; continue; }
      log(`${binding.thread} leaves: no façade, and its card silent for ${Math.round(quiet / 1000)} s`);
      bindings.delete(binding.binding_id); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send('binding.unregister', { binding_id: binding.binding_id });
    }
    if (!left) { clearInterval(orphanTimer); orphanTimer = null; scheduleExit(); }
  }, Math.min(60_000, Math.max(50, ORPHAN_TTL_MS / 4)));
  orphanTimer.unref?.();
}
/** What the harness must have open before anything is delivered (Cursor's editor: the view's bridge), for a
 *  binding that already exists — so a command right behind `register` finds it. The same for a new binding
 *  and for one restored after a handover, which asks for the port its card already knows. */
async function prepareBinding(binding, { port = null } = {}) {
  const module = harnessFor(binding.harness);
  if (typeof module.prepare !== 'function') return null;
  // `admitted`: the harness itself answered that the conversation took a message (Cursor's editor answers a
  // card's ui/message when the turn it started has run) — as good a second tick as seeing it recorded.
  const admitted = message_id => { const sent = binding.pending?.get(message_id); if (sent) reportRead(binding, { message_id, session_id: sent.session_id, revision: sent.revision }); };
  let prepared = null;
  try { prepared = await module.prepare(binding.delivery, { log, admitted, port }) || null; } catch (error) { log(`${binding.thread} could not be prepared for delivery: ${error.message}`); }
  if (prepared && bindings.get(binding.binding_id) === binding) { binding.release = prepared.release; binding.detachable = !!prepared.detachable; binding.prepared = prepared.info || null; }
  else prepared?.release?.();
  return prepared;
}

/** A façade taking over a conversation kept without one. Only one that says it speaks for editor chats, and
 *  only a conversation of that kind: the id is what the chat was handed by its own voice_connect. */
function adopt(client, client_ref) {
  const binding = [...bindings.values()].find(b => b.client_ref === client_ref && !b.owner && b.detachable);
  if (!binding) return null;
  binding.owner = client; binding.orphanedAt = null; client.bindings.add(binding);
  log(`${binding.thread} taken over by a new façade`);
  return { binding_id: binding.binding_id, client_ref: binding.client_ref, harness: binding.harness, title: binding.title, capabilities: binding.capabilities, experimental: binding.experimental || [] };
}

/** The conversation a voice_say belongs to, among those its façade speaks for: the one its session names
 *  (`typed:<conversation>`, handed only to that conversation), or the one that turn was delivered to. */
function routeSpeech({ client_refs, session_id, revision, adopt_orphans }) {
  // An editor façade also speaks for the editor chats kept without one: the turn names which.
  const own = new Set([...client_refs, ...(adopt_orphans ? [...bindings.values()].filter(b => !b.owner && b.detachable).map(b => b.client_ref) : [])]);
  // Routed (several conversations, no one named): `typed:<conversation>` names one only for a conversation that
  // cannot take input. An editor chat speaking first names itself to its façade, which sends it here by name.
  if (typeof session_id === 'string' && session_id.startsWith('typed:')) {
    const named = session_id.slice(6);
    const binding = [...bindings.values()].find(b => b.client_ref === named);
    if (own.has(named) && binding && capabilityState(binding, 'deliver') === 'unsupported') return named;
    throw new Error('AMBIGUOUS: that typed session belongs to no conversation of yours that cannot take input; reply in writing');
  }
  const delivered = [...(turnsDelivered.get(turnKey(session_id, revision)) || [])];
  const mine = delivered.filter(ref => own.has(ref));
  if (mine.length === 1 && delivered.length === 1) return mine[0];
  throw new Error(delivered.length > 1
    ? 'AMBIGUOUS: that voice turn went to more than one conversation, so it cannot say which one is answering; reply in writing'
    : 'AMBIGUOUS: this session_id and revision name no voice turn delivered to this chat — it has received none to answer by voice (reply in writing), or use the ones from the voice message you are answering');
}

async function command(client, input) {
  let params = input.params || {};
  switch (input.method) {
    case 'register': {
      const { client_ref, harness, thread, title, delivery, inbound, capabilities, experimental, engine, route } = params;
      if (!client_ref || !thread || !delivery?.kind) throw new Error('client_ref, thread and delivery are required');
      closedByRoom.delete(client_ref);   // joining again is the user's explicit request
      const existing = [...bindings.values()].find(b => b.client_ref === client_ref);
      if (existing) {
        // The same conversation again — its façade after a handover, or a second voice_connect: only the owner
        // is attached; the binding, its watch and its preparation are the ones already there.
        Object.assign(existing, { owner: client, delivery, inbound, capabilities, experimental, route, orphanedAt: null });
        clearTimeout(existing.ownerTimer); existing.ownerTimer = null;
        client.bindings.add(existing);
        return { binding_id: existing.binding_id, thread, connected: reachable(), ...(existing.prepared ? { prepared: existing.prepared } : {}) };
      }
      const local_id = 'local-' + randomUUID();
      log(`${harness} ${thread} joins ("${title || ''}", delivery ${delivery.kind}, inbound ${inbound ? (inbound.ok ? 'ok' : 'held') : 'n/a'})`);
      const binding = { binding_id: local_id, client_ref, harness, thread, title, delivery, inbound, capabilities, experimental, engine, route, owner: client };
      bindings.set(local_id, binding); client.bindings.add(binding); clearTimeout(idleTimer); open(); watch(binding);
      const prepared = await prepareBinding(binding);
      // A room that is not there yet is not a failure: the binding is registered on the next welcome.
      const reply = await joinRoom(binding).catch(error => { if (!connected && !refusal && !coreError) return null; bindings.delete(binding.binding_id); client.bindings.delete(binding); unwatch(binding); throw error; });
      return { binding_id: reply?.binding_id || binding.binding_id, thread, connected: reachable(), pending: !reply, ...(prepared?.info ? { prepared: prepared.info } : {}) };
    }
    case 'publish': {
      // A façade speaking for several conversations names them all, and the turn names which one.
      let adopted = null;
      if (Array.isArray(params.client_refs) && !params.client_ref) {
        params = { ...params, client_ref: routeSpeech(params) };
        if (params.adopt_orphans && !params.client_refs.includes(params.client_ref)) adopted = adopt(client, params.client_ref);
      }
      const binding = bindings.get(params.binding_id) || [...bindings.values()].find(b => b.client_ref === params.client_ref);
      // Why it is gone travels with the refusal: a pairing revoked and a channel closed are not the
      // same news for the conversation, and only it can say the right one to the person.
      if (!binding) throw new Error(closedByRoom.has(params.client_ref) ? 'CLOSED_BY_ROOM:' + closedByRoom.get(params.client_ref) + ':' + params.client_ref : 'Unknown binding');
      const speech = { event_id: params.event_id || randomUUID(), binding_id: binding.binding_id,
        session_id: params.session_id, revision: params.revision, utterance_id: params.utterance_id || randomUUID(), text: params.text, language: params.language };
      outbox.push(speech); saveOutbox();
      // What the room never confirmed stays in the outbox and goes again on the next welcome; the
      // conversation is told it is queued rather than left waiting on a room that is not there.
      const reply = await publish(speech).catch(() => null);
      if (!reply) return { status: 'queued', utterance_id: speech.utterance_id };
      const { type, event_id, ...result } = reply;
      return adopted ? { ...result, adopted } : result;
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
    case 'adopt': return adopt(client, params.client_ref) || { adopted: false };
    case 'node.status': return nodeStatus();
    case 'node.ensure': await startCore(); return nodeStatus();
    case 'node.restart': return nodeRestart();
    case 'handover': return yieldToSupervisor(client);
    case 'status': return snapshot();
    case 'pair_device': return devicePairingCode();
    default: throw new Error('Unknown connector command');
  }
}

function serve(socket) {
  const client = { socket, bindings: new Set() };
  clients.add(client);

  let buffer = '';
  socket.on('data', chunk => {
    buffer += chunk;
    if (buffer.length > 1 << 20) { socket.destroy(); return; }
    let index;
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
      if (!line.trim()) continue;
      let input; try { input = JSON.parse(line); } catch { socket.write(JSON.stringify({ ok: false, error: 'Invalid JSON' }) + '\n'); continue; }
      // Handing over: nothing new is started here; the façade asks the supervisor instead.
      if (handingOver) { socket.write(JSON.stringify({ id: input.id, ok: false, error: 'HANDOVER: this connector is handing over to the node service' }) + '\n'); continue; }
      if (!PROBES.has(input.method) && !client.engaged) { client.engaged = true; clearTimeout(idleTimer); log(`façade attached (${engaged()} now)`); }
      const job = command(client, input);
      inFlight.add(job); job.catch(() => {}).finally(() => inFlight.delete(job));
      job.then(result => { if (!socket.destroyed) socket.write(JSON.stringify({ id: input.id, ok: true, result }) + '\n'); })
        .catch(error => { if (!socket.destroyed) socket.write(JSON.stringify({ id: input.id, ok: false, error: error.message }) + '\n'); });
    }
  });
  socket.on('error', () => {});
  socket.on('close', () => {
    clients.delete(client);
    // Handed over: its conversations are the supervisor's now, not gone.
    if (client.handedOver) return;
    if (client.engaged) log(`façade detached (${engaged()} left); dropping ${client.bindings.size} binding(s)`);
    // The façade is gone: so is every conversation it spoke for.
    for (const binding of client.bindings) {
      // A conversation whose delivery does not go through its façade (an editor chat's card) outlives it:
      // Cursor replaces a window's MCP process at will, and the chat, its card and its voice are still there.
      if (binding.detachable) { binding.owner = null; binding.orphanedAt = Date.now(); keepOrphans(); log(`${binding.thread} kept without a façade (its card delivers)`); continue; }
      bindings.delete(binding.binding_id); unwatch(binding); if (!binding.binding_id.startsWith('local-')) send('binding.unregister', { binding_id: binding.binding_id });
    }
    // A probe leaving changes nothing: the idle timer runs on as it was.
    if (client.engaged) scheduleExit();
  });
}

/** `sidevoice connector [--supervise]`. */
export async function run(argv = [], environment = process.env) {
  env = environment;
  supervised = argv.includes('--supervise');
  dataDir = dataDirOf(env);
  files = nodeFiles(dataDir);
  socketPath = connectorSocketOf(env);
  lockPath = socketPath + '.lock';
  outboxPath = path.join(dataDir, 'outbox.json');
  credentialsPath = roomCredentialPath(dataDir, env);
  idleMs = Number(env.SIDEVOICE_CONNECTOR_IDLE_MS || 15_000);
  // Who this machine is, said at every connection: the room keeps the latest and lists it.
  identity = machineIdentity(env);
  hostId = identity.host;
  logPath = supervised ? files.serviceLog : env.SIDEVOICE_CONNECTOR_LOG || files.connectorLog;
  // Which manager runs this supervisor: its definition says (`service.mjs`); detached, none.
  serviceKind = supervised ? (env.SIDEVOICE_SERVICE || 'none') : 'none';
  external = externalCore();

  // A plain connector gives way to the supervisor; one started in the gap of a takeover (a façade's launcher
  // finding no socket for a moment) is taken over in turn.
  // Handed over to by another supervisor (`--after <pid>`): that one leaves first.
  const after = Number(argv[argv.indexOf('--after') + 1]);
  if (argv.includes('--after')) { const until = Date.now() + 30_000; while (isProcess(after) && Date.now() < until) await wait(100); }
  // Refused, not repaired: a data directory others can write into is not one to serve from (`secure-fs.mjs`).
  try { verifyPrivateDir(dataDir, { create: true }); }
  catch (error) { log(`not starting: ${error.message}`); process.exitCode = 78; return; }
  for (let attempt = 0; !acquireLock(); attempt++) {
    if (!supervised || attempt >= 5 || !(await takeOver(readLock(lockPath)))) process.exit(0);
  }
  if (supervised) {
    // A supervisor starting is the next login, or a person's start: a stop no longer holds.
    try { rmSync(files.stopped, { force: true }); } catch {}
    const restored = readJson(files.status);
    supervisor = superviseWith(restored);
    // A restart a person asked the service manager for closes the window, as one asked of this process would.
    if (existsSync(files.restart)) { supervisor.closeWindow(); try { rmSync(files.restart, { force: true }); } catch {} }
    // An install transaction that died is reconciled before anything runs from it — unless an installer holds
    // the lock now, which then does it. Looked at again while a journal waits.
    await checkInstallJournal();
    if (closed) return;
    setInterval(() => { if (!pendingJournal) checkInstallJournal().catch(() => {}); }, 5000).unref();
  }
  log(`connector ${VERSION} starting${supervised ? ' as the node service (' + serviceKind + ')' : ''}: pid ${process.pid}, host ${hostId}, ${external ? 'core at ' + external.room : 'this machine\'s own core'}, socket ${socketPath}, log ${logPath}`);
  loadOutbox();
  if (outbox.length) log(`${outbox.length} speech frame(s) waiting in the outbox`);
  await restoreHandover();
  try { unlinkSync(socketPath); } catch {}
  server = net.createServer(serve);
  // Created 0600 (a umask that lets nothing through while it is bound), and checked: a socket another user could
  // open is not served on — every command on it is this user's authority.
  const umask = process.umask(0o077);
  try { await new Promise((resolve, reject) => server.once('error', reject).listen(socketPath, resolve)); }
  finally { process.umask(umask); }
  chmodSync(socketPath, 0o600);
  const bound = lstatSync(socketPath);
  if (!bound.isSocket() || bound.uid !== process.getuid() || (bound.mode & 0o077)) { log(`not serving: ${socketPath} is not this user's alone`); server.close(); process.exitCode = 78; return; }
  process.on('SIGTERM', shutdown); process.on('SIGINT', shutdown);
  scheduleExit();
  if (supervisor && !external) supervisor.boot();
  else open();
}
