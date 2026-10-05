// A fake Sidevoice Core: the connector side of Core↔Connector protocol v3, and nothing else.
//
// The wire is the real one (sidevoice-core rust/server/connectors_v3.rs at 0a9da38): JSON-RPC 2.0 text frames on
// a WebSocket at /api/connectors/v3, served on the Unix socket that core.json names, after a connector.hello that
// carries the credential written in core.json. A real connector binary links to it unchanged.
//
// What Core does with what it receives is modelled only as far as a connector can observe it: bindings, the input
// journal (push delivery with retries, pull with claims and acknowledgements), receipts, and speech. Every frame
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
    this.peer = null; // { ws, hello, connectedAt, pending: Map }
    this.bindings = new Map(); // binding_id → binding
    this.messages = []; // input journal, oldest first
    this.speech = [];
    this.log = [];
    this.serial = 0;
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
    const conn = { ws, hello: null, pending: new Map(), inFlight: new Set(), serial: 0 };
    const helloTimer = setTimeout(() => {
      this.#record('event', null, 'hello timeout');
      ws.close(1008, 'hello required');
    }, HELLO_TIMEOUT_MS);
    ws.on('message', (data, isBinary) => {
      if (isBinary) return ws.close(1003, 'text frames only');
      let frame;
      try {
        frame = JSON.parse(data.toString('utf8'));
      } catch {
        this.#record('in', data.toString('utf8'), 'invalid JSON');
        return ws.close(1002, 'invalid JSON');
      }
      this.#record('in', frame);
      if (!conn.hello) {
        clearTimeout(helloTimer);
        return this.#hello(conn, frame);
      }
      this.#onFrame(conn, frame);
    });
    ws.on('close', (code, reason) => {
      clearTimeout(helloTimer);
      for (const waiter of conn.pending.values()) waiter.reject(keyed('bench.disconnected'));
      conn.pending.clear();
      if (this.peer === conn) {
        this.peer = null;
        this.#record('event', null, `connector disconnected (${code}${reason?.length ? ` ${reason}` : ''})`);
        this.#detachAll();
      }
    });
    ws.on('error', () => {});
  }

  #hello(conn, frame) {
    const params = frame?.params;
    const fail = (code, message) => {
      this.#send(conn.ws, { jsonrpc: '2.0', id: validId(frame?.id) ? frame.id : null, error: { code, message } });
      conn.ws.close(1008, message);
    };
    if (frame?.jsonrpc !== '2.0' || frame.method !== 'connector.hello' || !validId(frame.id) || !isObject(params)) {
      return fail(-32002, 'Protocol 3 hello is required');
    }
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
    const previous = this.peer;
    conn.hello = {
      connector_id: cid,
      host: str(params.host, 200),
      platform: str(params.platform, 60),
      version: str(params.version, 40),
      harnesses: Array.isArray(params.harnesses) ? params.harnesses.filter((h) => str(h, 40)).slice(0, 8) : [],
    };
    conn.connectedAt = this.now();
    this.peer = conn;
    if (previous) {
      this.peer = conn; // set first, so the old peer's close does not detach the new one's bindings
      previous.ws.close(1000, 'replaced');
      this.#detachAll();
    }
    this.#send(conn.ws, { jsonrpc: '2.0', id: frame.id, result: { protocol: PROTOCOL } });
    this.#send(conn.ws, { jsonrpc: '2.0', method: 'connector.welcome', params: { protocol: PROTOCOL } });
    this.#send(conn.ws, { jsonrpc: '2.0', method: 'node.rendezvous', params: this.rendezvous });
    this.#emitState();
  }

  #onFrame(conn, frame) {
    const close = (why) => {
      this.#record('event', null, `protocol error: ${why}`);
      conn.ws.close(1002, why);
    };
    if (!isObject(frame) || frame.jsonrpc !== '2.0') return close('jsonrpc must be 2.0');
    if ('method' in frame) {
      if (typeof frame.method !== 'string' || !frame.method || frame.method.length > 128) return close('bad method');
      if ('params' in frame && !isObject(frame.params)) return close('params must be an object');
      if (!('id' in frame)) return this.#notification(frame.method, frame.params ?? {});
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
    if (('result' in frame) === ('error' in frame)) return close('response needs exactly one of result/error');
    const waiter = conn.pending.get(String(frame.id));
    if (!waiter) return; // a late answer to a request that timed out
    conn.pending.delete(String(frame.id));
    if ('error' in frame) waiter.reject(Object.assign(new Error(JSON.stringify(frame.error)), { rpc: frame.error }));
    else waiter.resolve(frame.result);
  }

  // ---- Connector → Core ------------------------------------------------------------------------------------

  #notification(method, params) {
    // Real Core answers no notifications; it also treats the same methods as requests (with a null result).
    this.#request(this.peer, method, params, true).catch(() => {});
  }

  async #request(conn, method, params, notification = false) {
    if (conn !== this.peer) throw new RpcError(-32001, 'room.connector_disconnected');
    const policy = this.policies[method] ?? 'normal';
    if (policy === 'silent') return SILENT;
    if (policy === 'error') throw new RpcError(-32603, `bench.policy_error:${method}`);
    switch (method) {
      case 'binding.register':
        return this.#register(params, policy);
      case 'binding.unregister':
        return this.#unregister(params);
      case 'speech.publish':
        return this.#publish(params, policy);
      case 'input.working':
        return this.#working(params);
      case 'input.engine':
        return this.#engine(params);
      case 'input.read':
        return this.#read(params);
      case 'input.pull':
        return this.#pull(params);
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

  #register(params, policy) {
    if (policy === 'refuse') return { error: 'room.thread_invalid' };
    const thread = params.thread;
    if (typeof thread !== 'string' || !THREAD.test(thread)) return { error: 'room.thread_invalid' };
    const mode = params.input_mode ?? 'push';
    if (mode !== 'push' && mode !== 'pull') return { error: 'room.input_mode_invalid' };
    let id = typeof params.binding_id === 'string' && params.binding_id ? params.binding_id : null;
    const existing = id ? this.bindings.get(id) : null;
    if (existing && existing.thread !== thread) return { error: 'room.binding_foreign' };
    id ??= randomUUID();
    if (existing) this.#releaseClaims(id);
    const engine = isObject(params.engine) && typeof params.engine.model === 'string' ? params.engine : null;
    const capabilities = normalizeCapabilities(params.capabilities);
    const binding = {
      binding_id: id,
      thread,
      client_ref: str(params.client_ref, 200),
      harness: str(params.harness, 40) || 'unknown',
      title: str(params.title, 200) ?? '',
      input_mode: mode,
      capabilities,
      experimental: Array.isArray(params.experimental)
        ? params.experimental.filter((name) => capabilities[name] === 'supported')
        : [],
      engine: engine ?? existing?.engine ?? null,
      route: ROUTES.includes(params.route) ? params.route : null,
      inbound: isObject(params.inbound) ? params.inbound : null,
      delivery: isObject(params.delivery) ? params.delivery : null, // not read by real Core; shown for diagnosis
      live: true,
      working: existing?.working ?? null,
      turn: existing?.turn ?? null,
      registered_at: this.now(),
      seq: ++this.serial,
    };
    this.bindings.set(id, binding);
    this.#emitState();
    return { client_ref: binding.client_ref, binding_id: id, thread };
  }

  #unregister(params) {
    const binding = this.bindings.get(params.binding_id);
    if (binding) {
      binding.live = false;
      this.#releaseClaims(binding.binding_id);
      this.#emitState();
    }
    return null;
  }

  #publish(params, policy) {
    const { binding_id: bindingId, event_id: eventId, utterance_id: utteranceId } = params;
    if (!str(eventId, 200) || !eventId || !str(utteranceId, 200) || !utteranceId) {
      throw new RpcError(-32602, 'Invalid params');
    }
    if (!Number.isInteger(params.revision) || params.revision < 0) throw new RpcError(-32602, 'Invalid params');
    const echo = { event_id: eventId, utterance_id: utteranceId };
    const binding = this.bindings.get(bindingId);
    if (policy === 'unknown_binding' || !binding || !binding.live) return { status: 'unknown_binding', ...echo };
    const text = params.text;
    const language = params.language ?? null;
    if (
      policy === 'rejected' ||
      typeof text !== 'string' ||
      !text ||
      Buffer.byteLength(text) > 6000 ||
      (language !== null && !LANGUAGES.includes(language))
    ) {
      return {
        status: 'rejected',
        error: 'room.speech_invalid',
        terminal: true,
        reason_code: 'application_refusal',
        ...echo,
      };
    }
    const repeat = this.speech.find((item) => item.utterance_id === utteranceId);
    if (repeat) return { status: repeat.status, text_saved: true, ...echo };
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

  #working(params) {
    const binding = this.bindings.get(params.binding_id);
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

  #engine(params) {
    const binding = this.bindings.get(params.binding_id);
    if (binding && isObject(params.engine) && params.engine.model) {
      const { model, effort = null, thinking = null } = params.engine;
      binding.engine = { model, effort, thinking };
      this.#emitState();
    }
    return null;
  }

  #read(params) {
    const binding = this.bindings.get(params.binding_id);
    const message = this.messages.find((m) => m.message_id === params.message_id);
    if (binding && message && message.thread === binding.thread && message.status !== 'read') {
      this.#receipt(message, 'read', 'input.read');
    }
    return null;
  }

  #pull(params) {
    const binding = this.bindings.get(params.binding_id);
    if (!binding || !binding.live || binding.input_mode !== 'pull') throw new RpcError(-403, 'room.pull_binding_invalid');
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
    const summary = { connected: true, pending: open().length > 0, count: open().length, fresh: fresh() };
    if (operation === 'check') return summary;
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

  /** What `POST /api/presentation/close` does in real Core: the room closes a conversation. */
  closeBinding(bindingId, reason = 'closed_from_room') {
    const binding = this.bindings.get(bindingId);
    if (!binding) throw keyed('bench.unknown_binding');
    binding.live = false;
    this.#releaseClaims(bindingId);
    if (this.peer) this.notify('binding.close', { binding_id: bindingId, thread: binding.thread, reason });
    this.#emitState();
  }

  /** Drops the connector's link, as a Core restart would; the connector reconnects and replays by itself. */
  dropLink() {
    if (!this.peer) throw keyed('bench.no_connector_linked');
    this.peer.ws.terminate();
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

  /** What the person says, as text standing in for voice. Queued for the thread's current binding. */
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

  // The newest live binding on a thread is the one input goes to (real Core: the newest active, live binding).
  #target(thread) {
    let best = null;
    for (const binding of this.bindings.values()) {
      if (binding.thread === thread && binding.live && (!best || binding.seq > best.seq)) best = binding;
    }
    return best;
  }

  #releaseClaims(bindingId) {
    for (const message of this.messages) {
      if (message.claimed_by === bindingId && message.status === 'delivered') {
        message.claimed_by = null;
        this.#receipt(message, 'pending', 'claim released');
      }
    }
  }

  #detachAll() {
    for (const binding of this.bindings.values()) {
      if (binding.live) {
        binding.live = false;
        this.#releaseClaims(binding.binding_id);
      }
    }
    this.#emitState();
  }

  #pump() {
    const now = this.now();
    const busy = new Set(this.messages.filter((m) => m.in_flight).map((m) => m.thread));
    for (const message of this.messages) {
      if (message.status !== 'pending' || message.in_flight) continue;
      if (now - message.created_at > INPUT_TTL_MS) {
        this.#receipt(message, 'not_sent', 'expired');
        continue;
      }
      if (busy.has(message.thread) || now < message.next_attempt || !this.peer) continue;
      const binding = this.#target(message.thread);
      if (!binding || binding.input_mode !== 'push') continue;
      busy.add(message.thread);
      this.#deliver(message, binding);
    }
  }

  async #deliver(message, binding) {
    message.in_flight = true;
    message.attempts += 1;
    message.binding_id = binding.binding_id;
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
    let ack;
    try {
      ack = await this.request('input.deliver', params, DELIVER_TIMEOUT_MS);
    } catch (error) {
      ack = { status: 'failed', error: error.message };
    }
    message.in_flight = false;
    message.last_ack = ack;
    if (message.status === 'read') return this.#emitState(); // input.read arrived before the acknowledgement
    const status = isObject(ack) ? ack.status : undefined;
    if (status === 'accepted') this.#receipt(message, 'delivered', 'input.deliver', ack.detail);
    else if (status === 'unknown') this.#receipt(message, 'unconfirmed', 'input.deliver', ack.detail);
    else if (status === 'unsupported') this.#receipt(message, 'not_sent', 'input.deliver', ack.detail);
    else {
      const delay = RETRY_DELAYS_MS[Math.min(message.attempts - 1, RETRY_DELAYS_MS.length - 1)];
      message.next_attempt = this.now() + delay;
      message.receipts.push({ status: 'retry', at: this.now(), via: 'input.deliver', detail: JSON.stringify(ack) });
      this.#emitState();
    }
  }

  // ---- state -----------------------------------------------------------------------------------------------

  snapshot() {
    return {
      core: { launch_id: this.launchId, socket: this.socketPath, connector_id: this.connectorId, protocol: PROTOCOL },
      connector: this.peer ? { ...this.peer.hello, connected_at: this.peer.connectedAt } : null,
      rendezvous: this.rendezvous,
      session_id: this.sessionId,
      policies: this.policies,
      policy_choices: POLICIES,
      bindings: [...this.bindings.values()].sort((a, b) => b.seq - a.seq),
      messages: this.messages.map(({ in_flight: inFlight, ...m }) => ({ ...m, in_flight: inFlight })),
      speech: this.speech,
    };
  }

  #emitState() {
    this.emit('state');
  }
}

const SILENT = Symbol('silent');
