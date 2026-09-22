/** What Claude Code will do with a message we post to a session's inbox, decided before we post it.
 *
 *  A session that bypasses permission prompts holds an injected message for its user's approval
 *  instead of delivering it, and the inbox sends us no receipt to say so — the write looks
 *  identical either way. So the only honest moment to find out is at voice_connect, from the
 *  session's own launch flags and settings. Best effort by design: a managed policy layer we
 *  cannot read could tighten this further, and the result says so rather than pretending.
 *  Documented at https://code.claude.com/docs/en/cross-session-messaging */
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { defineHarness, envelope, SUPPORTED, tailJsonl } from './harness-contract.mjs';

const configDir = () => process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude');

function readJson(file) {
  try { return JSON.parse(readFileSync(file, 'utf8')); } catch { return null; }
}

/** The record Claude Code keeps for a session, or null. It publishes `status` there and keeps it current. */
export function sessionRecord(sessionId) {
  const registry = path.join(configDir(), 'sessions');
  let entries = [];
  try { entries = readdirSync(registry).filter(name => name.endsWith('.json')); } catch { return null; }
  for (const name of entries) {
    const record = readJson(path.join(registry, name));
    if (record?.sessionId === sessionId) return record;
  }
  return null;
}

/** Whether that session is working on something right now: true, false, or null when it cannot be told.
 *  This is Claude Code's own bookkeeping, not a published interface: an unknown value answers null rather
 *  than guessing, and the room falls back to what the conversation says about its own replies. */
export function sessionWorking(sessionId) {
  const status = sessionRecord(sessionId)?.status;
  if (status === 'busy') return true;
  if (status === 'idle' || status === 'ready' || status === 'waiting') return false;
  return null;
}

/** The pid of the session with this id, from Claude Code's own session registry. */
function sessionPid(sessionId) {
  const registry = path.join(configDir(), 'sessions');
  let entries = [];
  try { entries = readdirSync(registry).filter(name => name.endsWith('.json')); } catch { return null; }
  for (const name of entries) {
    const record = readJson(path.join(registry, name));
    if (record?.sessionId === sessionId) return record.pid ?? null;
  }
  return null;
}

/** The launch arguments of a pid, via ps so this works the same on Linux and macOS. */
function launchArgs(pid) {
  if (!pid) return '';
  try { return execFileSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8', timeout: 4000 }).trim(); }
  catch { return ''; }
}

function flag(args, name) {
  const match = args.match(new RegExp(`${name}[= ]('[^']*'|"[^"]*"|\\S+)`));
  if (!match) return undefined;
  return match[1].replace(/^['"]|['"]$/g, '');
}

/** `--settings` takes inline JSON or a path to a file; both may carry crossSessionInbound. */
function settingsFromFlag(args) {
  const value = flag(args, '--settings');
  if (!value) return null;
  if (value.trim().startsWith('{')) { try { return JSON.parse(value); } catch { return null; } }
  return readJson(value);
}

const BYPASS_MODES = new Set(['bypassPermissions']);

/** Which model this conversation runs, read from the session's own launch line — no model is asked to
 *  say what it is. Absent rather than guessed when the launcher did not name one (the CLI's default). */
export function sessionEngine(sessionId) {
  const args = launchArgs(sessionPid(sessionId));
  if (!args) return null;
  const model = flag(args, '--model'), effort = flag(args, '--effort'), thinking = flag(args, '--thinking');
  if (!model && !effort) return null;
  return { model: model || null, effort: effort || null, thinking: thinking || null };
}

/** Will an injected message be delivered to this Claude session, or held for its user? */
export function inspectInbound(sessionId) {
  const pid = sessionPid(sessionId);
  const args = launchArgs(pid);
  const user = readJson(path.join(configDir(), 'settings.json')) || {};
  const flagged = settingsFromFlag(args) || {};
  const mode = flag(args, '--permission-mode') || user.permissions?.defaultMode || 'default';
  // Launch flags beat user settings; a managed policy layer could still tighten either.
  const inbound = flagged.crossSessionInbound ?? user.crossSessionInbound;
  const bypassing = BYPASS_MODES.has(mode);
  if (!bypassing) return { ok: true, mode, crossSessionInbound: inbound ?? null };
  if (inbound === 'accept') return { ok: true, mode, crossSessionInbound: inbound };
  return {
    ok: false,
    mode,
    crossSessionInbound: inbound ?? null,
    reason: inbound === 'refuse'
      ? 'This session refuses messages from other local processes (crossSessionInbound is "refuse").'
      : 'This session bypasses permission prompts, so Claude Code holds messages from other local '
        + 'processes for the user to approve instead of delivering them, and it sends no receipt to '
        + 'say so. Voice will appear to be sent and nothing will arrive.',
    remedy: inbound === 'refuse'
      ? 'Change crossSessionInbound from "refuse" to "accept", or start the session in a permission '
        + 'mode that prompts.'
      : 'Two ways out. Per session: start it with --settings \'{"crossSessionInbound":"accept"}\'. '
        + 'For every session on this machine: add "crossSessionInbound": "accept" to '
        + `${path.join(configDir(), 'settings.json')} — that takes effect immediately, releases any `
        + 'messages already held, and also lets any other local process post into all your sessions, '
        + 'which is the safeguard it removes. Or run the conversation in a prompting mode such as '
        + '--permission-mode auto.',
    // Said plainly so nothing downstream reports this as certain.
    confidence: pid ? 'read from the session launch flags and settings' : 'settings only; the session process was not found',
  };
}

/** Identity and private inbox inherited by the MCP façade Claude Code spawned. */
export function sessionIdentity({ env = process.env } = {}) {
  if (!env.CLAUDE_CODE_SESSION_ID) return null;
  return {
    harness: 'claude',
    thread: env.CLAUDE_CODE_SESSION_ID,
    ...(env.CLAUDE_CODE_MESSAGING_SOCKET ? { delivery: {
      kind: 'claude-uds',
      socket: env.CLAUDE_CODE_MESSAGING_SOCKET,
      token: env.CLAUDE_CODE_MESSAGING_TOKEN || '',
    } } : {}),
  };
}

/** Claude Code's session inbox sends no acknowledgement. The read receipt comes from watching the
 *  session's own transcript (see observe); this method reports only what the socket itself proves. */
export function deliver(delivery, event) {
  if (delivery?.kind !== 'claude-uds') throw new Error(`Unsupported Claude delivery kind: ${delivery?.kind}`);
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const socket = net.createConnection(delivery.socket);
    let settled = false, wrote = 0, replied = '';
    const finish = (error, status, detail) => {
      if (settled) return;
      settled = true; clearTimeout(timer); socket.destroy();
      if (error) return reject(error);
      resolve({ status, detail: `${detail} after ${Date.now() - started}ms${replied ? ', peer said ' + replied.slice(0, 120) : ''}` });
    };
    const timer = setTimeout(() => finish(null, 'unknown', 'connection still open, no acknowledgement'), 1500);
    socket.on('error', error => finish(error));
    socket.on('data', chunk => { replied += chunk; });
    socket.on('connect', () => {
      socket.write(JSON.stringify({ type: 'auth', token: delivery.token }) + '\n');
      socket.write(JSON.stringify({ type: 'user', message: { role: 'user', content: envelope(event) } }) + '\n');
      wrote = Date.now();
    });
    socket.on('close', () => finish(null, 'rejected', wrote ? 'peer closed the connection' : 'peer closed before the frames were written'));
  });
}

/** The transcript Claude Code writes for a session: one JSON-lines file named after the session id, under
 *  the project directory it derives from the launch cwd. Found by name rather than derived, so a renamed
 *  or moved cwd changes nothing. */
export function transcriptPath(sessionId) {
  const projects = path.join(configDir(), 'projects');
  let dirs = [];
  try { dirs = readdirSync(projects); } catch { return null; }
  for (const dir of dirs) {
    const candidate = path.join(projects, dir, sessionId + '.jsonl');
    if (existsSync(candidate)) return candidate;
  }
  return null;
}

/** The text of a transcript entry that is a user message, or null for anything else (tool results,
 *  attachments, the session's own bookkeeping). */
export function userMessageText(entry) {
  // A message that arrives while the session is busy is not a `user` entry yet: Claude Code records it as a
  // queued command attachment and feeds it to the running turn. Its prompt is the message, whole.
  if (entry?.type === 'attachment' && entry.attachment?.type === 'queued_command')
    return typeof entry.attachment.prompt === 'string' ? entry.attachment.prompt : null;
  if (entry?.type !== 'user' || entry.message?.role !== 'user') return null;
  const content = entry.message.content;
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return null;
  const parts = content.filter(part => part?.type === 'text').map(part => part.text);
  return parts.length ? parts.join('\n') : null;
}

/** The model a transcript entry says answered with it. Claude Code writes it on every assistant entry —
 *  `{"type":"assistant","message":{"model":"claude-fable-5-1", ...}}` — which is why a session launched
 *  with no `--model` still says what it thinks with, the moment it answers once. */
export function assistantModel(entry) {
  if (entry?.type !== 'assistant') return null;
  const model = entry.message?.model;
  return typeof model === 'string' && model ? model : null;
}

const POLL_MS = Number(process.env.SIDEVOICE_WORK_POLL_MS || 400);

/** Watch one session through what Claude Code itself writes about it, and nothing installed in it:
 *  its registry record says whether it is busy, its transcript records every user message the
 *  moment the session admits it (a message from the inbox is appended as the turn takes it, not when
 *  the socket accepted it — the difference is what the second tick shows), and every assistant entry
 *  names the model that wrote it. */
export function observe(sessionId, handlers) {
  let lastStatus = null, lastModel = null;
  const stopTranscript = tailJsonl(() => transcriptPath(sessionId), entry => {
    const model = assistantModel(entry);
    if (model && model !== lastModel) {
      lastModel = model;
      // Effort and thinking are still only said on the launch line; absent rather than guessed.
      const launched = sessionEngine(sessionId);
      handlers.engine?.({ model, effort: launched?.effort || null, thinking: launched?.thinking || null });
    }
    const text = userMessageText(entry);
    if (text !== null) handlers.userMessage({ text, turn_id: entry.promptId || null });
  }, { intervalMs: POLL_MS });
  const timer = setInterval(() => {
    const working = sessionWorking(sessionId);
    if (working === null || working === lastStatus) return;
    lastStatus = working;
    handlers.working(working, {});
  }, POLL_MS);
  timer.unref?.();
  return () => { clearInterval(timer); stopTranscript(); };
}

export const claudeHarness = defineHarness({
  name: 'claude',
  capabilities: {
    deliver: SUPPORTED,
    inspectInbound: SUPPORTED,
    working: SUPPORTED,
    endOfTurn: SUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  deliver,
  inspectInbound,
  engine: sessionEngine,
  observe,
  sessionIdentity,
});

export default claudeHarness;
