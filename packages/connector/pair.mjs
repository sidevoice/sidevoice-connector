#!/usr/bin/env node
/** One-time pairing: redeem the code shown by the room for this host's connector credential.
 *
 *  The code is the room's to give and the person's to carry: the room shows it to whoever is in it
 *  ("Emparejar conector"), and only that person can hand it to this machine. Nothing here asks the
 *  room for one — a caller that could would turn "you can reach the address" into "you are in the room". */
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';

export function dataDir(env = process.env) {
  return env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
}

/** Where a plaintext connection is acceptable: the token must never cross a network we do not own. Loopback,
 *  and a Kubernetes service name (`<svc>.<ns>.svc`, `<svc>.<ns>.svc.<cluster domain>`), which by construction
 *  resolves only inside the cluster and is routed there. Anything else — a private IP included — needs TLS: a
 *  host we cannot classify is not a reason to send a credential in clear. */
export function privateNetwork(hostname) {
  if (['127.0.0.1', 'localhost', '::1', '[::1]'].includes(hostname)) return true;
  return /^[a-z0-9-]+\.[a-z0-9-]+\.svc(\.[a-z0-9.-]+)?$/i.test(hostname);
}

/** The room's http(s) origin from the socket address the credential stores. */
export function roomOrigin(wsUrl) {
  const url = new URL(wsUrl); url.protocol = url.protocol === 'wss:' ? 'https:' : 'http:'; return url.origin;
}

/** Which room this machine is paired with, or null. */
export function pairedRoom(env = process.env) {
  try {
    const saved = JSON.parse(readFileSync(path.join(dataDir(env), 'credentials.json'), 'utf8'));
    if (!saved.url || !saved.connector_id || !saved.token) return null;
    return { origin: roomOrigin(saved.url), connector_id: saved.connector_id };
  } catch { return null; }
}

/** Redeem a code for this host's credential. Returns where it was written. */
export async function pair(room, code, env = process.env) {
  const base = new URL(room);
  if (base.protocol !== 'https:' && !privateNetwork(base.hostname)) {
    throw new Error(`${base.origin} is reached in clear over a network this machine does not own; the room must be https:// there (loopback and Kubernetes service names are the exceptions).`);
  }
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
  if (!room || !code) { console.error('usage: sidevoice pair <room-url> <pairing-code>   (the code is shown in the room under "Emparejar conector")'); process.exit(2); }
  try {
    const result = await pair(room, code);
    console.log(`Paired with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
  } catch (error) { console.error(error.message); process.exit(1); }
}
