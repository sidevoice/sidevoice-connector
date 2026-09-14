#!/usr/bin/env node
/** Stdio MCP façade. It never owns a WebSocket; connector.mjs does. */
import net from 'node:net';
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
const socketPath = process.env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
const connectorPath = fileURLToPath(new URL('./connector.mjs', import.meta.url));
const defaultConversation = process.env.CODEX_THREAD_ID;
function rpc(command) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath); let text = '';
    socket.once('error', reject); socket.on('connect', () => socket.write(JSON.stringify(command) + '\n'));
    socket.on('data', chunk => { text += chunk; const line = text.indexOf('\n'); if (line < 0) return; const reply = JSON.parse(text.slice(0, line)); socket.end(); reply.ok ? resolve(reply.result) : reject(new Error(reply.error)); });
  });
}
async function ensure() {
  try { await rpc({ method: 'status' }); return; } catch {}
  const child = spawn(process.execPath, [connectorPath], { detached: true, stdio: 'ignore', env: process.env }); child.unref();
  for (let i = 0; i < 30; i++) { await new Promise(r => setTimeout(r, 100)); try { await rpc({ method: 'status' }); return; } catch {} }
  throw new Error('Sidevoice connector did not start');
}
const tools = [
  { name: 'voice_connect', description: 'Connect this existing conversation to a Sidevoice room.', inputSchema: { type: 'object', properties: { room_id: { type: 'string' }, conversation_id: { type: 'string' }, delivery_url: { type: 'string' } }, required: ['room_id', 'delivery_url'] } },
  { name: 'voice_say', description: 'Publish a concise spoken presentation through the active Sidevoice binding.', inputSchema: { type: 'object', properties: { binding_id: { type: 'string' }, text: { type: 'string' }, language: { type: 'string' } }, required: ['binding_id', 'text'] } },
  { name: 'voice_disconnect', description: 'Disconnect a Sidevoice binding without stopping the task.', inputSchema: { type: 'object', properties: { binding_id: { type: 'string' } }, required: ['binding_id'] } },
  { name: 'voice_status', description: 'Read Sidevoice connector and binding status.', inputSchema: { type: 'object', properties: {} } },
];
async function invoke(name, args) {
  if (name === 'voice_status') return rpc({ method: 'status' });
  await ensure();
  if (name === 'voice_connect') {
    const conversation_id = args.conversation_id || defaultConversation;
    if (!conversation_id) throw new Error('conversation_id is required outside a Codex task');
    const binding_id = randomUUID();
    return rpc({ method: 'register', binding: { binding_id, room_id: args.room_id, conversation_id, delivery_url: args.delivery_url } });
  }
  if (name === 'voice_say') return rpc({ method: 'publish', binding_id: args.binding_id, text: args.text, language: args.language });
  if (name === 'voice_disconnect') return rpc({ method: 'unregister', binding_id: args.binding_id });
  throw new Error('Unknown tool');
}
let input = '';
process.stdin.setEncoding('utf8'); process.stdin.on('data', async chunk => {
  input += chunk;
  while (input.includes('\n')) {
    const index = input.indexOf('\n'); const line = input.slice(0, index); input = input.slice(index + 1); if (!line) continue;
    const request = JSON.parse(line); let result, error;
    try {
      if (request.method === 'initialize') result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'sidevoice', version: '0.1.0' } };
      else if (request.method === 'tools/list') result = { tools };
      else if (request.method === 'tools/call') result = { content: [{ type: 'text', text: JSON.stringify(await invoke(request.params.name, request.params.arguments || {})) }] };
      else if (request.method === 'ping') result = {};
      else throw new Error('Unsupported MCP method');
    } catch (e) { error = { code: -32603, message: e.message }; }
    if (request.id !== undefined) process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, ...(error ? { error } : { result }) }) + '\n');
  }
});
