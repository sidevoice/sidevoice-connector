/** Codex harness implementation.
 *
 * Delivery and identity are available to the stdio MCP façade. Configured UserPromptSubmit and
 * Stop hooks report the mechanical start and end of every interactive turn. */
import { execFile } from 'node:child_process';
import { defineHarness, envelope, SUPPORTED, UNSUPPORTED, WORKING_EVENT } from './harness-contract.mjs';

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

function endOfTurn(payload, env = process.env) {
  if (!payload || !/^stop$/i.test(String(payload.hook_event_name || payload.hookEventName || ''))) return null;
  const identity = sessionIdentity({ payload, env });
  return identity ? { thread: identity.thread, turn_id: payload.turn_id || null } : null;
}

/** A Codex lifecycle hook, normalized as a per-thread working transition. */
function working(payload, env = process.env) {
  const event = String(payload?.hook_event_name || payload?.hookEventName || '');
  if (!/^(userpromptsubmit|stop)$/i.test(event)) return null;
  const identity = sessionIdentity({ payload, env });
  if (!identity || typeof identity.thread !== 'string' || !identity.thread
      || typeof payload.turn_id !== 'string' || !payload.turn_id) return null;
  return {
    thread: identity.thread,
    turn_id: payload.turn_id || null,
    working: /^userpromptsubmit$/i.test(event),
  };
}

export const codexHarness = defineHarness({
  name: 'codex',
  workingSource: WORKING_EVENT,
  capabilities: {
    deliver: SUPPORTED,
    inspectInbound: UNSUPPORTED,
    working: SUPPORTED,
    endOfTurn: SUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  deliver,
  engine,
  working,
  endOfTurn,
  sessionIdentity,
});

export default codexHarness;
