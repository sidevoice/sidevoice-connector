/** A lock the kernel holds and gives up the moment its holder dies, however it dies: plain `flock(2)` on a lock
 *  file, exclusive and non-blocking. Nothing is ever stale, so nothing is ever reclaimed, and there is no protocol in
 *  which one process judges another gone and takes its place. Two locks use it (SEAMS §1): `D/connector.lock` — one
 *  connector serves the socket — and `D/install.lock` — every command that changes the installation, one at a time.
 *  Lock files are permanent inodes: created once, never replaced, never deleted (a holder of a deleted inode and a
 *  newcomer on its replacement would both hold "the" lock).
 *
 *  Node has no `flock`, so:
 *  - macOS: the file opened with `O_EXLOCK | O_NONBLOCK` — an exclusive flock taken in the same call; `EAGAIN` /
 *    `EWOULDBLOCK` means another process holds it.
 *  - Linux: the file opened here, and util-linux `flock -n 3` run on that very descriptor (handed to it as fd 3).
 *    A flock belongs to the open file description, which this process and the child share: it outlives the child,
 *    and goes when this process closes the descriptor or dies. `flock` missing is `lock.unavailable`.
 *  Either way the descriptor is close-on-exec (libuv opens every file so): no other child inherits the hold.
 *
 *  The holder writes who it is — pid, start time, kind — into the locked file itself, for people and for teardown,
 *  which signals that process only as its verified self (`proc.mjs`). That record is information, never the lock. */
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { closeSync, constants, ftruncateSync, mkdirSync, openSync, writeSync } from 'node:fs';
import { selfIdentity } from './proc.mjs';
import { keyed } from './i18n.mjs';
import { readTrusted } from './secure-fs.mjs';
import { pause } from './testpoint.mjs';

const DARWIN_O_EXLOCK = 0x20, DARWIN_O_NONBLOCK = 0x4;
const FLOCK = ['flock', '/usr/bin/flock', '/bin/flock'];
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** The holder's record, or `null` (none, or not one): information only. */
export function readLock(file) {
  let text;
  try { text = readTrusted(file, { checkDir: false }); } catch { return null; }
  try { const record = JSON.parse(text || 'null'); return Number.isInteger(record?.pid) ? record : null; } catch { return null; }
}

/** `flock -n` on `fd`: true when taken, false when another holds it. */
function flockLinux(fd, env) {
  for (const bin of env.SIDEVOICE_FLOCK ? [env.SIDEVOICE_FLOCK] : FLOCK) {
    const ran = spawnSync(bin, ['-n', '3'], { stdio: ['ignore', 'ignore', 'pipe', fd], timeout: 10_000 });
    if (ran.error?.code === 'ENOENT') continue;
    if (ran.error) throw ran.error;
    if (ran.status === 0) return true;
    if (ran.status === 1) return false;
    throw keyed('lock.unavailable', { detail: `flock exited ${ran.status}: ${String(ran.stderr).trim()}` });
  }
  throw keyed('lock.unavailable', { detail: 'util-linux flock is not installed' });
}

/** Try once: `{held: true, release, record}` or `{held: false, owner}` (the record found, information only). */
export async function tryLock(file, { kind = 'lock', env = process.env } = {}) {
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  await pause(`lock-before-${kind}`);
  let fd;
  if (process.platform === 'darwin') {
    try { fd = openSync(file, constants.O_RDWR | constants.O_CREAT | constants.O_NOFOLLOW | DARWIN_O_EXLOCK | DARWIN_O_NONBLOCK, 0o600); }
    catch (error) { if (error.code === 'EAGAIN' || error.code === 'EWOULDBLOCK') return { held: false, owner: readLock(file) }; throw error; }
  } else if (process.platform === 'linux') {
    fd = openSync(file, constants.O_RDWR | constants.O_CREAT | constants.O_NOFOLLOW, 0o600);
    let taken;
    try { taken = flockLinux(fd, env); } catch (error) { closeSync(fd); throw error; }
    if (!taken) { closeSync(fd); return { held: false, owner: readLock(file) }; }
  } else throw keyed('lock.unavailable', { detail: process.platform });
  const record = { pid: process.pid, start: selfIdentity().start ?? null, kind, at: new Date().toISOString() };
  // Into the locked file itself: a replaced file would leave the lock on an inode nobody else opens.
  try { ftruncateSync(fd, 0); writeSync(fd, JSON.stringify(record), 0); } catch {}
  await pause(`lock-held-${kind}`);
  let released = false;
  return { held: true, record, release: () => { if (released) return; released = true; try { ftruncateSync(fd, 0); } catch {} try { closeSync(fd); } catch {} } };
}

/** The install lock (`D/install.lock`): taken by every command that changes the installation and held to its end.
 *  Asked for again by the process that holds it, it is the same hold. */
export function installLockPath(dataDir) { return path.join(dataDir, 'install.lock'); }
const holds = new Map();   // lock file -> {depth}: this process's own hold, taken again
/** The lock, waited for up to `timeout`; `{wait: false}` answers null at once when another process holds it. */
export async function takeLock(file, { kind = 'lock', env = process.env, timeout = 20 * 60_000, wait: waiting = true, log = () => {} } = {}) {
  const mine = holds.get(file);
  if (mine) { mine.depth++; return () => { if (--mine.depth === 0) { holds.delete(file); mine.release(); } }; }
  const deadline = Date.now() + timeout;
  let said = false;
  for (;;) {
    const taken = await tryLock(file, { kind, env });
    if (taken.held) {
      const hold = { depth: 1, release: taken.release };
      holds.set(file, hold);
      return () => { if (--hold.depth === 0) { holds.delete(file); taken.release(); } };
    }
    if (!waiting) return null;
    if (!said) { log(`another process (pid ${taken.owner?.pid ?? '?'}) holds ${path.basename(file)}; waiting for it`); said = true; }
    if (Date.now() > deadline) throw keyed('service.busy', { detail: `pid ${taken.owner?.pid ?? '?'} holds ${file}` });
    await wait(250);
  }
}
