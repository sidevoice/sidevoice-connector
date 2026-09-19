#!/usr/bin/env node
/** Harness hook: `sidevoice hook` on stdin gets the harness's hook payload and, when the prompt being admitted is a
 *  Sidevoice voice message, tells the connector the conversation has read it (the room shows the second tick) and
 *  hands the harness a line of context so the model speaks first. It never blocks the harness: any failure exits 0. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { capabilityState, SUPPORTED } from './harness-contract.mjs';
import { identifyHookHarness } from './harnesses.mjs';

const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');

/** The envelope a harness delivery module puts before the user's words. Anything else is not ours. */
export function voiceEnvelope(prompt) {
  if (typeof prompt !== 'string' || !prompt.startsWith('{')) return null;
  const end = prompt.indexOf('}');
  if (end < 0) return null;
  let header;
  try { header = JSON.parse(prompt.slice(0, end + 1)); } catch { return null; }
  if (!header || header.channel !== 'voice' || !header.message_id || !header.session_id || !Number.isInteger(header.revision)) return null;
  return { message_id: header.message_id, session_id: header.session_id, revision: header.revision, text: prompt.slice(end + 1).trim() };
}

/** What this hook invocation means, or null when it is not a Sidevoice voice message being admitted. */
export function interpret(payload, env = process.env) {
  if (!payload || typeof payload !== 'object') return null;
  const event = payload.hook_event_name || payload.hookEventName;
  if (event && !/^userpromptsubmit$/i.test(String(event))) return null;
  const envelope = voiceEnvelope(payload.prompt);
  if (!envelope) return null;
  const identity = identifyHookHarness(payload, env);
  return identity ? { thread: identity.thread, turn_id: payload.turn_id || null, ...envelope } : null;
}

/** A harness-native end-of-turn signal, normalized by that harness's module. */
export function interpretTurnEnd(payload, env = process.env) {
  const identity = identifyHookHarness(payload, env);
  if (!identity || capabilityState(identity.module, 'endOfTurn') !== SUPPORTED) return null;
  return identity.module.endOfTurn(payload, env);
}

/** The context handed back to the harness: the acknowledgement is asked for at the moment the message is read. */
export function nudge(reading) {
  return `A voice message just arrived from the room (session ${reading.session_id}, revision ${reading.revision}). `
    + 'Before any other tool, publish a short spoken acknowledgement with voice_say that says what you understood and what you will do next, '
    + 'using that session_id and revision; then continue the work and publish the result by voice as well.';
}

function report(method, params, timeoutMs = 1500) {
  return new Promise(resolve => {
    const socket = net.createConnection(socketPath);
    const done = value => { clearTimeout(timer); socket.destroy(); resolve(value); };
    const timer = setTimeout(() => done({ ok: false, error: 'timeout' }), timeoutMs);
    let buffer = '';
    socket.on('error', error => done({ ok: false, error: error.code || error.message }));
    socket.on('connect', () => socket.write(JSON.stringify({ id: 1, method, params }) + '\n'));
    socket.on('data', chunk => {
      buffer += chunk; const index = buffer.indexOf('\n');
      if (index < 0) return;
      try { done(JSON.parse(buffer.slice(0, index))); } catch { done({ ok: false, error: 'bad reply' }); }
    });
  });
}

async function main() {
  let raw = '';
  for await (const chunk of process.stdin) raw += chunk;
  let payload = null;
  try { payload = JSON.parse(raw); } catch {}
  const ended = interpretTurnEnd(payload);
  if (ended) {
    const outcome = await report('turn_end', ended);
    if (!outcome.ok) console.error('[sidevoice hook] end-of-turn report not sent: ' + (outcome.error || 'unknown'));
    return;
  }
  const reading = interpret(payload);
  if (!reading) return;
  const outcome = await report('read', reading);
  if (!outcome.ok) console.error('[sidevoice hook] read receipt not sent: ' + (outcome.error || 'unknown'));
  if (process.env.SIDEVOICE_HOOK_NUDGE !== '0') {
    process.stdout.write(JSON.stringify({ hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext: nudge(reading) } }) + '\n');
  }
}

if (process.argv[1] && /hook\.mjs$/.test(process.argv[1]) || process.env.SIDEVOICE_HOOK_MAIN === '1') {
  main().catch(error => { console.error('[sidevoice hook] ' + (error?.message || error)); }).finally(() => process.exit(0));
}
