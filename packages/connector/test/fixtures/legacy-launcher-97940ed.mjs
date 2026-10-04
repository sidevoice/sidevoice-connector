/** Frozen launcher from stacked base 97940ed. Only relative import paths differ so this fixture can run beside
 *  current tests. It models an MCP facade that was already alive before the new installer began. */
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { keyed } from '../../i18n.mjs';
import { dataDirOf, nodeFiles } from '../../node-files.mjs';
import { installedService, status } from '../../service.mjs';

const SERVICE_WAIT_MS = () => Number(process.env.SIDEVOICE_SERVICE_START_WAIT_MS || 10_000);
const SPAWN_WAIT_MS = 4000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

export function stoppedByPerson(env = process.env) {
  return existsSync(nodeFiles(dataDirOf(env)).stopped);
}

export async function launch({ connect, self, env = process.env }) {
  try { return await connect(); } catch (error) { if (error.unsafe) throw error; }
  if (stoppedByPerson(env)) throw keyed('node.stopped');
  if (installedService(env)?.connector) {
    const deadline = Date.now() + SERVICE_WAIT_MS();
    while (Date.now() < deadline) {
      await wait(100);
      try { return await connect(); } catch (error) { if (error.unsafe) throw error; }
    }
    const now = await status(env, { connectorRunning: false });
    if (now.state === 'stopped-by-person') throw keyed('node.stopped');
    const reason = now.state === 'service-failed' ? now.failure?.key : null;
    throw keyed(reason ? `service.${reason}` : 'connector.service-down', { detail: reason ?? now.state, state: now.state }, { status: now });
  }
  const child = spawn(self[0], [...self.slice(1), 'connector'], { detached: true, stdio: 'ignore', env });
  child.on('error', () => {});
  child.unref();
  const deadline = Date.now() + SPAWN_WAIT_MS;
  while (Date.now() < deadline) {
    await wait(100);
    try { return await connect(); } catch {}
  }
  throw keyed('connector.not-started');
}
