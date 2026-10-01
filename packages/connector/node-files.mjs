/** The connector's files in its data dir, `D` (`$SIDEVOICE_DATA_DIR` or `~/.sidevoice`), by name, in one place
 *  (SEAMS §1). The connector is their one writer — except `core/`, which is the core's — and each is written
 *  whole, 0600, through a temporary file and a rename, so a reader never sees half of one. Each is read only if
 *  it is a trusted record (`secure-fs.mjs`): this user's, writable by nobody else, not a link, in a data
 *  directory nobody else can write into. An `install.json` someone else could have written names a program to
 *  run, and is refused. */
import os from 'node:os';
import path from 'node:path';
import { readTrustedJson, writePrivateFile } from './secure-fs.mjs';

export function dataDirOf(env = process.env) {
  return env.SIDEVOICE_DATA_DIR || path.join(env.HOME || os.homedir(), '.sidevoice');
}

/** Where façades reach the connector: `D/connector.sock` (a test may name another). */
export function connectorSocketOf(env = process.env) {
  return env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDirOf(env), 'connector.sock');
}

/** The connector's own lock, beside the socket it protects: `D/connector.lock`. */
export function connectorLockOf(env = process.env) {
  return path.join(path.dirname(connectorSocketOf(env)), 'connector.lock');
}

export function nodeFiles(dataDir) {
  const at = name => path.join(dataDir, name);
  return {
    install: at('install.json'),           // {command, releases, definitions}: what runs it (through `R/current`), and where it is
    stopped: at('node-stopped.json'),      // a person stopped Sidevoice: {at}
    connectorLog: at('connector.log'),
    coreStderr: at('core.stderr.log'),     // what launchd catches from the core job: a crash's output
  };
}

/** What `install.json` records (SEAMS §1): `{command, releases, definitions}` — the command everything runs, and the
 *  absolute paths the installation was made at (`R`, each job definition), so that whatever acts on it later finds it
 *  there whatever its own environment says. Null when nothing is installed; an untrusted record throws (keyed). */
export function recordedInstallation(env = process.env) {
  const record = readTrustedJson(nodeFiles(dataDirOf(env)).install);
  return record && typeof record === 'object' ? record : null;
}

/** A trusted record, parsed; null when it is absent or does not parse. An untrusted one throws (keyed). */
export function readJson(file) {
  return readTrustedJson(file);
}

/** Write it whole or not at all, readable by this user only. */
export function writePrivate(file, value) {
  writePrivateFile(file, typeof value === 'string' ? value : JSON.stringify(value, null, 2) + '\n');
}
