#!/usr/bin/env node
/** One-time pairing: redeem a room's code for this host's connector credential — the credential this
 *  machine's core links with the room by.
 *
 *  The code comes from the room's page ("Emparejar máquina"), carried by the person (`sidevoice pair`,
 *  `voice_pair`), or this machine asks the room for one itself (`sidevoice link-room`). Registering with a
 *  room is open by decision: a room is a relay and grants nothing by itself — which device may use this
 *  machine is this machine's own pairing (`docs/DEVICE_PAIRING.md`). The conversation never asks the room
 *  for a code: linking a room is still the person's act, by hand. */
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { machineIdentity } from './identity.mjs';

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

/** The room's http(s) origin, from whatever address is to hand. A credential written by an older
 *  version names a socket and a path; the room is the same room, and the path is no longer the
 *  credential's to remember — the room will refuse that credential's protocol and say to pair again. */
export function roomOrigin(address) {
  const url = new URL(address);
  url.protocol = url.protocol === 'wss:' ? 'https:' : url.protocol === 'ws:' ? 'http:' : url.protocol;
  return url.origin;
}

/** Which room this machine is paired with, or null. */
export function pairedRoom(env = process.env) {
  try {
    const saved = JSON.parse(readFileSync(path.join(dataDir(env), 'credentials.json'), 'utf8'));
    if (!saved.url || !saved.connector_id || !saved.token) return null;
    return { origin: roomOrigin(saved.url), connector_id: saved.connector_id };
  } catch { return null; }
}

/** The room's address, if this machine may send it a credential or a code: in clear only where neither can
 *  leave the machine or the cluster. */
function roomBase(room) {
  const base = new URL(room);
  if (base.protocol !== 'https:' && !privateNetwork(base.hostname)) {
    throw new Error(`${base.origin} is reached in clear over a network this machine does not own; the room must be https:// there (loopback and Kubernetes service names are the exceptions).`);
  }
  return base;
}

/** Redeem a code for this host's credential. Returns where it was written. */
export async function pair(room, code, env = process.env) {
  const base = roomBase(room);
  // The code, and who this machine is: the room lists what it is told, so it is told here too and
  // not only on every connection — a machine paired and never yet connected still reads as a machine.
  const response = await fetch(new URL('/api/connectors/pair', base), {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ code, ...machineIdentity(env) }), signal: AbortSignal.timeout(15_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error('Pairing failed: ' + (body.detail || response.status));
  const directory = dataDir(env);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const file = path.join(directory, 'credentials.json');
  // Where the room is, not how to reach it: the path and namespace that carry the link belong to
  // the client and move with its version, so an upgrade never has to rewrite what pairing wrote.
  // The dial key is what the room shows this machine's core if the room is ever the one to open the link.
  writeFileSync(file, JSON.stringify({ url: base.origin, connector_id: body.connector_id, token: body.token, protocol: body.protocol,
    ...(body.dial_key ? { dial_key: body.dial_key } : {}) }, null, 2), { mode: 0o600 });
  return { file, connector_id: body.connector_id, origin: base.origin };
}

/** Link this machine with a room with no page in between: ask it for a code as its own page does — naming
 *  the room as the origin, which is all the room asks of a caller — and redeem it at once. */
export async function linkRoom(room, env = process.env) {
  const base = roomBase(room);
  const response = await fetch(new URL('/api/connectors/pairing-code', base), {
    method: 'POST', headers: { origin: base.origin }, signal: AbortSignal.timeout(15_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || typeof body.code !== 'string' || !body.code) throw new Error(`${base.origin} gave no pairing code: ${body.detail || response.status}`);
  return pair(base.origin, body.code, env);
}

if (process.env.SIDEVOICE_PAIR_MAIN === '1') {
  const [room, code] = process.argv.slice(2);
  if (!room || !code) { console.error('usage: sidevoice pair <room-url> <pairing-code>   (the code is shown in the room under "Emparejar máquina")'); process.exit(2); }
  try {
    const result = await pair(room, code);
    console.log(`Paired with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
  } catch (error) { console.error(error.message); process.exit(1); }
}

if (process.env.SIDEVOICE_LINK_ROOM_MAIN === '1') {
  const [room] = process.argv.slice(2);
  if (!room) { console.error('usage: sidevoice link-room <room-url>   (links this machine with that room; no code needed)'); process.exit(2); }
  try {
    const result = await linkRoom(room);
    console.log(`Linked with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
  } catch (error) { console.error(error.message); process.exit(1); }
}
