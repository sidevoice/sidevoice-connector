/** The file-system half of the trust boundary (§1: the OS user). Node has no peer credentials on a Unix socket,
 *  so what keeps another OS user out is that every directory on the way to our files is this user's, or root's
 *  and not open to others for replacing entries — and our own directories and records are not writable by
 *  anyone else. A data directory or record that fails this is refused (`identity.unsafe-directory`,
 *  `identity.unsafe-file`), never repaired behind the person's back and never used.
 *
 *  Records are opened without following links and judged on the descriptor that was opened (`fstat`), so the
 *  file judged is the file read. Records are written through a private temporary file (0600, exclusive, no
 *  link followed) renamed into place. */
import path from 'node:path';
import { closeSync, constants, fchmodSync, fstatSync, lstatSync, mkdirSync, openSync, readFileSync, renameSync, unlinkSync, writeSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { keyed } from './i18n.mjs';

const uid = () => (typeof process.getuid === 'function' ? process.getuid() : null);
const NOFOLLOW = constants.O_NOFOLLOW ?? 0;

/** Every ancestor of `dir`: this user's or root's, and if others can write into it, sticky (as /tmp), so nobody
 *  else can replace what is in it. */
export function verifyAncestors(dir) {
  let current = path.dirname(path.resolve(dir));
  for (;;) {
    const stat = lstatSync(current);
    const owner = stat.uid === uid() || stat.uid === 0;
    const replaceable = (stat.mode & 0o022) && !(stat.mode & 0o1000);
    if (uid() !== null && (!owner || replaceable)) throw keyed('identity.unsafe-directory', { path: current, why: !owner ? `owned by uid ${stat.uid}` : 'others can replace what is in it' });
    const parent = path.dirname(current);
    if (parent === current) return;
    current = parent;
  }
}

/** A directory of ours: a real directory (not a link), this user's, nobody else may write in it, and its
 *  ancestors safe. `create` makes it 0700 when it is missing. `strict` also refuses any access for others
 *  (the core's directory: 0700 exactly, as `core-socket.mjs` checks). */
export function verifyPrivateDir(dir, { create = false, strict = false } = {}) {
  if (create) { mkdirSync(path.dirname(dir), { recursive: true, mode: 0o700 }); try { mkdirSync(dir, { mode: 0o700 }); } catch (error) { if (error.code !== 'EEXIST') throw error; } }
  let stat;
  try { stat = lstatSync(dir); } catch (error) { throw keyed('identity.unsafe-directory', { path: dir, why: error.code || error.message }); }
  const why = !stat.isDirectory() ? 'not a directory (or a link to one)'
    : uid() !== null && stat.uid !== uid() ? `owned by uid ${stat.uid}, not ${uid()}`
    : stat.mode & (strict ? 0o077 : 0o022) ? `mode ${(stat.mode & 0o777).toString(8)}`
    : null;
  if (why) throw keyed('identity.unsafe-directory', { path: dir, why });
  verifyAncestors(dir);
  return dir;
}

/** The contents of a record we trust: a regular file, not a link, this user's, writable by nobody else, in a
 *  directory that passes `verifyPrivateDir`. Null when it does not exist; refused otherwise. */
export function readTrusted(file, { checkDir = true } = {}) {
  if (checkDir) {
    try { lstatSync(path.dirname(file)); } catch (error) { if (error.code === 'ENOENT') return null; throw error; }
    verifyPrivateDir(path.dirname(file));
  }
  let fd;
  try { fd = openSync(file, constants.O_RDONLY | NOFOLLOW); }
  catch (error) {
    if (error.code === 'ENOENT') return null;
    if (error.code === 'ELOOP' || error.code === 'EMLINK') throw keyed('identity.unsafe-file', { path: file, why: 'a symbolic link' });
    throw error;
  }
  try {
    const stat = fstatSync(fd);
    const why = !stat.isFile() ? 'not a regular file' : uid() !== null && stat.uid !== uid() ? `owned by uid ${stat.uid}` : stat.mode & 0o022 ? `mode ${(stat.mode & 0o777).toString(8)}` : null;
    if (why) throw keyed('identity.unsafe-file', { path: file, why });
    return readFileSync(fd, 'utf8');
  } finally { closeSync(fd); }
}

/** A trusted JSON record, or null when it does not exist or does not parse. */
export function readTrustedJson(file, options) {
  const text = readTrusted(file, options);
  if (text === null) return null;
  try { return JSON.parse(text); } catch { return null; }
}

/** The connector's socket before a client talks to it: in a safe directory, a socket, this user's, open to
 *  nobody else. Absent is not unsafe (the launcher starts one); anything else wrong is refused. */
export function verifyConnectorSocket(socketPath) {
  verifyPrivateDir(path.dirname(socketPath));
  let stat;
  try { stat = lstatSync(socketPath); } catch (error) { if (error.code === 'ENOENT') return false; throw error; }
  if (!stat.isSocket() || (uid() !== null && stat.uid !== uid()) || (stat.mode & 0o077)) throw keyed('peer.uid-mismatch', { path: socketPath });
  return true;
}

/** Write a record whole, 0600: a fresh temporary file (exclusive, no link followed) renamed over the old one. */
export function writePrivateFile(file, text) {
  verifyPrivateDir(path.dirname(file), { create: true });
  const temporary = `${file}.${process.pid}.${randomBytes(4).toString('hex')}.tmp`;
  const fd = openSync(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | NOFOLLOW, 0o600);
  try { fchmodSync(fd, 0o600); writeSync(fd, text); } catch (error) { closeSync(fd); try { unlinkSync(temporary); } catch {} throw error; }
  closeSync(fd);
  renameSync(temporary, file);
}
