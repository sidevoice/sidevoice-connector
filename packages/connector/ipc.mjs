/** A façade's side of this machine's connector: one local connection, the connector started when there is
 *  none, and each request answered by its id. The MCP server holds one for as long as its conversation
 *  lives; a command (`sidevoice pair-device`) holds one for a single request. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// The connector is started through the same entry the façade came in by — `cli.mjs connector` —
// because published there is one bundled file and no `connector.mjs` beside it to point at.
const cliPath = fileURLToPath(new URL('./cli.mjs', import.meta.url));

export function connectorSocket(env = process.env) {
  const dataDir = env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
  return env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
}

export function connectorClient(env = process.env) {
  const socketPath = connectorSocket(env);
  let socket = null, buffer = '', serial = 0;
  const waiting = new Map();

  function connect() {
    return new Promise((resolve, reject) => {
      const attempt = net.createConnection(socketPath);
      attempt.once('error', reject);
      attempt.on('connect', () => {
        attempt.removeListener('error', reject);
        attempt.on('error', () => {});
        attempt.on('close', () => { if (socket === attempt) socket = null; for (const w of waiting.values()) w.reject(new Error('Connector went away')); waiting.clear(); });
        attempt.on('data', chunk => {
          buffer += chunk; let index;
          while ((index = buffer.indexOf('\n')) >= 0) {
            const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
            let reply; try { reply = JSON.parse(line); } catch { continue; }
            const pending = waiting.get(reply.id); if (!pending) continue; waiting.delete(reply.id);
            reply.ok ? pending.resolve(reply.result) : pending.reject(new Error(reply.error));
          }
        });
        socket = attempt; resolve(attempt);
      });
    });
  }

  async function ensure() {
    if (socket) return socket;
    try { return await connect(); } catch {}
    // Not the flag that made this process a command (`SIDEVOICE_PAIR_DEVICE_MAIN`…): the connector is not one.
    const childEnv = Object.fromEntries(Object.entries(env).filter(([name]) => !/^SIDEVOICE_\w+_MAIN$/.test(name)));
    const child = spawn(process.execPath, [cliPath, 'connector'], { detached: true, stdio: 'ignore', env: childEnv });
    child.unref();
    for (let attempt = 0; attempt < 40; attempt++) {
      await new Promise(r => setTimeout(r, 100));
      try { return await connect(); } catch {}
    }
    throw new Error('The Sidevoice connector did not start (is this host paired?)');
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
