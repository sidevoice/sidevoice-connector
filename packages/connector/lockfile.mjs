/** A lock the kernel holds — the connector's singleton lock and the install lock — and gives up the moment its
 *  holder dies, however it dies. Nothing is ever stale, so nothing is ever reclaimed: there is no protocol in
 *  which one process judges another gone and takes its place.
 *
 *  - macOS: the lock file opened with `O_EXLOCK | O_NONBLOCK` — an exclusive flock(2) taken in the same call;
 *    `EAGAIN`/`EWOULDBLOCK` means another process holds it. The descriptor stays open for as long as this process
 *    holds the lock, and is close-on-exec (libuv opens every file so): no child inherits it.
 *  - Linux: a listening Unix socket in the abstract namespace — `EADDRINUSE` means held. It exists only while a
 *    process holds it open, and is close-on-exec too. Abstract names have no permissions, so they must not be
 *    guessable by another user, who could otherwise take the name first and keep this one from starting: each is
 *    derived from a 128-bit random salt kept in the lock's directory (`lock-salt`, 0600, made once and atomically —
 *    every process of this user reads the same one; the directory is private, so no other user can).
 *
 *  The lock file itself (`<lock>`) also carries who holds it — pid, process start time, kind — for people and for
 *  the takeover's handover request. That record is information only, never the lock: a record without a holder,
 *  or a holder whose record is not written yet, changes nothing. */
import net from 'node:net';
import path from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import { closeSync, constants, ftruncateSync, linkSync, mkdirSync, openSync, readdirSync, realpathSync, unlinkSync, writeSync } from 'node:fs';
import { isProcess, selfIdentity } from './proc.mjs';
import { keyed } from './i18n.mjs';
import { readTrusted, writePrivateFile } from './secure-fs.mjs';
import { pause } from './testpoint.mjs';

const DARWIN_O_EXLOCK = 0x20, DARWIN_O_NONBLOCK = 0x4;
const uid = () => (typeof process.getuid === 'function' ? process.getuid() : 0);
const safeList = directory => { try { return readdirSync(directory); } catch { return []; } };

/** The holder's record, or `null` when there is none, or `{unreadable: true}` (another format — the bare pid an
 *  older connector wrote, kept as `legacyPid` — or a half-written one): information only. */
export function readLock(file) {
  let text;
  try { text = readTrusted(file, { checkDir: false }); } catch { return { unreadable: true }; }
  if (text === null || text === '') return null;
  try {
    const record = JSON.parse(text);
    return Number.isInteger(record?.pid) ? record : { unreadable: true, legacyPid: Number.isInteger(record) ? record : null };
  } catch { return { unreadable: true }; }
}

/** The salt of a lock directory: read, or made — written whole to a private temporary file and linked into place,
 *  so two processes making it at once end up reading the same one. */
export function lockSalt(directory) {
  const file = path.join(directory, 'lock-salt');
  const read = () => { const text = readTrusted(file, { checkDir: false }); return text && /^[0-9a-f]{32}$/.test(text.trim()) ? text.trim() : null; };
  const existing = read();
  if (existing) return existing;
  // The salt is this directory's lock identity. Made again while a holder lives, it would give a second name — a
  // second lock beside the one held. Lost while held, it is an error until those holders are gone.
  for (const name of safeList(directory).filter(entry => entry.endsWith('.lock'))) {
    const owner = readLock(path.join(directory, name));
    if (owner && !owner.unreadable && isProcess(owner.pid, { start: owner.start ?? null })) {
      throw keyed('identity.lock-salt-missing', { path: file, pid: owner.pid, lock: name });
    }
  }
  const temporary = `${file}.${process.pid}.${randomBytes(4).toString('hex')}.tmp`;
  writePrivateFile(temporary, randomBytes(16).toString('hex'));
  try { linkSync(temporary, file); } catch (error) { if (error.code !== 'EEXIST') throw error; }
  finally { try { unlinkSync(temporary); } catch {} }
  const salt = read();
  if (!salt) throw new Error(`${file} is not a lock salt`);
  return salt;
}

/** The abstract socket name of a lock on Linux: this user, the directory's salt, this lock file's real directory and
 *  name, this kind — nothing another user can work out. */
export function abstractName(file, kind) {
  const directory = realpathSync(path.dirname(file));
  const digest = createHash('sha256').update(lockSalt(directory) + '\0' + directory + '\0' + path.basename(file)).digest('hex').slice(0, 32);
  return `\0sidevoice-${uid()}-${digest}-${kind}`;
}

/** Try once to take the lock: `{held: true, release, record}` or `{held: false, owner}` (the record found). */
export async function tryLock(file, { kind = 'lock' } = {}) {
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  await pause(`lock-publish-${kind}`);
  const record = { pid: process.pid, start: selfIdentity().start ?? null, kind, at: new Date().toISOString() };
  if (process.platform === 'darwin') {
    let fd;
    try { fd = openSync(file, constants.O_RDWR | constants.O_CREAT | (constants.O_NOFOLLOW ?? 0) | DARWIN_O_EXLOCK | DARWIN_O_NONBLOCK, 0o600); }
    catch (error) { if (error.code === 'EAGAIN' || error.code === 'EWOULDBLOCK') return { held: false, owner: readLock(file) }; throw error; }
    // The record goes into the locked file itself: replacing the file would leave the lock on an unlinked inode.
    try { ftruncateSync(fd, 0); writeSync(fd, JSON.stringify(record), 0); } catch {}
    await pause(`lock-held-${kind}`);
    let released = false;
    return { held: true, record, release: () => { if (released) return; released = true; try { ftruncateSync(fd, 0); } catch {} try { closeSync(fd); } catch {} } };
  }
  if (process.platform === 'linux') {
    const server = net.createServer(socket => socket.destroy());
    const outcome = await new Promise(resolve => {
      server.once('error', error => resolve(error.code === 'EADDRINUSE' ? 'held' : error));
      server.listen(abstractName(file, kind), () => resolve('ours'));
    });
    if (outcome === 'held') return { held: false, owner: readLock(file) };
    if (outcome !== 'ours') throw outcome;
    server.unref();   // the lock lives as long as the process holds it; it does not keep the process alive
    try { writePrivateFile(file, JSON.stringify(record)); } catch {}
    await pause(`lock-held-${kind}`);
    let released = false;
    // Let go: the record says nobody holds it (one a killed holder leaves names a dead process, and says so too).
    return { held: true, record, release: () => { if (released) return; released = true; try { writePrivateFile(file, ''); } catch {} try { server.close(); } catch {} } };
  }
  throw new Error(`No kernel-held lock on ${process.platform}`);
}
