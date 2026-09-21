#!/usr/bin/env node
/** One-time pairing: redeem the code shown by the room for this host's connector credential. */
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, writeFileSync } from 'node:fs';

export function dataDir(env = process.env) {
  return env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
}

/** Ask the room for a code. Only a caller that can already reach the room can do this, which is the
 *  gate that matters: the code is a handshake, not a secret. A room that refuses says so and the
 *  person reads one from its interface instead. */
export async function requestCode(room) {
  const response = await fetch(new URL('/api/connectors/pairing-code', new URL(room)), {
    method: 'POST', signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok) throw new Error(`The room would not issue a pairing code (${response.status}). Read one from its interface.`);
  const body = await response.json();
  return body.code;
}

/** Redeem a code for this host's credential. Returns where it was written. */
export async function pair(room, code, env = process.env) {
  const base = new URL(room);
  const response = await fetch(new URL('/api/connectors/pair', base), {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ code, host: os.hostname() }), signal: AbortSignal.timeout(15_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error('Pairing failed: ' + (body.detail || response.status));
  const ws = new URL('/api/connectors/ws', base); ws.protocol = base.protocol === 'https:' ? 'wss:' : 'ws:';
  const directory = dataDir(env);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const file = path.join(directory, 'credentials.json');
  writeFileSync(file, JSON.stringify({ url: ws.toString(), connector_id: body.connector_id, token: body.token, protocol: body.protocol }, null, 2), { mode: 0o600 });
  return { file, connector_id: body.connector_id, origin: base.origin };
}

if (process.env.SIDEVOICE_PAIR_MAIN === '1') {
  const [room, code] = process.argv.slice(2);
  if (!room || !code) { console.error('usage: sidevoice pair <room-url> <pairing-code>'); process.exit(2); }
  try {
    const result = await pair(room, code);
    console.log(`Paired with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
  } catch (error) { console.error(error.message); process.exit(1); }
}
