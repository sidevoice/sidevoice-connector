/** A façade's side of this machine's connector: one local connection, the connector started through the
 *  launcher when there is none, and each request answered by its id. The MCP server holds one for as long as
 *  its conversation lives; a command (`sidevoice pair-device`) holds one for a single request.
 *
 *  A connector handing over to the node service says so on the connection (`{type: 'handover'}`) before it
 *  closes it: `onHandover` is then called once the connection is gone, and the façade re-registers with the
 *  supervisor. Requests the closing connection could not answer fail with `gone` set, so the caller knows which
 *  ones may be asked again. */
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import { launch } from './launcher.mjs';
import { connectorSocketOf } from './node-files.mjs';
import { verifyConnectorSocket } from './secure-fs.mjs';

// The connector is started through the same entry the façade came in by — `cli.mjs connector` —
// because published there is one bundled file and no `connector.mjs` beside it to point at.
const cliPath = fileURLToPath(new URL('./cli.mjs', import.meta.url));

export function connectorSocket(env = process.env) {
  return connectorSocketOf(env);
}

const gone = message => Object.assign(new Error(message), { gone: true });

/** `self`: the argv prefix that runs the CLI a plain connector is started from — this package's own by default,
 *  an installation's `command` when an installer verifies that installation. */
export function connectorClient(env = process.env, { onHandover = () => {}, onLost = () => {}, self = [process.execPath, cliPath] } = {}) {
  const socketPath = connectorSocket(env);
  let socket = null, buffer = '', serial = 0;
  const waiting = new Map();

  function connect() {
    return new Promise((resolve, reject) => {
      // Only a socket this user owns, in a data directory nobody else can write into (`secure-fs.mjs`).
      try { verifyConnectorSocket(socketPath); } catch (error) { if (error.key) { error.unsafe = true; return reject(error); } }
      const attempt = net.createConnection(socketPath);
      let handedOver = false;
      attempt.once('error', reject);
      attempt.on('connect', () => {
        attempt.removeListener('error', reject);
        attempt.on('error', () => {});
        attempt.on('close', () => {
          if (socket === attempt) socket = null;
          for (const w of waiting.values()) w.reject(gone(handedOver ? 'HANDOVER: the connector handed over to the node service' : 'Connector went away'));
          waiting.clear();
          if (handedOver) onHandover(); else onLost();
        });
        attempt.on('data', chunk => {
          buffer += chunk; let index;
          while ((index = buffer.indexOf('\n')) >= 0) {
            const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
            let reply; try { reply = JSON.parse(line); } catch { continue; }
            if (reply.type === 'handover') { handedOver = true; continue; }
            const pending = waiting.get(reply.id); if (!pending) continue; waiting.delete(reply.id);
            if (reply.ok) pending.resolve(reply.result);
            else pending.reject(String(reply.error).startsWith('HANDOVER:') ? gone(reply.error) : new Error(reply.error));
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
