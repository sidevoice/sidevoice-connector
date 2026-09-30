/** A stand-in for `sidevoice-core`, as the connector starts it: flags in, a ready file out, the
 *  connector link on the path and namespace the real core serves, and a log of what it was told.
 *  The name matters: the connector recognises a live core by `sidevoice-core` in its command line. */
import { createServer } from 'node:http';
import { appendFileSync, mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { Server } from 'socket.io';

const flag = name => { const at = process.argv.indexOf(name); return at > 0 ? process.argv[at + 1] : null; };
const data = flag('--data-dir');
const port = Number(flag('--port') || 0);
mkdirSync(data, { recursive: true });
const credentialFile = path.join(data, 'connector-credential.json');
let credential; try { credential = JSON.parse(readFileSync(credentialFile, 'utf8')); } catch {}
if (!credential) { credential = { connector_id: 'local-' + randomUUID(), token: randomUUID() }; writeFileSync(credentialFile, JSON.stringify(credential)); }
const said = event => appendFileSync(path.join(data, 'said.jsonl'), JSON.stringify({ pid: process.pid, ...event }) + '\n');
said({ event: 'started', data: { argv: process.argv.slice(2) } });

const http = createServer((_, res) => { res.writeHead(404); res.end(); });
const io = new Server(http, { path: '/api/connectors/link', transports: ['websocket'] });
const namespace = io.of('/connectors');
namespace.use((socket, next) => {
  const auth = socket.handshake.auth || {};
  said({ event: 'handshake', data: { connector_id: auth.connector_id, protocol: auth.protocol } });
  next(auth.connector_id === credential.connector_id && auth.token === credential.token ? undefined : new Error('not this core\'s credential'));
});
namespace.on('connection', socket => {
  socket.emit('connector.welcome', { protocol: 2 });
  socket.onAny((event, ...args) => {
    const acknowledge = typeof args[args.length - 1] === 'function' ? args.pop() : null;
    const data = args[0] || {};
    said({ event, data });
    if (event === 'binding.register') acknowledge?.({ client_ref: data.client_ref, binding_id: 'core-' + data.client_ref, thread: data.thread });
    else acknowledge?.({ status: 'queued', text_saved: true, utterance_id: data.utterance_id });
  });
});
http.listen(port, '127.0.0.1', () => {
  const bound = http.address().port;
  const ready = path.join(data, 'core.json');
  writeFileSync(ready + '.tmp', JSON.stringify({ pid: process.pid, port: bound, url: `http://127.0.0.1:${bound}`,
    version: process.env.FAKE_CORE_VERSION || '0.1.0', protocol: 2, ...credential }), { mode: 0o600 });
  renameSync(ready + '.tmp', ready);
});
const leave = () => { try { if (JSON.parse(readFileSync(path.join(data, 'core.json'), 'utf8')).pid === process.pid) unlinkSync(path.join(data, 'core.json')); } catch {} process.exit(0); };
process.on('SIGTERM', leave); process.on('SIGINT', leave);
