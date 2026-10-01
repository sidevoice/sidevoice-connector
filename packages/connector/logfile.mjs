/** The two logs a person may be asked for — `core.log` (the core's own output) and `node-service.log` (the
 *  supervisor's, and whatever the service manager catches from it) — each kept under 5 MB, with the two
 *  previous ones beside it (`.1`, `.2`).
 *
 *  Rotated by copy-and-truncate, never by rename: the core and the service manager hold these files open
 *  (`O_APPEND`) for as long as they run, and a renamed file would keep receiving their output under its old
 *  name. Truncating in place moves their next write to the start of the same file. */
import { appendFileSync, copyFileSync, renameSync, statSync, truncateSync } from 'node:fs';

export const LOG_MAX = Number(process.env.SIDEVOICE_LOG_MAX_BYTES || 5 * 1024 * 1024);
export const LOG_KEPT = 2;

/** Rotate `file` if it has grown past the limit. Cheap enough to call before every write. */
export function rotate(file, max = LOG_MAX) {
  let size = 0; try { size = statSync(file).size; } catch { return false; }
  if (size <= max) return false;
  try {
    for (let index = LOG_KEPT - 1; index >= 1; index--) { try { renameSync(`${file}.${index}`, `${file}.${index + 1}`); } catch {} }
    copyFileSync(file, `${file}.1`);
    truncateSync(file, 0);
    return true;
  } catch { return false; }
}

/** Output appended as it comes (a child's stream this process owns), the log rotated first. */
export function appendChunk(file, chunk) {
  try { rotate(file); appendFileSync(file, chunk, { mode: 0o600 }); } catch {}
}

/** One line appended to a rotated log, 0600. */
export function appendLine(file, line) {
  try { rotate(file); appendFileSync(file, line.endsWith('\n') ? line : line + '\n', { mode: 0o600 }); } catch {}
}
