// A fake Sidevoice Core: the connector side of Core↔Connector protocol v3, and nothing else.
//
// The wire is the real one (sidevoice-core rust/server/connectors_v3.rs at 0a9da38): JSON-RPC 2.0 text frames on
// a WebSocket at /api/connectors/v3, served on the Unix socket that core.json names, after a connector.hello that
// carries the credential written in core.json. A real connector binary links to it unchanged.
//
// What Core does with what it receives is modelled only as far as a connector can observe it: bindings, the input
// journal (push delivery with retries, pull with claims and acknowledgements), receipts, and speech. Where it
// models something, it follows real Core's code (room.rs `register`, `attach`/`detach`, the delivery loop,
// `valid_delivery_ack`, `publish`, `read`, `close_channel`); where it does not (calls, audience, devices, the
// hosted room), it says so in docs/TESTING-SEAMS.md. Every frame
// in either direction is kept in a bounded log and emitted as an event, so a UI or a test can see the raw protocol.
// How Core answers each connector request can be overridden (`setPolicy`), to drive the connector's failure paths.

import { EventEmitter } from 'node:events';
import { randomBytes, randomUUID } from 'node:crypto';
import { chmodSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { WebSocketServer } from 'ws';

export const PROTOCOL = 3;
const MAX_FRAME = 1 << 20;
const HELLO_TIMEOUT_MS = 10_000;
const DELIVER_TIMEOUT_MS = 60_000;
const RETRY_DELAYS_MS = [2_000, 5_000, 15_000, 60_000];
const INPUT_TTL_MS = 600_000;
const PULL_PAGE = 32;
const LOG_LIMIT = 1000;
const THREAD = /^[A-Za-z0-9._:-]{1,200}$/;
const CAPABILITIES = ['deliver', 'inspectInbound', 'working', 'endOfTurn', 'sessionIdentity'];
const ROUTES = ['cursor-editor-bridge', 'cursor-editor-view', 'cursor-cli-persist', 'cursor-cli'];
const LANGUAGES = ['es', 'en', 'fr', 'it', 'pt', 'hi'];

// How the fake Core may answer each connector request. `normal` is what real Core does on the happy path.
export const POLICIES = {
  'binding.register': ['normal', 'refuse', 'error', 'silent'],
  'speech.publish': ['normal', 'text_only', 'unknown_binding', 'rejected', 'error', 'silent'],
  'input.pull': ['normal', 'error', 'silent'],
  'device.pairing_code': ['normal', 'error', 'silent'],
};

/** A refusal a person may see: a stable i18n key (messages/en.json `error.<key>`) and its parameters. */
export function keyed(key, params = {}) {
  return Object.assign(new Error(key), { key, params });
}

class RpcError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const validId = (id) =>
  (typeof id === 'string' && id.length > 0 && id.length <= 100) ||
  (Number.isInteger(id) && Math.abs(id) <= Number.MAX_SAFE_INTEGER);
const str = (value, max) => (typeof value === 'string' && value.length <= max ? value : null);

function normalizeCapabilities(raw) {
  const out = {};
  for (const name of CAPABILITIES) {
    const value = isObject(raw) ? raw[name] : undefined;
    out[name] = value === 'supported' || value === 'unsupported' ? value : 'unknown';
  }
  return out;
}

// Real Core's acknowledgement check (room.rs `valid_delivery_ack`): anything else is a failed delivery, retried.
function validDeliveryAck(value) {
  if (!isObject(value)) return null;
  if (Buffer.byteLength(JSON.stringify(value)) > 4096) return null;
  if (Object.keys(value).some((key) => !['status', 'detail', 'error'].includes(key))) return null;
  for (const key of ['detail', 'error']) {
    if (key in value && (typeof value[key] !== 'string' || value[key].length > 1000)) return null;
  }
  return ['accepted', 'unknown', 'unsupported', 'failed', 'unknown_binding'].includes(value.status)
    ? value.status
    : null;
}

const REJECTED = { status: 'rejected', error: 'room.speech_invalid', terminal: true, reason_code: 'application_refusal' };

export class FakeCore extends EventEmitter {
  constructor({ dataDir, connectorId = 'bench-connector', token, version = 'bench', now = Date.now } = {}) {
    super();
    if (!dataDir) throw new Error('dataDir is required');
    this.dataDir = dataDir;
    this.socketPath = path.join(dataDir, 'local.sock');
    this.readyPath = path.join(dataDir, 'core.json');
    this.connectorId = connectorId;
    this.token = token ?? randomBytes(32).toString('base64url');
    this.version = version;
    this.now = now;
    this.launchId = randomUUID();
    this.policies = Object.fromEntries(Object.keys(POLICIES).map((method) => [method, 'normal']));
    this.rendezvous = { room: 'bench://local', connected: true, via: 'bench', error: null, refused: null };
    this.peer = null; // the current connection: { ws, hello, generation, pending, inFlight, serial }
    this.bindings = new Map(); // binding_id → binding
    this.inflight = new Map(); // binding_id → message_id: one delivery in flight per binding, as in real Core
    this.messages = []; // input journal, oldest first
    this.speech = [];
    this.log = [];
    this.serial = 0;
    this.created = 0;
    this.cursor = 0;
    this.revision = 0;
    this.sessionId = randomUUID();
    this.server = null;
    this.timer = null;
  }

  // ---- lifecycle -------------------------------------------------------------------------------------------

  async start() {
    rmSync(this.socketPath, { force: true });
    this.wss = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME });
    this.server = http.createServer((req, res) => this.#http(req, res));
    this.server.on('upgrade', (req, socket, head) => this.#upgrade(req, socket, head));
    await new Promise((resolve, reject) => {
      this.server.once('error', reject);
      this.server.listen(this.socketPath, resolve);
    });
    chmodSync(this.socketPath, 0o600);
    this.#writeReady();
    this.timer = setInterval(() => this.#pump(), 250);
    this.timer.unref?.();
    this.#emitState();
    return this;
  }

  async stop() {
    clearInterval(this.timer);
    this.peer?.ws.terminate();
    this.peer = null;
    rmSync(this.readyPath, { force: true });
    await new Promise((resolve) => (this.server ? this.server.close(() => resolve()) : resolve()));
    this.wss?.close();
    rmSync(this.socketPath, { force: true });
  }

  /**
   * What a Core restart looks like to the connector: the link drops, the launch id changes, and every binding is
   * gone (real Core keeps them in memory only). The journal is in memory too: what had not been delivered is lost,
   * shown here as not sent so the history stays readable. The connector must re-register and re-key its outbox.
   */
  restart() {
    this.peer?.ws.terminate();
    this.peer = null;
    this.bindings.clear();
    this.inflight.clear();
    for (const message of this.messages) {
      if (message.status === 'pending') this.#receipt(message, 'not_sent', 'core restarted');
      message.claimed_by = null;
      message.in_flight = false;
    }
    this.launchId = randomUUID();
    this.#writeReady();
    this.#record('event', null, 'core restarted');
    this.#emitState();
  }

  /** Drops the link and nothing else, as a network blip would; the connector reconnects and replays. */
  dropLink() {
    if (!this.peer) throw keyed('bench.no_connector_linked');
    this.peer.ws.terminate();
  }

  #writeReady() {
    // The fields the connector reads (connector-rust src/proof.rs `Ready`), plus the rest of real Core's ready
    // file so a reader that expects them is not surprised.
    const ready = {
      pid: process.pid,
      port: 0,
      url: null,
      socket: this.socketPath,
      launch_id: this.launchId,
      version: this.version,
      api: 1,
      protocol: 2,
      connector_protocols: [PROTOCOL],
      connector_id: this.connectorId,
      token: this.token,
    };
    const tmp = `${this.readyPath}.${process.pid}.tmp`;
    writeFileSync(tmp, JSON.stringify(ready), { mode: 0o600 });
    renameSync(tmp, this.readyPath);
  }

  #http(req, res) {
    // Bodies carry a Content-Length: the connector reads the health answer as raw HTTP/1.1, not chunked.
    const reply = (status, value) => {
      const body = Buffer.from(JSON.stringify(value));
      res.writeHead(status, { 'content-type': 'application/json', 'content-length': body.length, connection: 'close' });
      res.end(body);
    };
    if (!req.headers.origin && req.method === 'GET' && req.url === '/api/local/health') {
      reply(200, {
        launch_id: this.launchId,
        pid: process.pid,
        version: this.version,
        api: 1,
        fingerprint: 'bench',
        public_key: null,
        host: 'bench',
        calls: 0,
      });
      return;
    }
    reply(404, { error: 'not_found' });
  }

  #upgrade(req, socket, head) {
    if (req.url !== '/api/connectors/v3' || req.headers.origin) {
      socket.end('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n');
      return;
    }
    this.wss.handleUpgrade(req, socket, head, (ws) => this.#accept(ws));
  }

  // ---- the link --------------------------------------------------------------------------------------------

  #record(direction, frame, note) {
    const entry = { seq: ++this.serial, at: this.now(), direction, frame, ...(note ? { note } : {}) };
    this.log.push(entry);
    if (this.log.length > LOG_LIMIT) this.log.shift();
    this.emit('frame', entry);
  }

  #send(ws, frame) {
    this.#record('out', frame);
    ws.send(JSON.stringify(frame));
  }

  #accept(ws) {
    const conn = { ws, hello: null, generation: randomUUID(), pending: new Map(), inFlight: new Set(), serial: 0 };
    const helloTimer = setTimeout(() => {
      this.#record('event', null, 'hello timeout');
      ws.close(1008);
    }, HELLO_TIMEOUT_MS);
    ws.on('message', (data, isBinary) => {
      if (isBinary) return ws.close(conn.hello ? 1003 : 1008);
      let frame;
      try {
        frame = JSON.parse(data.toString('utf8'));
      } catch {
        this.#record('in', data.toString('utf8'), 'invalid JSON');
        return ws.close(conn.hello ? 1002 : 1008);
      }
      this.#record('in', frame);
      if (!conn.hello) {
        clearTimeout(helloTimer);
        return this.#hello(conn, frame);
      }
      this.#onFrame(conn, frame);
    });
    ws.on('close', (code) => {
      clearTimeout(helloTimer);
      for (const waiter of conn.pending.values()) waiter.reject(keyed('bench.disconnected'));
      conn.pending.clear();
      if (this.peer === conn) {
        this.peer = null;
        this.#record('event', null, `connector disconnected (${code})`);
        this.#detach(false);
      }
    });
    ws.on('error', () => {});
  }

  #hello(conn, frame) {
    // Real Core (connectors_v3.rs `run`): a first frame that is not a connector.hello request is closed with 1008
    // and no answer; a hello with the wrong protocol or credential is answered with an error, then closed.
    const params = isObject(frame?.params) ? frame.params : {};
    if (!isObject(frame) || frame.jsonrpc !== '2.0' || frame.method !== 'connector.hello' || !validId(frame.id)) {
      return conn.ws.close(1008);
    }
    const fail = (code, message) => {
      this.#send(conn.ws, { jsonrpc: '2.0', id: frame.id, error: { code, message } });
      conn.ws.close(1008);
    };
    const cid = params.connector_id;
    if (
      params.protocol !== PROTOCOL ||
      typeof cid !== 'string' ||
      !cid ||
      cid.length > 200 ||
      typeof params.token !== 'string' ||
      !params.token ||
      params.token.length > 512
    ) {
      return fail(-32002, 'Protocol 3 hello is required');
    }
    if (cid !== this.connectorId || params.token !== this.token) {
      return fail(-32001, 'Connector credential refused');
    }
    conn.hello = {
      connector_id: cid,
      host: str(params.host, 200),
      platform: str(params.platform, 60),
      version: str(params.version, 40),
      harnesses: Array.isArray(params.harnesses) ? params.harnesses.filter((h) => str(h, 40)).slice(0, 8) : [],
    };
    conn.connectedAt = this.now();
    const previous = this.peer;
    this.peer = conn; // first, so the old connection's close is not taken for the current one's
    this.#detach(true);
    previous?.ws.close(1000);
    this.#send(conn.ws, { jsonrpc: '2.0', id: frame.id, result: { protocol: PROTOCOL } });
    this.#send(conn.ws, { jsonrpc: '2.0', method: 'connector.welcome', params: { protocol: PROTOCOL } });
    this.#send(conn.ws, { jsonrpc: '2.0', method: 'node.rendezvous', params: this.rendezvous });
    this.#emitState();
  }

  // Real Core `attach` (a new connection, onlyActive) and `detach` (the connection went away): the connector's
  // bindings stop being live, their pull claims are released, and a delivery in flight goes straight back to
  // pending, with no attempt counted and no back-off.
  #detach(onlyActive) {
    for (const binding of this.bindings.values()) {
      if (onlyActive && !binding.active) continue;
      binding.live = false;
      this.#releaseClaims((m) => m.claimed_by === binding.binding_id);
      const messageId = this.inflight.get(binding.binding_id);
      this.inflight.delete(binding.binding_id);
      const message = this.messages.find((m) => m.message_id === messageId);
      if (message) {
        message.in_flight = false;
        message.next_attempt = 0;
      }
    }
    this.#emitState();
  }

  #onFrame(conn, frame) {
    const close = (why) => {
      this.#record('event', null, `protocol error: ${why}`);
      conn.ws.close(1002);
    };
    if (!isObject(frame) || frame.jsonrpc !== '2.0') return close('jsonrpc must be 2.0');
    if ('method' in frame) {
      if (typeof frame.method !== 'string' || !frame.method || frame.method.length > 128) return close('bad method');
      if ('params' in frame && !isObject(frame.params)) return close('params must be an object');
      if (!('id' in frame)) return this.#notification(conn, frame.method, frame.params ?? {});
      if (!validId(frame.id)) return close('bad id');
      const key = JSON.stringify(frame.id);
      if (conn.inFlight.has(key)) return close('duplicate in-flight id');
      if (conn.inFlight.size >= 32) {
        return this.#send(conn.ws, {
          jsonrpc: '2.0',
          id: frame.id,
          error: { code: -32000, message: 'Connector request capacity reached' },
        });
      }
      conn.inFlight.add(key);
      Promise.resolve()
        .then(() => this.#request(conn, frame.method, frame.params ?? {}))
        .then(
          (result) => {
            if (result === SILENT) return;
            if (conn.ws.readyState === conn.ws.OPEN) this.#send(conn.ws, { jsonrpc: '2.0', id: frame.id, result });
          },
          (error) => {
            const code = error instanceof RpcError ? error.code : -32603;
            if (conn.ws.readyState === conn.ws.OPEN) {
              this.#send(conn.ws, { jsonrpc: '2.0', id: frame.id, error: { code, message: error.message } });
            }
          },
        )
        .finally(() => conn.inFlight.delete(key));
      return;
    }
    if (!validId(frame.id)) return close('response without id');
    if ('result' in frame === 'error' in frame) return close('response needs exactly one of result/error');
    const waiter = conn.pending.get(String(frame.id));
    if (!waiter) return; // a late answer to a request that timed out
    conn.pending.delete(String(frame.id));
    if ('error' in frame) waiter.reject(Object.assign(new Error(JSON.stringify(frame.error)), { rpc: frame.error }));
    else waiter.resolve(frame.result);
  }

  // ---- Connector → Core ------------------------------------------------------------------------------------

  #notification(conn, method, params) {
    // Real Core answers no notifications; frames from a connection that is no longer current are ignored.
    this.#request(conn, method, params, true).catch(() => {});
  }

  async #request(conn, method, params, notification = false) {
    if (conn !== this.peer) throw new RpcError(-32001, 'room.connector_disconnected');
    const policy = this.policies[method] ?? 'normal';
    if (policy === 'silent') return SILENT;
    if (policy === 'error') throw new RpcError(-32603, `bench.policy_error:${method}`);
    switch (method) {
      case 'binding.register':
        return this.#register(conn, params, policy);
      case 'binding.unregister':
        return this.#unregister(conn, params);
      case 'speech.publish':
        return this.#publish(conn, params, policy);
      case 'input.working':
        return this.#working(conn, params);
      case 'input.engine':
        return this.#engine(conn, params);
      case 'input.read':
        return this.#read(conn, params);
      case 'input.pull':
        return this.#pull(conn, params);
      case 'device.pairing_code':
        return {
          code: `SV1.${randomBytes(18).toString('base64url')}`,
          payload: { v: 1, fp: 'bench', host: 'bench', urls: [], rv: null, secret: 'bench', exp: 0 },
          expires_in: 600,
        };
      default:
        if (notification) return null;
        throw new RpcError(-32601, 'Method not found');
    }
  }

  /** A binding of this connection's connector, live. */
  #own(conn, bindingId) {
    const binding = this.bindings.get(bindingId);
    return binding && binding.connector === conn.hello.connector_id && binding.live ? binding : null;
  }

  // Real Core `register` (room.rs): a known id is reused (refused if another connector's); otherwise the newest
  // active binding of this connector on the thread is reused; otherwise a new id is made. An unknown id the
  // connector sends is never adopted.
  #register(conn, params, policy) {
    if (policy === 'refuse') return { error: 'room.thread_invalid' };
    const thread = params.thread;
    if (typeof thread !== 'string' || !THREAD.test(thread)) return { error: 'room.thread_invalid' };
    const cid = conn.hello.connector_id;
    const mode = params.input_mode ?? 'push';
    if (mode !== 'push' && mode !== 'pull' && mode !== '') return { error: 'room.input_mode_invalid' };
    const pull = mode === 'pull';
    const requested = typeof params.binding_id === 'string' ? params.binding_id : '';
    const known = requested ? this.bindings.get(requested) : null;
    if (known && known.connector !== cid) return { error: 'room.binding_foreign' };
    let binding =
      known ??
      [...this.bindings.values()]
        .filter((b) => b.connector === cid && b.thread === thread && b.active)
        .sort((a, b) => b.created - a.created)[0];
    if (!binding) {
      binding = {
        binding_id: randomUUID(),
        connector: cid,
        thread,
        title: '',
        created: ++this.created,
        engine: null,
        inbound: null,
      };
      this.bindings.set(binding.binding_id, binding);
    }
    const capabilities = normalizeCapabilities(params.capabilities);
    const title = typeof params.title === 'string' && params.title ? params.title.slice(0, 200) : null;
    const engine = isObject(params.engine) && typeof params.engine.model === 'string' ? params.engine : null;
    Object.assign(binding, {
      client_ref: str(params.client_ref, 200),
      harness: (typeof params.harness === 'string' && params.harness ? params.harness : 'unknown').slice(0, 40),
      input_mode: pull ? 'pull' : 'push',
      capabilities,
      experimental: Array.isArray(params.experimental)
        ? params.experimental.filter((name) => capabilities[name] === 'supported')
        : [],
      route: ROUTES.includes(params.route) ? params.route : null,
      delivery: isObject(params.delivery) ? params.delivery : null, // not read by real Core; shown for diagnosis
      active: true,
      live: true,
      working: null, // real Core forgets the thread's working state on register
      turn: null,
      registered_at: this.now(),
    });
    if (title) binding.title = title;
    if (isObject(params.inbound)) binding.inbound = params.inbound;
    if (engine) binding.engine = engine;
    // Claims on the thread go back to the queue, except this binding's own when it pulls.
    this.#releaseClaims((m) => m.thread === binding.thread && (!pull || m.claimed_by !== binding.binding_id));
    this.#emitState();
    return { client_ref: params.client_ref ?? null, binding_id: binding.binding_id, thread: binding.thread };
  }

  #unregister(conn, params) {
    const binding = this.bindings.get(params.binding_id);
    if (binding && binding.connector === conn.hello.connector_id) {
      binding.active = false;
      binding.live = false;
      binding.working = null;
      this.#releaseClaims((m) => m.claimed_by === binding.binding_id);
      this.#emitState();
    }
    return null;
  }

  #publish(conn, params, policy) {
    const { binding_id: bindingId, event_id: eventId, utterance_id: utteranceId } = params;
    // connectors_v3.rs checks the ids itself, before the room sees the speech.
    if (!str(eventId, 200) || !eventId || !str(utteranceId, 200) || !utteranceId) {
      throw new RpcError(-32602, 'Invalid params');
    }
    const echo = { event_id: eventId, utterance_id: utteranceId };
    const binding = this.#own(conn, bindingId);
    if (policy === 'unknown_binding' || !binding) return { status: 'unknown_binding', ...echo };
    const { text } = params;
    const language = params.language ?? null;
    if (
      policy === 'rejected' ||
      typeof text !== 'string' ||
      !text ||
      Buffer.byteLength(text) > 6000 ||
      !Number.isInteger(params.revision) ||
      params.revision < 0 ||
      (language !== null && !LANGUAGES.includes(language))
    ) {
      return { ...REJECTED, ...echo };
    }
    const repeat = this.speech.find((item) => item.utterance_id === utteranceId);
    if (repeat) {
      return repeat.text === text && repeat.thread === binding.thread
        ? { status: repeat.status, text_saved: true, ...echo }
        : { ...REJECTED, ...echo };
    }
    const status = policy === 'text_only' ? 'text_only' : 'queued';
    const item = {
      ...echo,
      binding_id: bindingId,
      thread: binding.thread,
      session_id: params.session_id ?? null,
      revision: params.revision,
      text,
      language,
      status,
      at: this.now(),
    };
    this.speech.push(item);
    this.emit('speech', item);
    this.#emitState();
    if (status === 'text_only') return { status, text_saved: true, reason: 'session_changed', ...echo };
    return { status, session_id: item.session_id, revision: item.revision, text_saved: true, ...echo };
  }

  #working(conn, params) {
    const binding = this.#own(conn, params.binding_id);
    if (binding && typeof params.working === 'boolean') {
      binding.working = params.working;
      binding.turn = {
        turn_id: params.turn_id ?? null,
        turn_phase: params.turn_phase ?? null,
        session_id: params.session_id ?? null,
        revision: params.revision ?? null,
        at: this.now(),
      };
      this.#emitState();
    }
    return null;
  }

  #engine(conn, params) {
    const binding = this.#own(conn, params.binding_id);
    if (binding && isObject(params.engine) && params.engine.model) {
      const { model, effort = null, thinking = null } = params.engine;
      binding.engine = { model, effort, thinking };
      this.#emitState();
    }
    return null;
  }

  #read(conn, params) {
    const binding = this.#own(conn, params.binding_id);
    if (!binding) return null;
    const message = this.messages.findLast(
      (m) => m.message_id === params.message_id && m.thread === binding.thread && !['read', 'not_sent'].includes(m.status),
    );
    if (message) this.#receipt(message, 'read', 'input.read');
    return null;
  }

  #pull(conn, params) {
    const binding = this.bindings.get(params.binding_id);
    if (!binding || binding.connector !== conn.hello.connector_id || !binding.live || binding.input_mode !== 'pull') {
      throw new RpcError(-403, 'room.pull_binding_invalid');
    }
    if (this.#target(binding.thread) !== binding) throw new RpcError(-409, 'room.pull_binding_superseded');
    const { operation } = params;
    if (operation !== 'check' && operation !== 'get') throw new RpcError(-400, 'room.pull_operation_invalid');
    const ackIds = params.ack_ids;
    if (ackIds !== undefined) {
      if (
        operation === 'check' ||
        !Array.isArray(ackIds) ||
        ackIds.length > PULL_PAGE ||
        ackIds.some((id) => typeof id !== 'string' || !id)
      ) {
        throw new RpcError(-400, 'room.pull_ack_invalid');
      }
    }
    if (params.after !== undefined && (!Number.isInteger(params.after) || params.after < 0)) {
      throw new RpcError(-400, 'room.pull_cursor_invalid');
    }
    let acknowledged = 0;
    for (const id of ackIds ?? []) {
      const message = this.messages.find(
        (m) => m.message_id === id && m.thread === binding.thread && m.claimed_by === binding.binding_id,
      );
      if (message && message.status !== 'read') {
        this.#receipt(message, 'read', 'input.pull ack');
        acknowledged += 1;
      }
    }
    const open = () =>
      this.messages.filter(
        (m) =>
          m.thread === binding.thread &&
          (m.status === 'pending' || (m.status === 'delivered' && m.claimed_by === binding.binding_id)),
      );
    const fresh = () => open().filter((m) => m.status === 'pending').length;
    if (operation === 'check') return { connected: true, pending: open().length > 0, count: open().length, fresh: fresh() };
    const after = params.after ?? 0;
    const page = open().filter((m) => m.cursor > after);
    const taken = page.slice(0, PULL_PAGE);
    for (const message of taken) {
      if (message.status === 'pending') {
        message.claimed_by = binding.binding_id;
        this.#receipt(message, 'delivered', 'input.pull get');
      }
    }
    this.#emitState();
    return {
      connected: true,
      pending: open().length > 0,
      count: open().length,
      fresh: fresh(),
      messages: taken.map((m) => ({
        message_id: m.message_id,
        session_id: m.session_id,
        revision: m.revision,
        channel: 'voice',
        text: m.text,
        arrival_time: new Date(m.created_at).toISOString(),
        cursor: m.cursor,
      })),
      cursor: taken.length ? taken.at(-1).cursor : after,
      more: page.length > taken.length,
      acknowledged,
    };
  }

  // ---- Core → Connector ------------------------------------------------------------------------------------

  /** Sends a request to the linked connector and resolves with its result (rejects on error or timeout). */
  request(method, params = {}, timeoutMs = 20_000) {
    const conn = this.peer;
    if (!conn) return Promise.reject(keyed('bench.no_connector_linked'));
    const id = `s:${++conn.serial}`;
    if (conn.pending.size >= 128) return Promise.reject(keyed('bench.too_many_pending'));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        conn.pending.delete(id);
        reject(keyed('bench.request_timed_out', { method }));
      }, timeoutMs);
      conn.pending.set(id, {
        resolve: (value) => (clearTimeout(timer), resolve(value)),
        reject: (error) => (clearTimeout(timer), reject(error)),
      });
      this.#send(conn.ws, { jsonrpc: '2.0', id, method, params });
    });
  }

  notify(method, params = {}) {
    if (!this.peer) throw keyed('bench.no_connector_linked');
    this.#send(this.peer.ws, { jsonrpc: '2.0', method, params });
  }

  /** Any frame, verbatim; for probing the connector with things real Core would never send. */
  sendRaw(frame) {
    if (!this.peer) throw keyed('bench.no_connector_linked');
    this.#send(this.peer.ws, frame);
  }

  setRendezvous(update) {
    this.rendezvous = { ...this.rendezvous, ...update };
    if (this.peer) this.notify('node.rendezvous', this.rendezvous);
    this.#emitState();
  }

  /**
   * What `POST /api/presentation/close` does in real Core (room.rs `close_channel`): the thread's binding is
   * closed, what was still waiting for it is not sent, and the connector is told.
   */
  closeBinding(bindingId, reason = 'closed_from_room') {
    const binding = this.bindings.get(bindingId);
    if (!binding) throw keyed('bench.unknown_binding');
    binding.active = false;
    binding.live = false;
    binding.working = null;
    this.inflight.delete(bindingId);
    for (const message of this.messages) {
      if (message.thread === binding.thread && message.status === 'pending') {
        message.in_flight = false;
        this.#receipt(message, 'not_sent', 'channel_closed');
      }
    }
    if (this.peer) this.notify('binding.close', { binding_id: bindingId, thread: binding.thread, reason });
    this.#emitState();
  }

  setPolicy(method, mode) {
    if (!POLICIES[method]?.includes(mode)) throw keyed('bench.unknown_policy', { method, mode });
    this.policies[method] = mode;
    this.#emitState();
  }

  newSession() {
    this.sessionId = randomUUID();
    this.#emitState();
    return this.sessionId;
  }

  // ---- the input journal -----------------------------------------------------------------------------------

  /** What the person says, as text standing in for voice. Queued for the thread's delivery binding. */
  say(thread, text) {
    if (typeof thread !== 'string' || !THREAD.test(thread)) throw keyed('bench.invalid_thread');
    if (typeof text !== 'string' || !text.trim()) throw keyed('bench.empty_text');
    const message = {
      message_id: randomUUID(),
      event_id: randomUUID(),
      thread,
      text,
      session_id: this.sessionId,
      revision: ++this.revision,
      cursor: ++this.cursor,
      status: 'pending',
      created_at: this.now(),
      attempts: 0,
      next_attempt: 0,
      in_flight: false,
      claimed_by: null,
      binding_id: null,
      receipts: [{ status: 'pending', at: this.now(), via: 'say' }],
      last_ack: null,
    };
    this.messages.push(message);
    this.emit('receipt', { message_id: message.message_id, status: 'pending' });
    this.#emitState();
    return message;
  }

  #receipt(message, status, via, detail) {
    message.status = status;
    message.receipts.push({ status, at: this.now(), via, ...(detail ? { detail } : {}) });
    this.emit('receipt', { message_id: message.message_id, status });
    this.#emitState();
  }

  // Real Core `delivery_binding`: the newest active, live binding on the thread, by creation.
  #target(thread) {
    let best = null;
    for (const b of this.bindings.values()) {
      if (b.thread !== thread || !b.active || !b.live) continue;
      if (!best || b.created > best.created) best = b;
    }
    return best;
  }

  #releaseClaims(released) {
    for (const message of this.messages) {
      if (message.claimed_by && message.status === 'delivered' && released(message)) {
        message.claimed_by = null;
        this.#receipt(message, 'pending', 'claim released');
      }
    }
  }

  // Real Core's delivery loop (room.rs, every 250 ms): each pending message, oldest first, goes to the newest
  // active, live, push binding on its thread that has nothing in flight; none while the delivery binding pulls.
  #pump() {
    const now = this.now();
    for (const message of this.messages) {
      if (message.status !== 'pending' || message.in_flight) continue;
      if (now - message.created_at >= INPUT_TTL_MS) {
        this.#receipt(message, 'not_sent', 'expired');
        continue;
      }
      if (now < message.next_attempt) continue;
      if (this.#target(message.thread)?.input_mode === 'pull') continue;
      const binding = [...this.bindings.values()]
        .filter(
          (b) =>
            b.active && b.live && b.thread === message.thread && b.input_mode === 'push' && !this.inflight.has(b.binding_id),
        )
        .sort((a, b) => b.created - a.created)[0];
      if (!binding || !this.peer) continue;
      this.#deliver(message, binding, this.peer);
    }
  }

  async #deliver(message, binding, conn) {
    message.in_flight = true;
    message.binding_id = binding.binding_id;
    this.inflight.set(binding.binding_id, message.message_id);
    this.#emitState();
    const params = {
      event_id: message.event_id,
      binding_id: binding.binding_id,
      thread: message.thread,
      text: message.text,
      channel: 'voice',
      session_id: message.session_id,
      revision: message.revision,
      message_id: message.message_id,
    };
    let answer;
    try {
      answer = await this.request('input.deliver', params, DELIVER_TIMEOUT_MS);
    } catch (error) {
      answer = { failed: error.message };
    }
    // An answer from a connection that has been replaced, or for a delivery a detach already took back, is ignored.
    if (this.inflight.get(binding.binding_id) !== message.message_id || this.peer !== conn) return;
    this.inflight.delete(binding.binding_id);
    message.in_flight = false;
    message.last_ack = answer;
    if (message.status === 'read') return this.#emitState(); // input.read arrived before the acknowledgement
    const status = validDeliveryAck(answer);
    if (status === 'accepted') this.#receipt(message, 'delivered', 'input.deliver', answer.detail);
    else if (status === 'unknown') this.#receipt(message, 'unconfirmed', 'input.deliver', answer.detail);
    else if (status === 'unsupported') this.#receipt(message, 'not_sent', 'input.deliver', answer.detail);
    else {
      message.attempts += 1;
      message.next_attempt = this.now() + RETRY_DELAYS_MS[Math.min(message.attempts, 4) - 1];
      message.receipts.push({ status: 'retry', at: this.now(), via: 'input.deliver', detail: JSON.stringify(answer) });
      this.#emitState();
    }
  }

  // ---- state -----------------------------------------------------------------------------------------------

  snapshot() {
    return {
      core: { launch_id: this.launchId, socket: this.socketPath, connector_id: this.connectorId, protocol: PROTOCOL },
      connector: this.peer?.hello ? { ...this.peer.hello, connected_at: this.peer.connectedAt } : null,
      rendezvous: this.rendezvous,
      session_id: this.sessionId,
      policies: this.policies,
      policy_choices: POLICIES,
      bindings: [...this.bindings.values()].sort((a, b) => b.created - a.created),
      messages: this.messages,
      speech: this.speech,
    };
  }

  #emitState() {
    this.emit('state');
  }
}

const SILENT = Symbol('silent');
