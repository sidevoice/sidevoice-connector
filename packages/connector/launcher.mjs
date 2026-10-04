/** The one way anything gets a connector (§2.6): the MCP façade and `pair-device`. Clients of a managed connector
 *  never start it; only a machine without one does.
 *
 *  1. The socket answers: done.
 *  2. A person stopped Sidevoice (`node-stopped.json`): nothing is started, and the caller is told how to start it
 *     again. Only `service start`, or the next login, clears that.
 *  3. A connector job is defined: nothing is spawned — its manager runs it, and may be restarting it right now. The
 *     socket is waited for, up to 10 s; then the failure is what the node's status says (`service.mjs`).
 *  4. No job (no user manager: containers, `su` shells, Linux without a session bus; or nothing installed as a
 *     service): a plain connector is spawned, detached, as before; two of them settle it by its lock. */
import { spawn } from 'node:child_process';
import { keyed } from './i18n.mjs';
import { dataDirOf, nodeStopped, recordedInstallation, runtimeSwitching } from './node-files.mjs';
import { selectedDaemonCommand, selection } from './release.mjs';
import { installedService, status } from './service.mjs';

const SERVICE_WAIT_MS = () => Number(process.env.SIDEVOICE_SERVICE_START_WAIT_MS || 10_000);
const SPAWN_WAIT_MS = 4000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** Whether a person stopped Sidevoice on this machine. */
export function stoppedByPerson(env = process.env) {
  return nodeStopped(dataDirOf(env), selection(env, 'current')?.id);
}

/** A connection to this machine's connector, getting one started the only way allowed. `connect()` tries the
 *  socket once; `self` is the argv prefix that runs this package's CLI (a plain connector is `self + connector`). */
export async function launch({ connect, self, env = process.env }) {
  try { return await connect(); } catch (error) { if (error.unsafe) throw error; }
  if (runtimeSwitching(dataDirOf(env))) throw keyed('install.runtime-switching');
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
  // A façade can outlive the release that launched it. Always start the release selected now.
  const command = recordedInstallation(env) && selection(env, 'current') ? selectedDaemonCommand(env) : self;
  const child = spawn(command[0], [...command.slice(1), 'connector'], { detached: true, stdio: 'ignore', env });
  child.on('error', () => {});
  child.unref();
  const deadline = Date.now() + SPAWN_WAIT_MS;
  while (Date.now() < deadline) {
    await wait(100);
    try { return await connect(); } catch {}
  }
  throw keyed('connector.not-started');
}
