/** The one way anything gets a connector (§4.2): the MCP façade, `pair-device`, `install`, and the app through
 *  `service start`. Before this, every façade that found no socket spawned a connector of its own, and the
 *  node service could lose its socket to one of them.
 *
 *  - A person stopped Sidevoice (`node-stopped.json`): nothing is started, and the caller is told how to start
 *    it again. Only `service start`, or the next login, clears that.
 *  - A node service is registered: the service manager is asked to start it and the socket waited for, up to
 *    10 s. Nothing is spawned — a connector beside the service's would take its socket. A manager that will not
 *    start it is `service.not-loaded`, with the remedy.
 *  - No service: a plain connector is spawned, detached, as before; two of them settle it by the lock. */
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { keyed } from './i18n.mjs';
import { dataDirOf, nodeFiles } from './node-files.mjs';
import { installedService, managerStart } from './service.mjs';

const SERVICE_WAIT_MS = Number(process.env.SIDEVOICE_SERVICE_START_WAIT_MS || 10_000);
const SPAWN_WAIT_MS = 4000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** Whether a person stopped the node service on this machine. */
export function stoppedByPerson(env = process.env) {
  return existsSync(nodeFiles(dataDirOf(env)).stopped);
}

/** A connection to this machine's connector, getting one started the only way allowed. `connect()` tries the
 *  socket once; `self` is the argv prefix that runs this package's CLI (a plain connector is `self + connector`). */
export async function launch({ connect, self, env = process.env }) {
  try { return await connect(); } catch (error) { if (error.unsafe) throw error; }
  if (stoppedByPerson(env)) throw keyed('node.stopped');
  const service = installedService(env);
  if (service) {
    const started = await managerStart(env, { load: false });
    if (!started.ok) throw keyed('service.not-loaded', { detail: started.detail || started.key });
    const deadline = Date.now() + SERVICE_WAIT_MS;
    while (Date.now() < deadline) {
      try { return await connect(); } catch {}
      await wait(100);
    }
    throw keyed('service.not-loaded', { detail: `no answer on the socket within ${SERVICE_WAIT_MS / 1000} s` });
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
