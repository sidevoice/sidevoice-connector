#!/usr/bin/env node
/** Stdio MCP façade for one conversation. It holds no connection to the room: it starts or reuses
 *  the host's connector and keeps one local connection to it for as long as this session lives. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { advertisedCapabilities, capabilityState, SUPPORTED } from './harness-contract.mjs';
import { harnessFor, identifyHarness } from './harnesses.mjs';
import { pair, pairedRoom } from './pair.mjs';
import { readFileSync } from 'node:fs';

const VERSION = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')).version;

const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const connectorPath = fileURLToPath(new URL('./connector.mjs', import.meta.url));

const INSTRUCTIONS = `Sidevoice connects this conversation to the user's voice room.
- Call voice_connect only when the user asks to join the voice room or enable voice for this conversation; never as a side effect.
- Voice input arrives as a user message that starts with a JSON header ({"channel":"voice","session_id":...,"revision":...,"message_id":...}) followed by the user's literal words, and ends with a line marked [Sidevoice] that is not the user's words: it asks you to acknowledge by voice first. Treat the header as opaque reply metadata; if the same message_id arrives twice, it is a redelivery: do not act on it again.
- For substantive work, one incoming voice message may receive multiple voice_say publications: an immediate acknowledgement that states what was understood and the next action, meaningful progress checkpoints while work continues, and a final result. Use the same original session_id and revision for every publication, with distinct utterances; do not manufacture filler or narrate every tool call.
- A progress publication is not itself a listening point. Divide substantive execution into bounded steps and, after each tool result or operational boundary, process newly arrived user input before starting the next step. Do not add artificial sleeps or fixed pauses.
- If a new user message arrives during active work, treat it as an addition, refinement, or replacement according to its meaning. Stop not-yet-started obsolete work, preserve completed work that remains useful, acknowledge the new interpretation before continuing, and do not later answer a stale request. A tool already running may finish before the correction takes effect; delegation is not a substitute for listening.
- A "published" voice_say result means the room stored it, not that the user heard it. If publication fails, continue in writing.
- If the user closes this conversation's voice channel from the room, the connection is removed: voice_say then fails saying so. Continue in writing and do not try to speak again; call voice_connect only when the user asks for voice again.
- voice_status reports whether the room can currently reach this conversation, and which room this machine is paired with.
- Pairing is the user's act, never yours. If voice_connect answers that this machine is not paired with the room (or is paired with a different one), ask the user for the room's address and the one-time pairing code the room shows them under "Emparejar conector" (it expires in ten minutes), then call voice_pair with both and voice_connect again. Never try to obtain a code from the room yourself, and do not offer to: the room only shows it to the person in it.
- On Claude Code, /voice-room is a shortcut for the same joining steps. Read receipts and working state need nothing from you: the room learns them from what the harness records about this conversation.
- If voice_connect returns inbound.ok false, voice will look sent and never arrive: this harness holds or refuses messages posted by other local processes. Tell the user what inbound.reason says, offer inbound.remedy in your own words including what safeguard the machine-wide option removes, and let them choose. Do not change their settings without being asked to.`;

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
  { name: 'voice_connect', description: 'Connect this conversation to the voice room. Only on an explicit request to join or enable voice. Fails, saying what to ask the user, when this machine is not paired with the room.',
    inputSchema: { type: 'object', properties: { title: { type: 'string', description: 'Short label for this conversation in the room' }, room: { type: 'string', description: 'The room\'s address (https://…) when the user names one; omitted, the room this machine is paired with' } }, additionalProperties: false } },
  { name: 'voice_pair', description: 'Pair this machine with a room using the one-time code the user read from the room\'s interface ("Emparejar conector"). Only with a code the user gave you; one room per machine, a new pairing replaces the previous one.',
    inputSchema: { type: 'object', properties: { room: { type: 'string', description: 'The room\'s address (https://…)' }, code: { type: 'string', description: 'The one-time pairing code shown by the room' } }, required: ['room', 'code'], additionalProperties: false } },
  { name: 'voice_say', description: 'Publish a concise spoken version of your reply to the room, with the session_id and revision from the voice message header.',
    inputSchema: { type: 'object', properties: { text: { type: 'string' }, session_id: { type: 'string' }, revision: { type: 'integer', minimum: 0 }, utterance_id: { type: 'string' }, language: { type: 'string', enum: ['es', 'en', 'fr', 'it', 'pt', 'hi'] } }, required: ['text', 'session_id', 'revision'], additionalProperties: false } },
  { name: 'voice_disconnect', description: 'Leave the voice room. The conversation and its work continue in writing.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
  { name: 'voice_status', description: 'Whether the room can currently reach this conversation.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
];
let binding = null;
function originOf(room) {
  try { return new URL(room).origin; } catch { throw new Error(`"${room}" is not a room address; expected something like https://voice.example`); }
}
/** Ask the user, do not guess: the code exists only on the room's screen. */
function pairingNeeded(room, paired) {
  const target = room ? originOf(room) : null;
  if (!paired) return `This machine is not paired with ${target ? 'the room at ' + target : 'any room'}. Ask the user for the room's address${target ? ' (confirm ' + target + ')' : ''} and the one-time pairing code the room shows under "Emparejar conector", then call voice_pair with both. Do not fetch a code yourself.`;
  if (target && target !== paired.origin) return `This machine is paired with ${paired.origin}, not ${target}. One room per machine: to switch, ask the user for the pairing code that ${target} shows under "Emparejar conector" and call voice_pair (it replaces the current pairing); to stay, call voice_connect without a room.`;
  return null;
}
/** A connector from another version serves this conversation with that version's behaviour. */
function versionNote(connectorVersion) {
  if (!ipc || connectorVersion === VERSION) return {};
  return { note: `The connector running on this machine is ${connectorVersion ? 'version ' + connectorVersion : 'older than this server'}; this conversation runs ${VERSION}. It exits 15 s after the last conversation leaves it; until then behaviour is that version's.` };
}
function inboundFor(harness, thread) {
  return capabilityState(harness, 'inspectInbound') === SUPPORTED ? harness.inspectInbound(thread) : null;
}
async function invoke(name, args, meta) {
  if (name === 'voice_status') {
    const status = ipc ? await rpc('status', {}) : { connected: false, bindings: [], closed_by_room: [] };
    // The room may have closed this conversation's voice since we joined: the connector is the truth.
    const closed = !!binding && (status.closed_by_room || []).includes(binding.client_ref);
    if (closed) binding = null;
    const module = binding ? harnessFor(binding.harness) : null;
    const inbound = binding ? inboundFor(module, binding.client_ref) : null;
    return { joined: !!binding, room: status.room || pairedRoom()?.origin || null, room_reachable: status.connected, room_error: status.room_error || null,
             version: VERSION, connector_version: status.version || null, ...versionNote(status.version),
             binding_id: binding?.binding_id || null, harness: binding?.harness || null,
             capabilities: binding?.capabilities || null, inbound,
             ...(closed ? { closed_by_room: true, note: 'The user closed this conversation\'s voice channel from the room. Continue in writing; call voice_connect again only if they ask for voice.' } : {}) };
  }
  if (name === 'voice_pair') {
    if (!args.room || !args.code) throw new Error('voice_pair needs the room\'s address and the code the user read from it.');
    const previous = pairedRoom();
    // The room shows the code in upper case and compares it that way; a dictated one arrives however it was heard.
    const result = await pair(originOf(args.room), String(args.code).trim().toUpperCase());
    // The connector that is up, if any, was started for the previous credential: let go of it so it can
    // exit, and the next voice_connect starts one for this room. Other conversations still bound to the
    // previous room keep that connector alive until they leave; they are not moved.
    if (binding) { try { await rpc('unregister', { binding_id: binding.binding_id }); } catch {} binding = null; }
    if (ipc) { ipc.end(); ipc = null; }
    return { status: 'paired', room: result.origin, connector_id: result.connector_id,
             ...(previous && previous.origin !== result.origin ? { replaced: previous.origin, note: 'Conversations on this machine still joined to the previous room keep it until they leave.' } : {}),
             next: 'Call voice_connect to join.' };
  }
  if (name === 'voice_connect') {
    const needed = pairingNeeded(args.room, pairedRoom());
    if (needed) { const error = new Error(needed); error.data = { pairing_needed: true, room: args.room ? originOf(args.room) : null }; throw error; }
    const who = identifyHarness(meta);
    const title = (args.title || process.env.SIDEVOICE_TITLE || path.basename(process.cwd())).slice(0, 200);
    // Refuse rather than join a room we cannot hear from: a conversation whose harness holds
    // what the room posts would sit in the list looking present while the user talks to nobody.
    const inbound = inboundFor(who.module, who.thread);
    if (inbound?.ok === false) {
      const error = new Error(`No se conecta esta conversación: ${inbound.reason} ${inbound.remedy}`);
      error.data = { inbound };
      throw error;
    }
    const capabilities = advertisedCapabilities(who.module);
    // Which model is answering, read from the session's own launch line rather than asked of the model.
    let engine = null;
    try { engine = who.module.engine?.(who.thread) || null; } catch { engine = null; }
    const result = await rpc('register', { client_ref: who.thread, harness: who.harness, thread: who.thread,
      title, delivery: who.delivery, inbound, capabilities, engine });
    binding = { ...result, harness: who.harness, client_ref: who.thread, capabilities };
    let connectorVersion = null; try { connectorVersion = (await rpc('status', {})).version || null; } catch {}
    return { status: result.pending ? 'joining' : 'joined', harness: who.harness, conversation: who.thread,
             binding_id: result.binding_id, delivery: 'push', room_reachable: result.connected, capabilities, inbound,
             version: VERSION, connector_version: connectorVersion, ...versionNote(connectorVersion) };
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
      if (request.method === 'initialize') result = { protocolVersion: request.params?.protocolVersion || '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'sidevoice', version: VERSION }, instructions: INSTRUCTIONS };
      else if (request.method === 'tools/list') result = { tools };
      else if (request.method === 'tools/call') { const value = await invoke(request.params.name, request.params.arguments || {}, request.params._meta); result = { content: [{ type: 'text', text: JSON.stringify(value) }] }; }
      else if (request.method === 'ping') result = {};
      else throw Object.assign(new Error('Method not found'), { code: -32601 });
    } catch (e) { error = { code: e.code || -32603, message: e.message }; }
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, ...(error ? { error } : { result }) }) + '\n');
  }
});
process.stdin.on('end', () => { ipc?.end(); process.exit(0); });
