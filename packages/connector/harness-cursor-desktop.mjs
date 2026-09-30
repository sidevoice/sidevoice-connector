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
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

export function bridgeDir(env = process.env) {
  return env.CURSOR_DESKTOP_BRIDGE_DIR?.trim() || path.join(env.HOME || os.homedir(), '.cursor', 'desktop-bridge');
}

const alive = pid => { try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; } };

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

function request(instance, body, timeoutMs = 10_000) {
  return new Promise((resolve, reject) => {
    const payload = JSON.stringify(body);
    const req = http.request({ socketPath: instance.socketPath, path: '/', method: 'POST', timeout: timeoutMs,
      headers: { authorization: `Bearer ${instance.token}`, 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } }, res => {
      let text = ''; res.setEncoding('utf8');
      res.on('data', chunk => { text += chunk; });
      res.on('end', () => { let parsed = null; try { parsed = JSON.parse(text); } catch {} resolve({ statusCode: res.statusCode, body: parsed }); });
    });
    req.on('timeout', () => req.destroy(new Error('the Cursor Desktop Bridge did not answer')));
    req.on('error', reject);
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
  const running = (await listThreads(env)).filter(t => t.status === 'running' && (t.source === 'local' || t.source === undefined));
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
    catch (error) { last = error; continue; }
    const outcome = answer.body?.outcome ?? answer.body?.status;
    if (answer.statusCode === 200 && (outcome === 'submitted' || outcome === 'queued')) return { outcome, title: answer.body.threadTitle ?? null };
    if (outcome === 'not-found') { last = Object.assign(new Error(`Cursor does not know chat ${threadId}`), { code: 'NOT_FOUND' }); continue; }
    last = Object.assign(new Error(`Cursor did not take it: ${answer.body?.reason || answer.body?.message || answer.body?.error || outcome || answer.statusCode}`), { code: 'REFUSED' });
    break;
  }
  throw last || new Error('The Cursor Desktop Bridge did not take the message');
}

/** The Cursor chat whose record holds our conversation id — the result of its voice_connect call, which Cursor
 *  stores with the chat's bubbles in the app's state database (written up to ~30 s late). */
export async function composerHolding(conversation, env = process.env) {
  let DatabaseSync;
  try { ({ DatabaseSync } = await import('node:sqlite')); } catch { return null; }
  for (const instance of bridgeInstances(env)) {
    const file = path.join(instance.userDataDir || '', 'User', 'globalStorage', 'state.vscdb');
    if (!instance.userDataDir || !existsSync(file)) continue;
    let db;
    try {
      db = new DatabaseSync(file, { readOnly: true });
      const row = db.prepare("SELECT key FROM cursorDiskKV WHERE key LIKE 'bubbleId:%' AND instr(CAST(value AS TEXT), ?) > 0 LIMIT 1").get(conversation);
      const composer = typeof row?.key === 'string' ? row.key.split(':')[1] : null;
      if (composer) return composer;
    } catch {} finally { try { db?.close(); } catch {} }
  }
  return null;
}
