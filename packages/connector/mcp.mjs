#!/usr/bin/env node
/** Stdio MCP façade for one conversation. It holds no connection to the room: it starts or reuses
 *  the host's connector and keeps one local connection to it for as long as this session lives. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { inspectInbound } from './harness-claude.mjs';

const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const connectorPath = fileURLToPath(new URL('./connector.mjs', import.meta.url));

const INSTRUCTIONS = `Sidevoice connects this conversation to the user's voice room.
- Call voice_connect only when the user asks to join the voice room or enable voice for this conversation; never as a side effect.
- Voice input arrives as a user message that starts with a JSON header ({"channel":"voice","session_id":...,"revision":...,"message_id":...}) followed by the user's literal words. Treat the header as opaque reply metadata; if the same message_id arrives twice, it is a redelivery: do not act on it again.
- For substantive work, one incoming voice message may receive multiple voice_say publications: an immediate acknowledgement that states what was understood and the next action, meaningful progress checkpoints while work continues, and a final result. Use the same original session_id and revision for every publication, with distinct utterances; do not manufacture filler or narrate every tool call.
- A progress publication is not itself a listening point. Divide substantive execution into bounded steps and, after each tool result or operational boundary, process newly arrived user input before starting the next step. Do not add artificial sleeps or fixed pauses.
- If a new user message arrives during active work, treat it as an addition, refinement, or replacement according to its meaning. Stop not-yet-started obsolete work, preserve completed work that remains useful, acknowledge the new interpretation before continuing, and do not later answer a stale request. A tool already running may finish before the correction takes effect; delegation is not a substitute for listening.
- A "published" voice_say result means the room stored it, not that the user heard it. If publication fails, continue in writing.
- If the user closes this conversation's voice channel from the room, the connection is removed: voice_say then fails saying so. Continue in writing and do not try to speak again; call voice_connect only when the user asks for voice again.
- voice_status reports whether the room can currently reach this conversation.
- On Claude Code, if a skill named voice-room is available, joining through it (/voice-room) is preferred: it registers, for this session only, the hook that gives the room read receipts and asks you to speak first.
- If voice_connect returns inbound.ok false, voice will look sent and never arrive: this harness holds or refuses messages posted by other local processes. Tell the user what inbound.reason says, offer inbound.remedy in your own words including what safeguard the machine-wide option removes, and let them choose. Do not change their settings without being asked to.`;

/** Who this façade speaks for, decided by what spawned it — never by the model. */
function identity(meta) {
  if (process.env.CLAUDE_CODE_SESSION_ID && process.env.CLAUDE_CODE_MESSAGING_SOCKET) {
    return { harness: 'claude', thread: process.env.CLAUDE_CODE_SESSION_ID,
      delivery: { kind: 'claude-uds', socket: process.env.CLAUDE_CODE_MESSAGING_SOCKET, token: process.env.CLAUDE_CODE_MESSAGING_TOKEN || '' } };
  }
  let turn = meta?.['x-codex-turn-metadata'] || {};
  if (typeof turn === 'string') { try { turn = JSON.parse(turn); } catch { turn = {}; } }
  const codexThread = meta?.['openai/threadId'] || meta?.['openai/thread_id'] || meta?.codexThreadId || meta?.codex_thread_id || turn.thread_id || process.env.CODEX_THREAD_ID;
  if (codexThread) {
    const delivery = process.env.SIDEVOICE_DELIVERY_URL ? { kind: 'http', url: process.env.SIDEVOICE_DELIVERY_URL, thread: codexThread } : { kind: 'codex-queue', thread: codexThread };
    return { harness: 'codex', thread: codexThread, delivery };
  }
  if (process.env.SIDEVOICE_THREAD && process.env.SIDEVOICE_DELIVERY_URL) {
    return { harness: process.env.SIDEVOICE_HARNESS || 'http', thread: process.env.SIDEVOICE_THREAD, delivery: { kind: 'http', url: process.env.SIDEVOICE_DELIVERY_URL, thread: process.env.SIDEVOICE_THREAD } };
  }
  throw new Error('Cannot tell which conversation this is: not launched by Claude Code or Codex, and no SIDEVOICE_THREAD/SIDEVOICE_DELIVERY_URL set');
}

// ----- one persistent connection to the connector -----
let ipc = null, ipcBuffer = '', ipcSerial = 0;
const ipcWaiting = new Map();
function connectIpc() {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once('error', reject);
    socket.on('connect', () => {
      socket.removeListener('error', reject);
      socket.on('error', () => {});
      socket.on('close', () => { if (ipc === socket) ipc = null; for (const w of ipcWaiting.values()) w.reject(new Error('Connector went away')); ipcWaiting.clear(); });
      socket.on('data', chunk => {
        ipcBuffer += chunk; let index;
        while ((index = ipcBuffer.indexOf('\n')) >= 0) {
          const line = ipcBuffer.slice(0, index); ipcBuffer = ipcBuffer.slice(index + 1);
          let reply; try { reply = JSON.parse(line); } catch { continue; }
          const waiting = ipcWaiting.get(reply.id); if (!waiting) continue; ipcWaiting.delete(reply.id);
          reply.ok ? waiting.resolve(reply.result) : waiting.reject(new Error(reply.error));
        }
      });
      ipc = socket; resolve(socket);
    });
  });
}
async function ensureConnector() {
  if (ipc) return ipc;
  try { return await connectIpc(); } catch {}
  const child = spawn(process.execPath, [connectorPath], { detached: true, stdio: 'ignore', env: process.env });
  child.unref();
  for (let attempt = 0; attempt < 40; attempt++) {
    await new Promise(r => setTimeout(r, 100));
    try { return await connectIpc(); } catch {}
  }
  throw new Error('The Sidevoice connector did not start (is this host paired? see docs/INSTALL.md)');
}
async function rpc(method, params) {
  await ensureConnector();
  const id = ++ipcSerial;
  return new Promise((resolve, reject) => { ipcWaiting.set(id, { resolve, reject }); ipc.write(JSON.stringify({ id, method, params }) + '\n'); });
}

// ----- tools -----
const tools = [
  { name: 'voice_connect', description: 'Connect this conversation to the voice room. Only on an explicit request to join or enable voice.',
    inputSchema: { type: 'object', properties: { title: { type: 'string', description: 'Short label for this conversation in the room' } }, additionalProperties: false } },
  { name: 'voice_say', description: 'Publish a concise spoken version of your reply to the room, with the session_id and revision from the voice message header.',
    inputSchema: { type: 'object', properties: { text: { type: 'string' }, session_id: { type: 'string' }, revision: { type: 'integer', minimum: 0 }, utterance_id: { type: 'string' }, language: { type: 'string', enum: ['es', 'en', 'fr', 'it', 'pt', 'hi'] } }, required: ['text', 'session_id', 'revision'], additionalProperties: false } },
  { name: 'voice_disconnect', description: 'Leave the voice room. The conversation and its work continue in writing.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
  { name: 'voice_status', description: 'Whether the room can currently reach this conversation.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
];
let binding = null;
async function invoke(name, args, meta) {
  if (name === 'voice_status') {
    const status = ipc ? await rpc('status', {}) : { connected: false, bindings: [], closed_by_room: [] };
    // The room may have closed this conversation's voice since we joined: the connector is the truth.
    const closed = !!binding && (status.closed_by_room || []).includes(binding.client_ref);
    if (closed) binding = null;
    const inbound = binding?.harness === 'claude' ? inspectInbound(binding.client_ref) : { ok: true };
    return { joined: !!binding, room_reachable: status.connected, room_error: status.room_error || null,
             binding_id: binding?.binding_id || null, harness: binding?.harness || null, inbound,
             ...(closed ? { closed_by_room: true, note: 'The user closed this conversation\'s voice channel from the room. Continue in writing; call voice_connect again only if they ask for voice.' } : {}) };
  }
  if (name === 'voice_connect') {
    const who = identity(meta);
    const title = (args.title || process.env.SIDEVOICE_TITLE || path.basename(process.cwd())).slice(0, 200);
    // Refuse rather than join a room we cannot hear from: a conversation whose harness holds
    // what the room posts would sit in the list looking present while the user talks to nobody.
    const inbound = who.harness === 'claude' ? inspectInbound(who.thread) : { ok: true };
    if (inbound.ok === false) {
      const error = new Error(`No se conecta esta conversación: ${inbound.reason} ${inbound.remedy}`);
      error.data = { inbound };
      throw error;
    }
    const result = await rpc('register', { client_ref: who.thread, harness: who.harness, thread: who.thread, title, delivery: who.delivery, inbound });
    binding = { ...result, harness: who.harness, client_ref: who.thread };
    return { status: result.pending ? 'joining' : 'joined', harness: who.harness, conversation: who.thread,
             binding_id: result.binding_id, delivery: 'push', room_reachable: result.connected, inbound };
  }
  if (!binding) throw new Error('Not connected to the voice room: call voice_connect first (only if the user asked).');
  if (name === 'voice_say') {
    let result;
    try {
      result = await rpc('publish', { binding_id: binding.binding_id, client_ref: binding.client_ref, text: args.text, session_id: args.session_id, revision: args.revision, utterance_id: args.utterance_id, language: args.language });
    } catch (error) {
      if (error.message === 'CLOSED_BY_ROOM') {
        binding = null;
        throw new Error('The user closed this conversation\'s voice channel from the room. Continue in writing and do not publish speech; call voice_connect again only if the user asks for voice.');
      }
      throw error;
    }
    return result.text_saved ? { status: 'published', text_saved: true, audio: result.status, reason: result.reason } : result;
  }
  if (name === 'voice_disconnect') { const result = await rpc('unregister', { binding_id: binding.binding_id }); binding = null; return { status: 'left', room_reachable: result.connected }; }
  throw new Error('Unknown tool');
}

// ----- JSON-RPC over stdio -----
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', async chunk => {
  input += chunk;
  while (input.includes('\n')) {
    const index = input.indexOf('\n'); const line = input.slice(0, index); input = input.slice(index + 1);
    if (!line.trim()) continue;
    let request; try { request = JSON.parse(line); } catch { continue; }
    if (request.id === undefined) continue; // notifications need no answer
    let result, error;
    try {
      if (request.method === 'initialize') result = { protocolVersion: request.params?.protocolVersion || '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'sidevoice', version: '0.2.0' }, instructions: INSTRUCTIONS };
      else if (request.method === 'tools/list') result = { tools };
      else if (request.method === 'tools/call') { const value = await invoke(request.params.name, request.params.arguments || {}, request.params._meta); result = { content: [{ type: 'text', text: JSON.stringify(value) }] }; }
      else if (request.method === 'ping') result = {};
      else throw Object.assign(new Error('Method not found'), { code: -32601 });
    } catch (e) { error = { code: e.code || -32603, message: e.message }; }
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, ...(error ? { error } : { result }) }) + '\n');
  }
});
process.stdin.on('end', () => { ipc?.end(); process.exit(0); });
