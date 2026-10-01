/** A stand-in for `sidevoice-core`, as the connector starts it (SEAMS §2): flags in; a TCP listener that
 *  serves only discovery; the core's own socket (`--socket`, default `<data>/local.sock`) serving
 *  `GET /api/local/health` and the connector link on the path and namespace the real core serves; a ready
 *  file naming the socket, the launch id and `api`; `core-failure.json` when it fails before serving; and a
 *  log of what it was told. The name matters: the connector recognises a live core by `sidevoice-core` in
 *  its command line.
 *
 *  How a launch behaves is chosen per launch, so a test can make one start fail and the next one work:
 *  `FAKE_CORE_MODES` names a file of lines, and each launch takes the first one (the last one repeats);
 *  otherwise `FAKE_CORE_MODE`. Modes: `ok`; `import` (an ImportError before serving: `core-failure.json`
 *  with `import.missing-module`, exit 1); `identity`; `bind`; `exit:<code>` (dies before serving, no report);
 *  `slow:<ms>` (ready that much later); `hang:<ms>` (serves, then stops answering health after that long);
 *  `deaf` (alive, never ready); add `+stubborn` to ignore SIGTERM, `+vanish` to delete the program that started
 *  it (`FAKE_CORE_WRAPPER`), so the next launch finds no executable, `+chatty:<bytes>` to write that much to stderr
 *  every 20 ms for as long as it runs. */
import { createServer } from 'node:http';
import { appendFileSync, lstatSync, mkdirSync, readFileSync, renameSync, rmSync, unlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { Server } from 'socket.io';

const flag = name => { const at = process.argv.indexOf(name); return at > 0 ? process.argv[at + 1] : null; };
const version = process.env.FAKE_CORE_VERSION || '0.1.0';

if (process.argv.includes('--self-test')) {
  // Imports everything serving needs, binds nothing, writes nothing.
  if (process.env.FAKE_CORE_SELF_TEST_FAIL) { console.log(JSON.stringify({ ok: false, key: 'import.missing-module', message: 'No module named soxr' })); process.exit(1); }
  console.log(JSON.stringify({ ok: true, version }));
  process.exit(0);
}

const data = flag('--data-dir');
const port = Number(flag('--port') || 0);
const launchId = flag('--launch-id');
const socketPath = flag('--socket') || path.join(data, 'local.sock');
const idleExit = Number(flag('--idle-exit') ?? 600);
const said = event => appendFileSync(path.join(data, 'said.jsonl'), JSON.stringify({ pid: process.pid, launch_id: launchId, ...event }) + '\n');

function mode() {
  const file = process.env.FAKE_CORE_MODES;
  if (!file) return process.env.FAKE_CORE_MODE || 'ok';
  let lines = [];
  try { lines = readFileSync(file, 'utf8').split('\n').map(line => line.trim()).filter(Boolean); } catch {}
  if (lines.length > 1) writeFileSync(file, lines.slice(1).join('\n') + '\n');
  return lines[0] || 'ok';
}
const [behaviour, ...modifiers] = mode().split('+');
const [kind, argument] = behaviour.split(':');
const stubborn = modifiers.includes('stubborn');
const chatty = Number(modifiers.find(item => item.startsWith('chatty:'))?.slice(7) || 0);
// `+link:<n>` / `+api:<n>`: a core that serves, but speaks another link protocol or client api.
const linkProtocol = Number(modifiers.find(item => item.startsWith('link:'))?.slice(5) || 2);
const clientApi = Number(modifiers.find(item => item.startsWith('api:'))?.slice(4) || 1);
if (chatty) setInterval(() => process.stderr.write('x'.repeat(chatty - 1) + '\n'), 20);
if (modifiers.includes('vanish') && process.env.FAKE_CORE_WRAPPER) rmSync(process.env.FAKE_CORE_WRAPPER, { force: true });

function fail(step, key, message, code = 1) {
  const report = { launch_id: launchId, step, key, message, at: new Date().toISOString() };
  writeFileSync(path.join(data, 'core-failure.json.tmp'), JSON.stringify(report), { mode: 0o600 });
  renameSync(path.join(data, 'core-failure.json.tmp'), path.join(data, 'core-failure.json'));
  console.error(`fake core: ${key}: ${message}`);
  process.exit(code);
}

// The directory first, as the real `main()` guard does: someone else's, or open to others, is refused.
try {
  const stat = lstatSync(data);
  if (!stat.isDirectory() || stat.uid !== process.getuid() || (stat.mode & 0o077)) fail('directory', 'identity.unsafe-directory', `${data} is not this user's alone`);
} catch (error) { if (error.code === 'ENOENT') mkdirSync(data, { recursive: true, mode: 0o700 }); else throw error; }
said({ event: 'started', data: { argv: process.argv.slice(2), mode: behaviour } });

if (kind === 'import') fail('import', 'import.missing-module', 'No module named soxr');
if (kind === 'identity') fail('identity', 'identity.unreadable', 'identity.json is not readable');
if (kind === 'bind') fail('bind', 'bind.port-in-use', `[Errno 98] address already in use: ('127.0.0.1', ${port})`);
if (kind === 'exit') { console.error('fake core: Traceback (most recent call last): dying before serving'); process.exit(Number(argument || 3)); }

const credentialFile = path.join(data, 'connector-credential.json');
let credential; try { credential = JSON.parse(readFileSync(credentialFile, 'utf8')); } catch {}
if (!credential) { credential = { connector_id: 'local-' + randomUUID(), token: randomUUID() }; writeFileSync(credentialFile, JSON.stringify(credential), { mode: 0o600 }); }
const identity = { fingerprint: 'fp-fake', public_key: 'pk-fake', host: 'fake' };
const calls = () => { try { return Number(readFileSync(path.join(data, 'fake-calls'), 'utf8')) || 0; } catch { return 0; } };
const hangAt = kind === 'hang' ? Date.now() + Number(argument || 0) : Infinity;

// TCP: discovery only. The link and the local routes are not here (404), as on the real core.
const tcp = createServer((req, res) => {
  if (req.method === 'GET' && req.url.startsWith('/api/rendezvous')) { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ kind: 'node', fingerprint: identity.fingerprint, api: 1 })); return; }
  res.writeHead(404); res.end();
});
// The socket: the local routes, and the link — as the real core serves them (SEAMS §2): the Host must name
// loopback, and a local-only path carrying an `Origin` is refused (404), since only a browser sends one and
// native callers must not.
const LOOPBACK = new Set(['localhost', '127.0.0.1', '[::1]', '::1']);
const hostOk = req => LOOPBACK.has(String(req.headers.host || '').toLowerCase().replace(/:\d+$/, ''));
const localOnly = url => /^\/api\/(local|device\/local|connectors\/link)/.test(url);
const refusal = req => (!hostOk(req) ? 421 : localOnly(req.url) && req.headers.origin !== undefined ? 404 : 0);
const local = createServer((req, res) => {
  const refused = refusal(req);
  if (refused) { said({ event: 'refused', data: { url: req.url, status: refused, origin: req.headers.origin ?? null, host: req.headers.host ?? null } }); res.writeHead(refused); res.end(); return; }
  if (req.url.startsWith('/api/connectors/link')) return;   // Socket.IO's
  if (Date.now() >= hangAt) return;                           // wedged: never answers
  if (req.method === 'GET' && req.url === '/api/local/health') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ launch_id: launchId, pid: process.pid, version, api: clientApi, ...identity, calls: calls() }));
    return;
  }
  res.writeHead(404); res.end();
});
const io = new Server(local, { path: '/api/connectors/link', transports: ['websocket'],
  // The link's upgrade goes through the same rule: a request carrying an Origin, or a foreign Host, is refused.
  allowRequest: (req, callback) => { const refused = refusal(req); if (refused) said({ event: 'refused', data: { url: req.url, status: refused, origin: req.headers.origin ?? null, host: req.headers.host ?? null } }); callback(refused ? 'refused' : null, !refused); } });
const namespace = io.of('/connectors');
let linked = 0, quietSince = Date.now();
namespace.use((socket, next) => {
  const auth = socket.handshake.auth || {};
  said({ event: 'handshake', data: { connector_id: auth.connector_id, protocol: auth.protocol } });
  next(auth.connector_id === credential.connector_id && auth.token === credential.token ? undefined : new Error('not this core\'s credential'));
});
namespace.on('connection', socket => {
  linked++;
  socket.on('disconnect', () => { linked--; quietSince = Date.now(); });
  socket.emit('connector.welcome', { protocol: 2 });
  // A core whose link with the room is up, as the real one reports it to its connector.
  socket.emit('node.rendezvous', { room: 'http://room.test', connected: true, via: 'outbound', error: null, refused: null });
  socket.onAny((event, ...args) => {
    const acknowledge = typeof args[args.length - 1] === 'function' ? args.pop() : null;
    const data = args[0] || {};
    said({ event, data });
    // A binding that names its id is that binding coming back (a reconnect, a handover): it keeps it.
    if (event === 'binding.register') acknowledge?.({ client_ref: data.client_ref, binding_id: data.binding_id || 'core-' + data.client_ref, thread: data.thread });
    else if (event === 'device.pairing_code') acknowledge?.({ code: 'SV1.fake-' + process.pid, payload: JSON.parse(process.env.FAKE_CORE_PAIRING_PAYLOAD || '{"v":1,"host":"fake","urls":["http://127.0.0.1:8768"],"rv":null}'), expires_in: 600 });
    else acknowledge?.({ status: 'queued', text_saved: true, utterance_id: data.utterance_id });
  });
});
// The test's handle on the connector: the fake delivers a voice turn when told to through a file.
globalThis.fakeCoreNamespace = namespace;

async function serve() {
  if (kind === 'deaf') return;   // alive, and never ready
  if (kind === 'slow') await new Promise(resolve => setTimeout(resolve, Number(argument || 0)));
  try { unlinkSync(socketPath); } catch {}
  await new Promise(resolve => local.listen(socketPath, resolve));
  await new Promise((resolve, reject) => tcp.once('error', reject).listen(port, '127.0.0.1', resolve))
    .catch(error => fail('bind', 'bind.port-in-use', error.message));
  const bound = tcp.address().port;
  const ready = path.join(data, 'core.json');
  writeFileSync(ready + '.tmp', JSON.stringify({ pid: process.pid, port: bound, url: `http://127.0.0.1:${bound}`,
    version, protocol: linkProtocol, api: clientApi, socket: socketPath, launch_id: launchId, ...credential }), { mode: 0o600 });
  renameSync(ready + '.tmp', ready);
  said({ event: 'ready', data: { socket: socketPath, port: bound } });
}
serve();

// Turns to deliver, dropped by a test into `deliver.jsonl`, delivered as the real core delivers them
// (`control/connectors.py`): one at a time, to the connector linked now; none linked — it waits for one; the
// connector gone while one is in flight — it goes back at once and is delivered again to the next; an answer other
// than accepted, unknown or unsupported — tried again a little later. Every attempt goes to `delivery-attempts.jsonl`
// and the settled answer to `delivered.jsonl`.
const queue = [];
let read = 0, inflight = null;
const record = (file, entry) => appendFileSync(path.join(data, file), JSON.stringify(entry) + '\n');
const SETTLED = new Set(['accepted', 'unknown', 'unsupported']);
setInterval(() => {
  let lines = [];
  try { lines = readFileSync(path.join(data, 'deliver.jsonl'), 'utf8').split('\n').filter(Boolean); } catch {}
  for (const line of lines.slice(read)) { read++; queue.push(JSON.parse(line)); }
  if (inflight || !queue.length) return;
  const socket = [...namespace.sockets.values()].filter(item => item.connected).at(-1);
  if (!socket) return;
  const frame = queue.shift();
  const attempt = inflight = { frame, socket };
  record('delivery-attempts.jsonl', { event_id: frame.event_id, at: Date.now() });
  socket.timeout(10_000).emit('input.deliver', frame, (error, answer) => {
    if (inflight !== attempt) return;   // already given back when its connector went away
    inflight = null;
    if (!error && SETTLED.has(answer?.status)) return record('delivered.jsonl', { frame, answer });
    setTimeout(() => queue.unshift(frame), 200);
  });
}, 25).unref();
namespace.on('connection', socket => socket.on('disconnect', () => {
  if (inflight?.socket !== socket) return;
  queue.unshift(inflight.frame); inflight = null;
}));

// No connector and no call for `--idle-exit` seconds: leave, as the real core does (0: never).
if (idleExit > 0) setInterval(() => { if (!linked && !calls() && Date.now() - quietSince >= idleExit * 1000) leave(); }, 200).unref();

function leave() {
  try { if (JSON.parse(readFileSync(path.join(data, 'core.json'), 'utf8')).pid === process.pid) unlinkSync(path.join(data, 'core.json')); } catch {}
  try { unlinkSync(socketPath); } catch {}
  process.exit(0);
}
process.on('SIGTERM', () => { said({ event: 'sigterm', data: {} }); if (!stubborn) leave(); });
process.on('SIGINT', leave);
