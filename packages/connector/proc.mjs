/** Which process a pid is, before anything is done to it. A pid read from a file is only a hint: pids are reused,
 *  and a stale lock or ready file can name somebody else's work. Nothing is signalled on a pid alone.
 *
 *  A process is identified by its owner, its start time and its command line — on Linux from `/proc`, elsewhere
 *  from `ps`. When those cannot be read, the answer is "unknown", and unknown never authorises a signal. */
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const uid = () => (typeof process.getuid === 'function' ? process.getuid() : null);
export const validPid = pid => Number.isInteger(pid) && pid > 1;

/** `{uid, start, command}` of a live pid, `null` when there is no such process, `undefined` when it cannot be told. */
export function processIdentity(pid) {
  if (!validPid(pid)) return null;
  try { process.kill(pid, 0); } catch (error) { if (error.code === 'ESRCH') return null; if (error.code !== 'EPERM') return undefined; }
  if (process.platform === 'linux') {
    try {
      const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
      const start = stat.slice(stat.lastIndexOf(')') + 2).split(' ')[19];   // field 22: start time, in clock ticks since boot
      const owner = Number(readFileSync(`/proc/${pid}/status`, 'utf8').match(/^Uid:\s+(\d+)/m)[1]);
      const command = readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').filter(Boolean).join(' ');
      return { uid: owner, start, command };
    } catch (error) { return error.code === 'ENOENT' ? null : undefined; }
  }
  try {
    const line = execFileSync('ps', ['-o', 'uid=', '-o', 'lstart=', '-o', 'command=', '-p', String(pid)], { encoding: 'utf8', timeout: 3000 }).trim();
    const match = line.match(/^(\d+)\s+(\w{3}\s+\w{3}\s+\d+\s+[\d:]+\s+\d{4})\s+(.*)$/);
    return match ? { uid: Number(match[1]), start: match[2].replace(/\s+/g, ' '), command: match[3] } : undefined;
  } catch (error) { return error.status === 1 ? null : undefined; }
}

/** This process, as a lock records it. */
let self = null;
export function selfIdentity() {
  return (self ??= processIdentity(process.pid) || { uid: uid(), start: null, command: process.argv.join(' ') });
}

/** Whether `pid` is still the process that was recorded as `{start}` and is this user's, and its command matches
 *  `command` (a RegExp) when given. False when it is gone, someone else's, another process now, or unknown. */
export function isProcess(pid, { start = null, command = null } = {}) {
  const found = processIdentity(pid);
  if (!found) return false;
  if (uid() !== null && found.uid !== uid()) return false;
  if (start !== null && found.start !== start) return false;
  if (command && !command.test(found.command)) return false;
  return true;
}

/** `gone` (no such process, or another one now under that pid), `alive` (the recorded one), or `unknown`. */
export function processState(pid, { start = null } = {}) {
  const found = processIdentity(pid);
  if (found === null) return 'gone';
  if (found === undefined || start === null) return 'unknown';
  return found.start === start ? 'alive' : 'gone';
}

/** Signal a pid only if it is still the process described; returns whether it was signalled. */
export function signalVerified(pid, signal, expected) {
  if (!isProcess(pid, expected)) return false;
  try { process.kill(pid, signal); return true; } catch { return false; }
}
