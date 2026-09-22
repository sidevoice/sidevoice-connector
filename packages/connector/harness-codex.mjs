/** Codex harness implementation.
 *
 * Delivery and identity are available to the stdio MCP façade. Working state and read receipts come
 * from the thread's own rollout file, which Codex appends as the turn runs; nothing is configured in Codex. */
import { execFile } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { defineHarness, envelope, SUPPORTED, UNSUPPORTED, tailJsonl } from './harness-contract.mjs';

function turnMetadata(meta) {
  let turn = meta?.['x-codex-turn-metadata'] || {};
  if (typeof turn === 'string') {
    try { turn = JSON.parse(turn); } catch { turn = {}; }
  }
  return turn;
}

function sessionIdentity({ meta, env = process.env, payload } = {}) {
  const turn = turnMetadata(meta);
  const thread = meta?.['openai/threadId'] || meta?.['openai/thread_id'] || meta?.codexThreadId
    || meta?.codex_thread_id || turn.thread_id || payload?.session_id || payload?.thread_id
    || env.CODEX_THREAD_ID;
  if (!thread) return null;
  const delivery = env.SIDEVOICE_DELIVERY_URL
    ? { kind: 'http', url: env.SIDEVOICE_DELIVERY_URL, thread }
    : { kind: 'codex-queue', thread };
  return { harness: 'codex', thread, delivery };
}

function deliver(delivery, event) {
  if (delivery?.kind === 'http') {
    return fetch(delivery.url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ thread_id: delivery.thread, text: event.text, message_id: event.message_id,
        session_id: event.session_id, revision: event.revision, channel: event.channel || 'voice' }),
      signal: AbortSignal.timeout(30_000),
    }).then(async response => {
      if (!response.ok) throw new Error(`Harness delivery failed (${response.status})`);
      return { status: 'accepted', detail: `receiver answered ${response.status}` };
    });
  }
  if (delivery?.kind !== 'codex-queue') throw new Error(`Unsupported Codex delivery kind: ${delivery?.kind}`);
  return new Promise((resolve, reject) => {
    const binary = process.env.SIDEVOICE_CODEX_BIN || 'codex';
    const args = ['queue', '--thread', delivery.thread, '--message', envelope(event)];
    execFile(binary, args, { timeout: 30_000, maxBuffer: 1 << 20 }, (error, stdout, stderr) => {
      if (error) return reject(new Error((stderr || stdout || error.message).toString().trim().slice(0, 400)));
      resolve({ status: 'accepted', detail: 'codex queue confirmed the thread' });
    });
  });
}

/** Which model this thread runs, from the launch line of the process that owns it, when it says. */
function engine(thread, env = process.env) {
  const named = env.CODEX_MODEL || null;
  return named ? { model: named, effort: env.CODEX_REASONING_EFFORT || null, thinking: null } : null;
}

const codexHome = () => process.env.CODEX_HOME || path.join(os.homedir(), '.codex');

/** The rollout Codex writes for a thread: `sessions/YYYY/MM/DD/rollout-<stamp>-<thread id>.jsonl`. Found by
 *  its name, newest day first, so nothing has to be asked of Codex's database. */
export function rolloutPath(threadId) {
  const root = path.join(codexHome(), 'sessions');
  const list = dir => { try { return readdirSync(dir).sort().reverse(); } catch { return []; } };
  for (const year of list(root)) for (const month of list(path.join(root, year))) for (const day of list(path.join(root, year, month))) {
    const dir = path.join(root, year, month, day);
    const file = list(dir).find(name => name.endsWith('-' + threadId + '.jsonl'));
    if (file) return path.join(dir, file);
  }
  return null;
}

/** What one rollout line says, in the terms of the contract: a working transition, a user message, or nothing. */
export function interpretRollout(entry, state = {}) {
  const payload = entry?.payload || {};
  if (entry?.type === 'event_msg') {
    if (payload.type === 'task_started' && payload.turn_id) { state.turn_id = payload.turn_id; return { working: true, turn_id: payload.turn_id }; }
    if ((payload.type === 'task_complete' || payload.type === 'turn_aborted') && payload.turn_id) {
      if (state.turn_id === payload.turn_id) state.turn_id = null;
      return { working: false, turn_id: payload.turn_id };
    }
    return null;
  }
  if (entry?.type === 'response_item' && payload.type === 'message' && payload.role === 'user') {
    const text = (payload.content || []).filter(part => part?.type === 'input_text').map(part => part.text).join('\n');
    return text ? { text, turn_id: state.turn_id || null } : null;
  }
  return null;
}

/** The model a rollout line says the thread thinks with. Codex writes it on the `turn_context` of every
 *  turn — `{"type":"turn_context","payload":{"model":"gpt-5.6-terra", ...}}` — and on the `session_meta`
 *  that opens the file when that one says it, so a thread nobody launched with `CODEX_MODEL` still says
 *  what it thinks with. */
export function rolloutModel(entry) {
  if (entry?.type !== 'turn_context' && entry?.type !== 'session_meta') return null;
  const model = entry.payload?.model;
  return typeof model === 'string' && model ? model : null;
}

const POLL_MS = Number(process.env.SIDEVOICE_WORK_POLL_MS || 400);

/** Watch one thread through its rollout: task_started/task_complete are the turn, a user message is the
 *  moment the thread took it (a queued message is written when the turn starts on it), and turn_context
 *  names the model. What the rollout already holds is read first, silently, so a turn that was running
 *  before we looked — and the model it was already thinking with — is reported once the replay is over;
 *  old messages are not re-read. */
export function observe(threadId, handlers, env = process.env) {
  const state = {};
  let working = null, model = null, reported = null;
  const sayModel = () => {
    if (!model || model === reported) return;
    reported = model;
    // Effort and thinking are still only said by the environment the thread was launched in.
    const launched = engine(threadId, env) || {};
    handlers.engine?.({ model, effort: launched.effort || null, thinking: launched.thinking || null });
  };
  return tailJsonl(() => rolloutPath(threadId), (entry, replayed) => {
    const named = rolloutModel(entry);
    if (named) { model = named; if (!replayed) sayModel(); }
    const seen = interpretRollout(entry, state);
    if (!seen) return;
    if (typeof seen.working === 'boolean') { working = seen.working; if (!replayed) handlers.working(seen.working, { turn_id: seen.turn_id }); }
    else if (!replayed) handlers.userMessage({ text: seen.text, turn_id: seen.turn_id });
  }, { intervalMs: POLL_MS, catchUp: true, caughtUp: () => {
    if (working !== null) handlers.working(working, { turn_id: working ? state.turn_id : null });
    sayModel();
  } });
}

export const codexHarness = defineHarness({
  name: 'codex',
  capabilities: {
    deliver: SUPPORTED,
    inspectInbound: UNSUPPORTED,
    working: SUPPORTED,
    endOfTurn: SUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  deliver,
  engine,
  observe,
  sessionIdentity,
});

export default codexHarness;
