/** Pause points for tests that need an exact interleaving — a lock half taken, an installer between two side
 *  effects — instead of a race won by timing. Inert unless `SIDEVOICE_TEST_HOOKS` names a directory: then
 *  `pause(name)` writes `<dir>/paused-<name>` (with this pid) and waits until `<dir>/resume-<name>` exists, and
 *  `crash(name)` kills this process when `<dir>/crash-<name>` exists. Names are listed where they are used. */
import { existsSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const dir = () => process.env.SIDEVOICE_TEST_HOOKS || null;
const sleeper = new Int32Array(new SharedArrayBuffer(4));

function armed(name) {
  const root = dir();
  if (!root || !existsSync(path.join(root, `pause-${name}`))) return null;
  writeFileSync(path.join(root, `paused-${name}`), String(process.pid));
  writeFileSync(path.join(root, `paused-${name}-${process.pid}`), '');   // every process held here, one by one
  return path.join(root, `resume-${name}`);
}

/** Wait here, without blocking the event loop, while a test holds this point. */
export async function pause(name) {
  const resume = armed(name);
  if (!resume) return;
  while (!existsSync(resume)) await new Promise(r => setTimeout(r, 20));
}

/** The same for synchronous code (a lock being taken): the whole process waits. */
export function pauseSync(name) {
  const resume = armed(name);
  if (!resume) return;
  while (!existsSync(resume)) Atomics.wait(sleeper, 0, 0, 20);
}

/** Die here (SIGKILL: no cleanup runs), when a test says so. */
export function crash(name) {
  const root = dir();
  if (root && existsSync(path.join(root, `crash-${name}`))) process.kill(process.pid, 'SIGKILL');
}
