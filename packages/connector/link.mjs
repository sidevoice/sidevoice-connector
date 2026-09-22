/** The link to the room, as one interface with one implementation.
 *
 *  `open`, `close`, `send(event, data)` for what needs no answer, `request(event, data)` for what
 *  does, and three callbacks: `onConnected` when the room has welcomed this connector, `onEvent`
 *  for what the room asks of it, `onLost` when the connection is not up. Everything above this —
 *  bindings, the outbox, the harness, the façades — is written against that and never learns what
 *  carries it.
 *
 *  What carries it is Socket.IO, and the point of choosing it is what this file therefore does not
 *  contain: an acknowledgement travels on the event that asked for it, keepalive is ping/pong, and
 *  a lost connection comes back on its own with backoff. What the library does not do is survive
 *  this process — the outbox does that, and nothing here relies on the library holding anything.
 *
 *  A credential the room refuses is not a connection problem and is not retried: the library stops
 *  by itself, `onLost` says so with `retrying: false`, and the person is told to pair again. */
import { io } from 'socket.io-client';

/** The room's address for this link. Not the default `/socket.io`: this path is what a proxy in
 *  front of the room exempts from its login, by name, and the namespace leaves the browser's own
 *  link room on the same server later. The credential stores the room's origin; the rest is ours. */
export const PATH = '/api/connectors/socket.io';
export const NAMESPACE = '/connectors';
export const UNREACHABLE = 'The room is unreachable; retrying in the background';

export function linkUrl(origin) {
  return new URL(NAMESPACE, origin).toString();
}

class RoomLink {
  constructor(options) {
    this.options = options;
    this.connected = false;
    this.closed = false;
    this.socket = null;
    this.attempts = 0;
    this.waiting = new Set();   // requests whose answer can still arrive
  }

  open() {
    const { origin, connector_id, token, protocol, host, version } = this.options;
    const socket = this.socket = io(linkUrl(origin), {
      path: PATH,
      transports: ['websocket'],   // no long-polling through a proxy, and no sticky-session question
      auth: { connector_id, token, protocol, host, version },
    });

    // The room welcomes a connector in its own connect handler, so the welcome and this client's
    // own `connect` can arrive in either order. Whichever is second is when the link is usable.
    let welcome = null, greeted = false;
    const ready = () => {
      if (greeted || !welcome || !socket.connected) return;
      greeted = true;
      this.attempts = 0;
      this.connected = true;
      this.options.onConnected?.(welcome);
    };
    const down = reason => {
      welcome = null; greeted = false;
      this.connected = false;
      this.fail(reason);
    };

    socket.on('connector.welcome', data => { welcome = data || {}; ready(); });
    socket.on('connect', ready);
    socket.on('connect_error', error => {
      this.attempts++;
      down({ error: error?.message || 'connection failed', retrying: socket.active });
    });
    socket.on('disconnect', reason => down({ close_reason: reason, retrying: socket.active }));

    // What the room asks of this connector. Which events those are is the room's vocabulary, not
    // this link's: whatever arrives goes up, and the answer — when the room waits for one — comes
    // back down the same way.
    socket.onAny((event, ...args) => {
      if (event === 'connector.welcome') return;
      const acknowledge = typeof args[args.length - 1] === 'function' ? args.pop() : null;
      Promise.resolve(this.options.onEvent?.(event, args[0] || {}))
        .then(answer => acknowledge?.(answer ?? {}))
        .catch(error => acknowledge?.({ error: String(error?.message || error).slice(0, 400) }));
    });
    return this;
  }

  /** Every wait ends when the connection does: a request whose answer can no longer arrive must
   *  fail now, not when its own timeout runs out. */
  fail(reason) {
    const gone = new Error(UNREACHABLE);
    for (const pending of [...this.waiting]) { this.waiting.delete(pending); pending.reject(gone); }
    if (!this.closed) this.options.onLost?.({ ...reason, attempt: this.attempts });
  }

  close() {
    this.closed = true;
    this.connected = false;
    try { this.socket?.close(); } catch {}
    this.fail({ close_reason: 'closed by this connector', retrying: false });
  }

  /** Say it and move on. False when there is no room to say it to; what is worth saying again once
   *  the room comes back is the caller's to decide, never a buffer's. */
  send(event, data) {
    if (!this.connected) return false;
    this.socket.emit(event, data);
    return true;
  }

  /** Ask, and wait for the room's answer to this and nothing else. */
  request(event, data, { timeout = 10_000 } = {}) {
    if (!this.connected) return Promise.reject(new Error(UNREACHABLE));
    return new Promise((resolve, reject) => {
      const pending = { reject };
      this.waiting.add(pending);
      this.socket.timeout(timeout).emit(event, data, (unanswered, answer) => {
        if (!this.waiting.delete(pending)) return;   // the connection already ended this wait
        if (unanswered) reject(new Error('The room did not answer'));
        else resolve(answer);
      });
    });
  }
}

/** One link to the room at that origin. */
export function roomLink(options) {
  return new RoomLink(options);
}
