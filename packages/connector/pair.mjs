#!/usr/bin/env node
/** One-time pairing: redeem the code shown by the room for this host's connector credential. */
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, writeFileSync } from 'node:fs';

const [room, code] = process.argv.slice(2);
if (!room || !code) { console.error('usage: pair.mjs <room-url> <pairing-code>'); process.exit(2); }
const base = new URL(room);
const response = await fetch(new URL('/api/connectors/pair', base), {
  method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ code, host: os.hostname() }),
  signal: AbortSignal.timeout(15_000),
});
const body = await response.json().catch(() => ({}));
if (!response.ok) { console.error('Pairing failed: ' + (body.detail || response.status)); process.exit(1); }
const ws = new URL('/api/connectors/ws', base); ws.protocol = base.protocol === 'https:' ? 'wss:' : 'ws:';
const dataDir = process.env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
mkdirSync(dataDir, { recursive: true, mode: 0o700 });
const file = path.join(dataDir, 'credentials.json');
writeFileSync(file, JSON.stringify({ url: ws.toString(), connector_id: body.connector_id, token: body.token, protocol: body.protocol }, null, 2), { mode: 0o600 });
console.log(`Paired with ${base.origin} as connector ${body.connector_id}; credential saved to ${file}`);
