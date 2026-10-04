/** A façade's side of this machine's connector: one local connection, the connector got through the launcher when
 *  there is none, and each request answered by its id. The MCP server holds one for as long as its conversation
 *  lives; a command (`sidevoice pair-device`) holds one for a single request. When the connection goes (the connector
 *  restarted, upgraded, stopped), `onLost` is called, and requests it could not answer fail with `gone` set, so the
 *  caller knows which ones may be asked again. */
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import { launch } from './launcher.mjs';
import { connectorSocketOf } from './node-files.mjs';
import { verifyConnectorSocket } from './secure-fs.mjs';
import { runningAsSea } from './sea-runtime.mjs';
import { selectedDaemonCommand, selection } from './release.mjs';
import { RUST_CONNECTOR_KIND } from './rust-connector.mjs';

// The connector is started through the same entry the façade came in by — `cli.mjs connector` —
// because published there is one bundled file and no `connector.mjs` beside it to point at.
const cliPath = fileURLToPath(new URL('./cli.mjs', import.meta.url));

export function connectorSocket(env = process.env) {
  return connectorSocketOf(env);
}

const gone = message => Object.assign(new Error(message), { gone: true });

/** `self`: the argv prefix that runs the CLI a plain connector is started from — this package's own by default. */
export function connectorClient(env = process.env, { onLost = () => {}, self = null } = {}) {
  if (!self) self = selection(env, 'current')?.release?.runtime_kind === RUST_CONNECTOR_KIND
    ? selectedDaemonCommand(env) : runningAsSea() ? [process.execPath] : [process.execPath, cliPath];
  const socketPath = connectorSocket(env);
  let socket = null, buffer = '', serial = 0;
  const waiting = new Map();

  function connect() {
    return new Promise((resolve, reject) => {
      // Only a socket this user owns, in a data directory nobody else can write into (`secure-fs.mjs`).
      try { verifyConnectorSocket(socketPath); } catch (error) { if (error.key) { error.unsafe = true; return reject(error); } }
      const attempt = net.createConnection(socketPath);
      attempt.once('error', reject);
      attempt.on('connect', () => {
        attempt.removeListener('error', reject);
        attempt.on('error', () => {});
        attempt.on('close', () => {
          if (socket === attempt) socket = null;
          for (const w of waiting.values()) w.reject(gone('Connector went away'));
          waiting.clear();
          onLost();
        });
        attempt.on('data', chunk => {
          buffer += chunk; let index;
          while ((index = buffer.indexOf('\n')) >= 0) {
            const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
            let reply; try { reply = JSON.parse(line); } catch { continue; }
            const pending = waiting.get(reply.id); if (!pending) continue; waiting.delete(reply.id);
            if (reply.ok) pending.resolve(reply.result);
            else pending.reject(new Error(reply.error));
          }
        });
        buffer = '';
        socket = attempt; resolve(attempt);
      });
    });
  }

  async function ensure() {
    if (socket) return socket;
    return launch({ connect, self, env });
  }

  async function rpc(method, params) {
    await ensure();
    const id = ++serial;
    return new Promise((resolve, reject) => { waiting.set(id, { resolve, reject }); socket.write(JSON.stringify({ id, method, params }) + '\n'); });
  }

  return {
    rpc, ensure,
    /** Whether a connection is open now: a façade that never needed the connector has none. */
    get connected() { return !!socket; },
    end() { const open = socket; socket = null; open?.end(); },
  };
}
