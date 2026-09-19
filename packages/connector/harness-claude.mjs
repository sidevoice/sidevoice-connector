/** What Claude Code will do with a message we post to a session's inbox, decided before we post it.
 *
 *  A session that bypasses permission prompts holds an injected message for its user's approval
 *  instead of delivering it, and the inbox sends us no receipt to say so — the write looks
 *  identical either way. So the only honest moment to find out is at voice_connect, from the
 *  session's own launch flags and settings. Best effort by design: a managed policy layer we
 *  cannot read could tighten this further, and the result says so rather than pretending.
 *  Documented at https://code.claude.com/docs/en/cross-session-messaging */
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

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
