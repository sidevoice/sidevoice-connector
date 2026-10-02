/** Experimental: a voice message into a chat of the Cursor editor, by the chat's own id, through Cursor's
 *  Desktop Bridge — the local API behind its `cursor desktop` CLI ("lists and sends messages to agent
 *  threads open in this desktop app"). Read from Cursor 3.22.12 (main process `desktopBridgeMainService`):
 *
 *  - it runs only when Cursor's `desktop_bridge` beta is on for the account (a server-side flag the app
 *    re-reads at every start) and the person turned on Settings → Beta → "Allow CLI to access desktop agents";
 *  - it announces itself in `~/.cursor/desktop-bridge/<hash>.json` (`CURSOR_DESKTOP_BRIDGE_DIR`), mode 0600:
 *    `{protocolVersion: 1, pid, socketPath, token, appName, appVersion, userDataDir, createdAt}`;
 *  - HTTP over that unix socket, `POST /`, `Authorization: Bearer <token>`, JSON body
 *    `{type: "listThreads"}` → `{threads: [{id, title, source, status, lastUpdatedAt, windowId}]}`, or
 *    `{type: "sendMessage", threadId, text, force?}` → submitted | queued | not-found | not-sendable | error.
 *    A chat that is not on screen is loaded to take it.
 *
 *  Nothing is installed in Cursor, and nothing of Cursor's is changed: if the beta is off, this route is simply
 *  not there and the card is the one left. */
import { execFile } from 'node:child_process';
import { runningAsSea } from './sea-runtime.mjs';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

export function bridgeDir(env = process.env) {
  return env.CURSOR_DESKTOP_BRIDGE_DIR?.trim() || path.join(env.HOME || os.homedir(), '.cursor', 'desktop-bridge');
}

const alive = pid => { try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; } };
const CURSOR_CHAT_QUERY = "SELECT DISTINCT substr(key, 10, instr(substr(key, 10), ':') - 1) AS chat FROM cursorDiskKV WHERE key >= ? AND key < ? AND instr(CAST(value AS TEXT), ?) > 0 LIMIT 2";

/** The running Cursor apps that announced a bridge, newest first. */
export function bridgeInstances(env = process.env) {
  const dir = bridgeDir(env);
  let names = [];
  try { names = readdirSync(dir).filter(name => name.endsWith('.json')); } catch { return []; }
  const found = [];
  for (const name of names) {
    let entry; try { entry = JSON.parse(readFileSync(path.join(dir, name), 'utf8')); } catch { continue; }
    if (entry?.protocolVersion !== 1 || typeof entry.socketPath !== 'string' || typeof entry.token !== 'string') continue;
    if (!Number.isInteger(entry.pid) || !alive(entry.pid)) continue;
    if (!entry.socketPath.startsWith('\\\\') && !existsSync(entry.socketPath)) continue;
    found.push(entry);
  }
  return found.sort((a, b) => (b.createdAt || 0) - (a.createdAt || 0));
}

/** An error before the request reached Cursor (no socket, refused) is safe to fall back from; one after it was
 *  written is not — Cursor may have taken the message — and says so with `sent: true`. */
function request(instance, body, timeoutMs = 10_000) {
  return new Promise((resolve, reject) => {
    const payload = JSON.stringify(body);
    let written = false;
    const req = http.request({ socketPath: instance.socketPath, path: '/', method: 'POST', timeout: timeoutMs,
      headers: { authorization: `Bearer ${instance.token}`, 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } }, res => {
      let text = ''; res.setEncoding('utf8');
      res.on('data', chunk => { text += chunk; });
      res.on('end', () => { let parsed = null; try { parsed = JSON.parse(text); } catch {} resolve({ statusCode: res.statusCode, body: parsed }); });
    });
    req.on('timeout', () => req.destroy(new Error('the Cursor Desktop Bridge did not answer')));
    req.on('error', error => reject(Object.assign(error, { sent: written })));
    req.on('socket', socket => socket.on('connect', () => { written = true; }));
    req.end(payload);
  });
}

/** Every agent thread the running Cursor apps list. */
export async function listThreads(env = process.env) {
  const threads = [];
  for (const instance of bridgeInstances(env)) {
    try {
      const { statusCode, body } = await request(instance, { type: 'listThreads' });
      if (statusCode === 200 && Array.isArray(body?.threads)) threads.push(...body.threads.filter(t => t && typeof t.id === 'string'));
    } catch {}
  }
  return threads;
}

/** The chat calling us right now, when that can be told: a chat is working while it runs a tool, so if exactly
 *  one local chat is working, it is the one. Several working at once: not told (null) — the chat's own
 *  records say it later. */
export async function callingThread(env = process.env) {
  // A candidate only: the caller confirms it (composerHolding) before sending anything to it.
  const running = (await listThreads(env)).filter(t => t.status === 'running' && t.source === 'local');
  return running.length === 1 ? running[0].id : null;
}

/** Send one message to one chat. What Cursor answers is what is known: submitted, or queued behind a running
 *  turn. A chat no app knows, or one that cannot take messages, is an error the caller can fall back from. */
export async function sendToThread(threadId, text, env = process.env, { force = false } = {}) {
  const instances = bridgeInstances(env);
  if (!instances.length) throw Object.assign(new Error('No Cursor Desktop Bridge is running (Settings → Beta → "Allow CLI to access desktop agents", when Cursor offers it)'), { code: 'NO_BRIDGE' });
  let last = null;
  for (const instance of instances) {
    let answer;
    try { answer = await request(instance, { type: 'sendMessage', threadId, text, force }); }
    catch (error) {
      // Written and then lost: Cursor may have it. Not tried again anywhere, or the chat gets it twice.
      if (error.sent) throw Object.assign(new Error('The Cursor Desktop Bridge took the request and did not answer'), { code: 'UNKNOWN' });
      last = Object.assign(error, { code: 'UNREACHABLE' }); continue;
    }
    const outcome = answer.body?.outcome ?? answer.body?.status;
    if (answer.statusCode === 200 && (outcome === 'submitted' || outcome === 'queued')) return { outcome, title: answer.body.threadTitle ?? null };
    // 3.22.12's main process says `unknown-thread` (the renderer, `not-found`): this app does not have that chat.
    if (outcome === 'not-found' || outcome === 'unknown-thread') { last = Object.assign(new Error(`Cursor does not know chat ${threadId}`), { code: 'NOT_FOUND' }); continue; }
    last = Object.assign(new Error(`Cursor did not take it: ${answer.body?.reason || answer.body?.message || answer.body?.error || outcome || answer.statusCode}`), { code: 'REFUSED' });
    break;
  }
  throw last || new Error('The Cursor Desktop Bridge did not take the message');
}

/** The Cursor chat whose record holds this marker — the view key voice_connect returned to that chat and to no
 *  other, stored by Cursor with the chat's bubbles in its state database (written up to ~30 s late). Given a
 *  candidate, only that chat's rows are read (an index range); otherwise every bubble, in a separate process so
 *  the connector never waits on it. Exactly one chat must hold it, or none is named. */
export async function composerHolding(marker, env = process.env, { candidate = null, onError = null } = {}) {
  if (!/^[0-9a-f]{64}$/.test(marker || '')) return null;
  for (const instance of bridgeInstances(env)) {
    const file = path.join(instance.userDataDir || '', 'User', 'globalStorage', 'state.vscdb');
    if (!instance.userDataDir || !existsSync(file)) continue;
    const [from, to] = candidate ? [`bubbleId:${candidate}:`, `bubbleId:${candidate};`] : ['bubbleId:', 'bubbleId;'];
    const script = `const { DatabaseSync } = require('node:sqlite');
const [file, marker, from, to] = process.argv.slice(1);
const db = new DatabaseSync(file, { readOnly: true });
const rows = db.prepare(${JSON.stringify(CURSOR_CHAT_QUERY)}).all(from, to, marker);
db.close(); process.stdout.write(JSON.stringify(rows.map(r => r.chat)));`;
    const args = runningAsSea()
      ? ['--sidevoice-cursor-db-query', file, marker, from, to]
      : ['--no-warnings', '-e', script, file, marker, from, to];
    const chats = await new Promise(resolve => execFile(process.execPath, args, { timeout: 60_000, maxBuffer: 1 << 16 },
      (error, stdout, stderr) => {
        if (error) { onError?.(String(stderr || error.message).trim().split('\n').pop().slice(0, 200)); return resolve(null); }
        try { resolve(JSON.parse(stdout)); } catch { resolve(null); } }));
    if (Array.isArray(chats) && chats.length === 1 && chats[0]) return chats[0];
  }
  return null;
}

/** Private child-process entry used by the SEA, which is not a Node `-e` interpreter. */
export async function cursorDatabaseMatches(file, marker, from, to) {
  const { DatabaseSync } = await import('node:sqlite');
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    const rows = db.prepare(CURSOR_CHAT_QUERY)
      .all(from, to, marker);
    return rows.map(row => row.chat);
  } finally { db.close(); }
}
