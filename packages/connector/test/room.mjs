/** A room, for the connector's tests: the real Socket.IO server the real room runs, on the path and
 *  in the namespace this package knows. The stand-in is the room's behaviour, never its transport —
 *  a hand-written server here would be a second implementation of what #70 stopped maintaining.
 *
 *  It can be stopped and started again on the same port, which is how a connector's reconnection,
 *  its re-registration and its outbox are put under real conditions. */
import { createServer } from 'node:http';
import { Server } from 'socket.io';
import { NAMESPACE, PATH } from '../link.mjs';

export const PROTOCOL = 2;

export async function startRoom({ port = 0 } = {}) {
  const room = {
    said: [],                 // every event the connector sent: { event, data }
    handle: null,             // (event, data) => acknowledgement, or undefined
    admit: null,              // (auth) => error message to refuse with, or null
    welcome: { protocol: PROTOCOL },
    connections: 0,
    socket: null,             // the connector's socket, while one is connected
  };

  room.start = async (on = room.port) => {
    const http = room.http = createServer((_, res) => { res.writeHead(404); res.end(); });
    const io = room.io = new Server(http, { path: PATH, transports: ['websocket'] });
    const namespace = io.of(NAMESPACE);
    namespace.use((socket, next) => {
      room.auth = socket.handshake.auth;
      const refusal = room.admit?.(socket.handshake.auth);
      next(refusal ? new Error(refusal) : undefined);
    });
    namespace.on('connection', socket => {
      room.socket = socket; room.connections++;
      socket.emit('connector.welcome', room.welcome);
      socket.onAny((event, ...args) => {
        const acknowledge = typeof args[args.length - 1] === 'function' ? args.pop() : null;
        const data = args[0] || {};
        room.said.push({ event, data });
        Promise.resolve(room.handle?.(event, data)).then(answer => acknowledge?.(answer ?? {}));
      });
      socket.on('disconnect', () => { if (room.socket === socket) room.socket = null; });
    });
    await new Promise(resolve => http.listen(on, '127.0.0.1', resolve));
    room.port = http.address().port;
    room.origin = `http://127.0.0.1:${room.port}`;
    return room;
  };

  /** Stop serving, keeping the port so the same room can come back on it. */
  room.stop = async () => {
    room.socket = null;
    await new Promise(resolve => room.io.close(resolve));
  };

  room.close = room.stop;
  room.sent = event => room.said.filter(one => one.event === event).map(one => one.data);

  /** Ask the connector something and wait for its acknowledgement, as the room's delivery does. */
  room.ask = (event, data, timeout = 10_000) => new Promise((resolve, reject) => {
    if (!room.socket) return reject(new Error('no connector is connected'));
    room.socket.timeout(timeout).emit(event, data, (unanswered, answer) =>
      unanswered ? reject(new Error(`${event} went unacknowledged`)) : resolve(answer));
  });

  room.tell = (event, data) => room.socket?.emit(event, data);

  await room.start(port);
  return room;
}
