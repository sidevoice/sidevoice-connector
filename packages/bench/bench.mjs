#!/usr/bin/env node
// The Sidevoice connector test bench: a fake Core (protocol v3) on a private profile's socket, the real connector
// linked to it, and a web UI on 127.0.0.1 to drive and watch both. `node bench.mjs --help`.

import { readFileSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { ConnectorSupervisor, findConnector } from './lib/connector.mjs';
import { FakeCore } from './lib/fake-core.mjs';
import { harnessSetup, writeHarnessFiles } from './lib/harness-setup.mjs';
import { LANGUAGES, MESSAGES_DIR, t } from './lib/i18n.mjs';
import { createProfile, defaultProfileRoot } from './lib/profile.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PUBLIC = path.join(HERE, 'public');
const STATIC = {
  '/': ['index.html', 'text/html; charset=utf-8'],
  '/app.js': ['app.js', 'text/javascript; charset=utf-8'],
  '/style.css': ['style.css', 'text/css; charset=utf-8'],
};
const BODY_LIMIT = 1 << 20;

const { values: options } = parseArgs({
  options: {
    port: { type: 'string', default: '4477' },
    profile: { type: 'string' },
    connector: { type: 'string' },
    'no-connector': { type: 'boolean', default: false },
    reset: { type: 'boolean', default: false },
    help: { type: 'boolean', short: 'h', default: false },
  },
});

if (options.help) {
  console.log(t('cli.usage'));
  process.exit(0);
}

const root = path.resolve(options.profile ?? defaultProfileRoot());
const paths = createProfile(root, { reset: options.reset });
const core = await new FakeCore({ dataDir: paths.coreDir }).start();
const binary = options['no-connector'] ? null : options.connector ? path.resolve(options.connector) : findConnector();
const supervisor = binary ? new ConnectorSupervisor({ binary, profileRoot: root }) : null;
const setup = harnessSetup({ binary: binary ?? '<sidevoice-rust-proof>', paths });
writeHarnessFiles(setup);

// ---- server-sent events --------------------------------------------------------------------------------------

const clients = new Set();
function broadcast(event, data) {
  const chunk = `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const res of clients) res.write(chunk);
}
let stateTimer = null;
function scheduleState() {
  stateTimer ??= setTimeout(() => {
    stateTimer = null;
    broadcast('state', state());
  }, 80);
}
core.on('state', scheduleState);
core.on('frame', (entry) => broadcast('frame', entry));
core.on('speech', (item) => broadcast('speech', item));
supervisor?.on('log', (entry) => broadcast('log', entry));
supervisor?.on('status', scheduleState);

function state() {
  return {
    ...core.snapshot(),
    process: supervisor?.status() ?? { binary: null, running: false, wanted: false, restarts: 0 },
    profile: root,
    setup,
  };
}

// ---- HTTP ----------------------------------------------------------------------------------------------------

const port = Number(options.port);
const allowedHosts = new Set([`127.0.0.1:${port}`, `localhost:${port}`]);

function send(res, status, value, type = 'application/json') {
  const body = Buffer.isBuffer(value) ? value : Buffer.from(typeof value === 'string' ? value : JSON.stringify(value));
  res.writeHead(status, { 'content-type': type, 'content-length': body.length, 'cache-control': 'no-store' });
  res.end(body);
}

async function readJson(req) {
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > BODY_LIMIT) throw new Error('body too large');
    chunks.push(chunk);
  }
  return chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {};
}

const actions = {
  say: ({ thread, text }) => core.say(thread, text),
  command: async ({ method, params = {}, notify = false, timeout_ms: timeoutMs = 20_000 }) => {
    if (typeof method !== 'string' || !method) throw new Error('method required');
    if (notify) return core.notify(method, params) ?? { sent: true };
    return { result: await core.request(method, params, timeoutMs) };
  },
  raw: ({ frame }) => (core.sendRaw(frame), { sent: true }),
  policy: ({ method, mode }) => core.setPolicy(method, mode),
  rendezvous: (update) => core.setRendezvous(update),
  close: ({ binding_id: bindingId, reason }) => core.closeBinding(bindingId, reason),
  session: () => ({ session_id: core.newSession() }),
  connector: ({ action }) => {
    if (!supervisor) throw new Error('no connector binary');
    if (action === 'start') supervisor.start();
    else if (action === 'stop') supervisor.stop();
    else throw new Error('unknown action');
    return supervisor.status();
  },
};

const server = http.createServer(async (req, res) => {
  // Only this machine's browser, and only this page: a POST from any other origin is refused (no CORS is
  // answered, and the custom header forces a preflight a foreign page cannot pass).
  if (!allowedHosts.has(req.headers.host ?? '')) return send(res, 421, { error: 'host' });
  const url = new URL(req.url, `http://${req.headers.host}`);
  try {
    if (req.method === 'GET') {
      if (STATIC[url.pathname]) {
        const [file, type] = STATIC[url.pathname];
        return send(res, 200, readFileSync(path.join(PUBLIC, file)), type);
      }
      const lang = url.pathname.match(/^\/messages\/([a-z]{2})\.json$/)?.[1];
      if (lang && LANGUAGES.includes(lang)) {
        return send(res, 200, readFileSync(path.join(MESSAGES_DIR, `${lang}.json`)));
      }
      if (url.pathname === '/api/state') return send(res, 200, state());
      if (url.pathname === '/api/frames') return send(res, 200, core.log);
      if (url.pathname === '/api/connector-log') return send(res, 200, supervisor?.log ?? []);
      if (url.pathname === '/api/events') {
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' });
        res.write(`event: state\ndata: ${JSON.stringify(state())}\n\n`);
        clients.add(res);
        req.on('close', () => clients.delete(res));
        return;
      }
      return send(res, 404, { error: 'not_found' });
    }
    if (req.method === 'POST' && url.pathname.startsWith('/api/')) {
      if (req.headers['x-sidevoice-bench'] !== '1') return send(res, 403, { error: 'forbidden' });
      const action = actions[url.pathname.slice('/api/'.length)];
      if (!action) return send(res, 404, { error: 'not_found' });
      const result = await action(await readJson(req));
      return send(res, 200, result ?? { ok: true });
    }
    send(res, 405, { error: 'method' });
  } catch (error) {
    send(res, 400, { error: error.message, ...(error.rpc ? { rpc: error.rpc } : {}) });
  }
});

server.on('error', (error) => {
  console.error(t('cli.listenFailed', { port, error: error.message }));
  process.exit(1);
});
server.listen(port, '127.0.0.1', () => {
  console.log(t('cli.ready', { url: `http://127.0.0.1:${port}/` }));
  console.log(t('cli.profile', { root }));
  if (supervisor) {
    console.log(t('cli.connector', { binary }));
    supervisor.start();
  } else {
    console.log(t(options['no-connector'] ? 'cli.connectorSkipped' : 'cli.connectorMissing'));
  }
  console.log(`\n${t('cli.harnesses')}`);
  console.log(`  Claude Code: ${setup.claude.run}`);
  console.log(`  Codex:       ${setup.codex.login}`);
  console.log(`               ${setup.codex.register}`);
  console.log(`               ${setup.codex.run}`);
  console.log(`  Cursor CLI:  ${setup.cursor.run}`);
  console.log(`\n${t('cli.reset', { command: setup.reset })}`);
});

let stopping = false;
async function shutdown() {
  if (stopping) return;
  stopping = true;
  for (const res of clients) res.end();
  server.close();
  await supervisor?.stop();
  await core.stop();
  process.exit(0);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
