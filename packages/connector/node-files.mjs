/** The connector's files in its data dir, `D` (`$SIDEVOICE_DATA_DIR` or `~/.sidevoice`), by name, in one place
 *  (SEAMS §1). The connector is their one writer — except `core/`, which is the core's — and each is written
 *  whole, 0600, through a temporary file and a rename, so a reader never sees half of one. */
import os from 'node:os';
import path from 'node:path';
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';

export function dataDirOf(env = process.env) {
  return env.SIDEVOICE_DATA_DIR || path.join(env.HOME || os.homedir(), '.sidevoice');
}

/** Where façades reach the connector: `D/connector.sock` (a test may name another). */
export function connectorSocketOf(env = process.env) {
  return env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDirOf(env), 'connector.sock');
}

export function nodeFiles(dataDir) {
  const at = name => path.join(dataDir, name);
  return {
    install: at('install.json'),           // the selected installation (§4.3)
    journal: at('install-txn.json'),       // an install transaction under way: {from, to}
    status: at('node-status.json'),        // the supervisor's last snapshot, with its budget window
    stopped: at('node-stopped.json'),      // a person stopped the node service: {at}
    restart: at('node-restart.json'),      // a person asked for a restart the service manager carries out: {at}
    handover: at('handover.json'),         // bindings moving from a plain connector to the supervisor
    serviceLog: at('node-service.log'),
    connectorLog: at('connector.log'),
  };
}

export function readJson(file) {
  try { return JSON.parse(readFileSync(file, 'utf8')); } catch { return null; }
}

/** Write it whole or not at all, readable by this user only. */
export function writePrivate(file, value) {
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const temporary = `${file}.${process.pid}.tmp`;
  writeFileSync(temporary, typeof value === 'string' ? value : JSON.stringify(value, null, 2) + '\n', { mode: 0o600 });
  renameSync(temporary, file);
}
