/** A lock held by a file, taken atomically and given up only by its owner — the connector's singleton lock
 *  (`connector.sock.lock`) and the install lock (`install.lock`).
 *
 *  Taking it: the owner's record — pid, the process's start time, a nonce — is written whole to a private
 *  temporary file and `link()`ed into place. The link either creates the lock with that complete content or fails
 *  (EEXIST): there is never a lock without its owner in it, so a reader can never see a half-made one and take it
 *  for abandoned.
 *
 *  Taking it from someone: only when its owner is proven gone — no such process, or another process under that pid
 *  (its start time differs). An owner that cannot be told (unreadable lock, unknown process) is held to be alive.
 *  The lock judged stale is renamed aside and the renamed file compared with what was judged: if a newer owner
 *  replaced it in between, theirs is put back and nothing is taken.
 *
 *  Giving it up: only if the lock is still this owner's (same nonce). */
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { linkSync, renameSync, unlinkSync } from 'node:fs';
import { processState, selfIdentity } from './proc.mjs';
import { readTrusted, writePrivateFile } from './secure-fs.mjs';
import { pauseSync } from './testpoint.mjs';

/** The lock's record, or `null` when there is none, or `{unreadable: true}`. */
export function readLock(file) {
  let text;
  try { text = readTrusted(file, { checkDir: false }); } catch { return { unreadable: true }; }
  if (text === null) return null;
  try { const record = JSON.parse(text); return Number.isInteger(record?.pid) && record.nonce ? record : { unreadable: true }; } catch { return { unreadable: true }; }
}

/** Whether a lock's owner is proven gone. */
export function lockStale(record) {
  if (!record || record.unreadable) return false;
  return processState(record.pid, { start: record.start ?? null }) === 'gone';
}

/** Try once to take the lock: `{held: true, release, record}` or `{held: false, owner}` (the record found). */
export function tryLock(file, { kind = 'lock' } = {}) {
  const me = selfIdentity();
  const record = { pid: process.pid, start: me.start ?? null, kind, nonce: randomBytes(12).toString('hex'), at: new Date().toISOString() };
  const temporary = `${file}.${process.pid}.${record.nonce}.tmp`;
  writePrivateFile(temporary, JSON.stringify(record));
  let reclaimed = null;
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      pauseSync(`lock-publish-${kind}`);
      try { linkSync(temporary, file); pauseSync(`lock-held-${kind}`); return { held: true, record, reclaimed, release: () => releaseLock(file, record) }; }
      catch (error) { if (error.code !== 'EEXIST') throw error; }
      const owner = readLock(file);
      if (!owner) continue;                                  // let go meanwhile: try again
      if (owner.pid === process.pid && owner.nonce !== record.nonce && owner.start === record.start) return { held: false, owner, self: true };
      if (!lockStale(owner)) return { held: false, owner };
      pauseSync(`lock-reclaim-${kind}`);
      if (!reclaim(file, owner)) return { held: false, owner: readLock(file) || owner };
      reclaimed = owner;
    }
    return { held: false, owner: readLock(file) };
  } finally { try { unlinkSync(temporary); } catch {} }
}

/** Move the stale lock aside; keep it only if the moved file is the very one judged stale. */
function reclaim(file, judged) {
  const aside = `${file}.stale.${process.pid}.${randomBytes(4).toString('hex')}`;
  try { renameSync(file, aside); } catch (error) { return error.code === 'ENOENT'; }
  const moved = readLock(aside);
  if (moved && !moved.unreadable && moved.nonce === judged.nonce) { try { unlinkSync(aside); } catch {} return true; }
  // Somebody took it in between: theirs goes back (a link, so a lock taken since is not overwritten).
  try { linkSync(aside, file); } catch {}
  try { unlinkSync(aside); } catch {}
  return false;
}

/** Give the lock up if it is still ours. */
export function releaseLock(file, record) {
  const current = readLock(file);
  if (current && !current.unreadable && current.nonce === record.nonce) { try { unlinkSync(file); } catch {} return true; }
  return false;
}

/** Whether `record` is still the lock's owner (a holder checks before acting as one). */
export function stillHeld(file, record) {
  const current = readLock(file);
  return !!current && current.nonce === record.nonce;
}

export const lockDir = file => path.dirname(file);
