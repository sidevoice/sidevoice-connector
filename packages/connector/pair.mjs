/** One-time pairing: redeem a room's code for this host's connector credential — the credential this
 *  machine's core links with the room by.
 *
 *  The code comes from the room's page ("Emparejar máquina"), carried by the person (`sidevoice pair`,
 *  `voice_pair`), or this machine asks the room for one itself (`sidevoice link-room`). Registering with a
 *  room is open by decision: a room is a relay and grants nothing by itself — which device may use this
 *  machine is this machine's own pairing (sidevoice-core's `server/devices.py`). The conversation never asks the room
 *  for a code: linking a room is still the person's act, by hand. */
import os from 'node:os';
import path from 'node:path';
import { readTrustedJson, writePrivateFile } from './secure-fs.mjs';
import { machineIdentity } from './identity.mjs';
import { t } from './i18n.mjs';
import { askConnector } from './service.mjs';

export function dataDir(env = process.env) {
  return env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
}

/** The hosts whoever runs this machine named as reachable in clear: `SIDEVOICE_TRUSTED_CLUSTER_HOSTS`, comma-separated. An
 *  entry starting with a dot is a suffix (`.svc.cluster.local`), any other one exact host. Empty by default. */
export function trustedClusterHosts(env = process.env) {
  return String(env.SIDEVOICE_TRUSTED_CLUSTER_HOSTS || '').split(',').map(entry => entry.trim().toLowerCase())
    .filter(entry => entry.replace(/\./g, ''));
}

/** Where a credential may travel in clear: loopback, and the hosts in `SIDEVOICE_TRUSTED_CLUSTER_HOSTS` — the same
 *  rule sidevoice-core applies when it dials the room. Listing a host there is whoever runs this machine saying the
 *  network to it is theirs (a cluster's pod network) and that whatever can read that network may read the credential: a host
 *  name proves nothing about where it resolves, so no spelling is trusted by default. Anything else — a private
 *  IP or a cluster service name included — needs TLS. */
export function plaintextAllowed(hostname, env = process.env) {
  const host = String(hostname || '').toLowerCase().replace(/\.$/, '');
  if (!host) return false;
  if (['127.0.0.1', 'localhost', '::1', '[::1]'].includes(host)) return true;
  return trustedClusterHosts(env).some(entry => entry.startsWith('.') ? host.endsWith(entry) : host === entry);
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
    const saved = readTrustedJson(path.join(dataDir(env), 'credentials.json'));
    if (!saved.url || !saved.connector_id || !saved.token) return null;
    return { origin: roomOrigin(saved.url), connector_id: saved.connector_id };
  } catch { return null; }
}

/** The room's address, if this machine may send it a credential or a code: https, or in clear only to loopback
 *  and the trusted cluster hosts. */
function roomBase(room, env) {
  const base = new URL(room);
  if (base.protocol !== 'https:' && !(base.protocol === 'http:' && plaintextAllowed(base.hostname, env))) {
    throw new Error(`${base.origin} would carry this machine's credential in clear; the room must be https:// (plain http only to loopback or a host in SIDEVOICE_TRUSTED_CLUSTER_HOSTS).`);
  }
  return base;
}

/** Redeem a code for this host's credential. Returns where it was written. */
export async function pair(room, code, env = process.env) {
  const base = roomBase(room, env);
  // The code, and who this machine is: the room lists what it is told, so it is told here too and
  // not only on every connection — a machine paired and never yet connected still reads as a machine.
  const response = await fetch(new URL('/api/connectors/pair', base), {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ code, ...machineIdentity(env) }), signal: AbortSignal.timeout(15_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error('Pairing failed: ' + (body.detail || response.status));
  const file = path.join(dataDir(env), 'credentials.json');
  // Where the room is, not how to reach it: the path and namespace that carry the link belong to
  // the client and move with its version, so an upgrade never has to rewrite what pairing wrote.
  // The dial key is what the room shows this machine's core if the room is ever the one to open the link.
  // Through a fresh private file renamed into place: an existing file's permissions or a link there never decide
  // who can read this machine's credential (`secure-fs.mjs`).
  writePrivateFile(file, JSON.stringify({ url: base.origin, connector_id: body.connector_id, token: body.token, protocol: body.protocol,
    ...(body.dial_key ? { dial_key: body.dial_key } : {}) }, null, 2));
  return { file, connector_id: body.connector_id, origin: base.origin };
}

/** Link this machine with a room with no page in between: ask it for a code as its own page does — naming
 *  the room as the origin, which is all the room asks of a caller — and redeem it at once. */
export async function linkRoom(room, env = process.env) {
  const base = roomBase(room, env);
  const response = await fetch(new URL('/api/connectors/pairing-code', base), {
    method: 'POST', headers: { origin: base.origin }, signal: AbortSignal.timeout(15_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || typeof body.code !== 'string' || !body.code) throw new Error(`${base.origin} gave no pairing code: ${body.detail || response.status}`);
  return pair(base.origin, body.code, env);
}

/** The core starts with this machine's pairing (`--room-credential`): once a new one is written, the connector
 *  running now — the node service's supervisor, or a plain one — restarts it with it (`node.restart`). None
 *  running: the next core starts with it anyway. Nothing is started here. */
async function restartCore(env) {
  const answered = await askConnector('node.restart', {}, { env, timeout: 90_000 });
  return answered && !answered.error ? answered.state : null;
}

/** `sidevoice pair <room-url> <code> [--json]`. */
export async function runPair(argv = [], env = process.env) {
  const json = argv.includes('--json');
  const [room, code] = argv.filter(item => !item.startsWith('--'));
  if (!room || !code) {
    if (json) { console.log(JSON.stringify({ ok: false, error: { key: 'pair.usage', message: t('pair.usage') } })); return 1; }
    console.error(t('pair.usage')); return 2;
  }
  try {
    const result = await pair(room, code, env);
    const core = await restartCore(env);
    if (json) console.log(JSON.stringify({ ok: true, room: result.origin, connector_id: result.connector_id }));
    else {
      console.log(`Paired with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
      if (core) console.log(t('pair.core-restarted'));
    }
    return 0;
  } catch (error) {
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key || 'pair.failed', message: error.message } }));
    else console.error(error.message);
    return 1;
  }
}

/** `sidevoice link-room <room-url>`. */
export async function runLinkRoom(argv = [], env = process.env) {
  const [room] = argv;
  if (!room) { console.error('usage: sidevoice link-room <room-url>   (links this machine with that room; no code needed)'); return 2; }
  try {
    const result = await linkRoom(room, env);
    await restartCore(env);
    console.log(`Linked with ${result.origin} as connector ${result.connector_id}; credential saved to ${result.file}`);
    return 0;
  } catch (error) { console.error(error.message); return 1; }
}
