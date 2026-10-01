/** This machine's core over its Unix socket, `~/.sidevoice/core/local.sock`: the only place the connector
 *  link (the credential that can mint device pairing codes) and the local routes are served.
 *
 *  Who may open it is decided by the file system: the core's directory is this user's and nobody else's
 *  (0700), so another OS user cannot reach the socket inside it. The connector creates that directory
 *  before any core starts and checks it before every dial — a directory someone else owns, or one others
 *  can enter, is refused with `identity.unsafe-directory`, and so is a socket that is not this user's.
 *
 *  Node has no `SO_PEERCRED`/`getpeereid`, so the connector cannot ask the kernel who is listening. The
 *  directory check stands in for it: only this uid can create or replace an entry in a 0700 directory it
 *  owns, so a socket found there, owned by this uid, was put there by this uid. The desktop app, which can,
 *  checks the peer uid as well. */
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import { lstatSync, mkdirSync } from 'node:fs';
import { keyed } from './i18n.mjs';

const uid = () => (typeof process.getuid === 'function' ? process.getuid() : null);

/** The core's directory, created 0700 if it is not there, and refused unless it is this user's alone. */
export function ensureCoreDirectory(directory) {
  mkdirSync(path.dirname(directory), { recursive: true, mode: 0o700 });
  try { mkdirSync(directory, { mode: 0o700 }); } catch (error) { if (error.code !== 'EEXIST') throw error; }
  return verifyCoreDirectory(directory);
}

/** Owner = this uid, a real directory (not a link to one), and nothing for group or others. */
export function verifyCoreDirectory(directory) {
  let stat;
  try { stat = lstatSync(directory); } catch (error) { throw keyed('identity.unsafe-directory', { path: directory, why: error.code || error.message }); }
  const why = !stat.isDirectory() ? 'not a directory'
    : uid() !== null && stat.uid !== uid() ? `owned by uid ${stat.uid}, not ${uid()}`
    : stat.mode & 0o077 ? `mode ${(stat.mode & 0o777).toString(8)}, not 700`
    : null;
  if (why) throw keyed('identity.unsafe-directory', { path: directory, why });
  return directory;
}

/** The socket in its directory: the directory as above, and the entry a socket this uid owns. */
export function verifySocket(socketPath) {
  verifyCoreDirectory(path.dirname(socketPath));
  let stat;
  try { stat = lstatSync(socketPath); } catch (error) { throw keyed('core.socket-missing', { path: socketPath, why: error.code || error.message }); }
  if (!stat.isSocket() || (uid() !== null && stat.uid !== uid())) throw keyed('peer.uid-mismatch', { path: socketPath });
  return socketPath;
}

/** An `http.Agent` that dials the socket whatever the URL says: what socket.io-client is handed so that its
 *  WebSocket goes to the core's socket and not to TCP. Checked again at every dial — a reconnect after the
 *  core restarted finds a new socket, and that one is checked too. */
export function socketAgent(socketPath) {
  const agent = new http.Agent({ keepAlive: false });
  agent.createConnection = (options, callback) => {
    try { verifySocket(socketPath); } catch (error) {
      // A refusal travels as the connection's error, the way a refused dial would.
      const refused = new net.Socket();
      process.nextTick(() => refused.destroy(error));
      return refused;
    }
    return net.createConnection({ path: socketPath }, callback);
  };
  return agent;
}

/** `GET /api/local/health` on the socket: `{status, body}`, or null when nothing answered within `timeout`.
 *  The readiness probe: a core that answers here is serving, not merely alive. */
export function localHealth(socketPath, timeout = 2000) {
  try { verifySocket(socketPath); } catch { return Promise.resolve(null); }
  return new Promise(resolve => {
    const request = http.get({ socketPath, path: '/api/local/health', headers: { host: 'localhost' }, timeout }, response => {
      let body = '';
      response.setEncoding('utf8');
      response.on('data', chunk => { body += chunk; if (body.length > 1 << 16) request.destroy(); });
      response.on('end', () => { let parsed = null; try { parsed = JSON.parse(body); } catch {} resolve({ status: response.statusCode, body: parsed }); });
      response.on('error', () => resolve(null));
    });
    request.on('timeout', () => request.destroy(new Error('timeout')));
    request.on('error', () => resolve(null));
  });
}
