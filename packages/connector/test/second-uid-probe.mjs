#!/usr/bin/env node
/** Run as a second OS user against the first user's running node (CI, `ci.yml` job `client`): it must not reach the
 *  core's directory, its socket — where the connector link and the local routes are — nor the connector's socket;
 *  and the core's TCP listener must not offer the connector link. Node's own modules only: this file is copied
 *  where that other user can read it, without the repository. Exit 0 when every refusal holds. */
import net from 'node:net';
import { lstatSync, readdirSync } from 'node:fs';
import path from 'node:path';

const [dataDir, tcpUrl] = process.argv.slice(2);
const results = [];
const refused = async (what, attempt) => {
  try { await attempt(); results.push([what, false, 'reached']); }
  catch (error) { results.push([what, true, error.code || error.message]); }
};
const connect = file => new Promise((resolve, reject) => {
  const socket = net.createConnection(file);
  socket.once('connect', () => { socket.destroy(); resolve(); });
  socket.once('error', reject);
});

await refused('list the core directory', () => readdirSync(path.join(dataDir, 'core')));
await refused('stat the core socket', () => lstatSync(path.join(dataDir, 'core', 'local.sock')));
await refused('connect to the core socket', () => connect(path.join(dataDir, 'core', 'local.sock')));
await refused('connect to the connector socket', () => connect(path.join(dataDir, 'connector.sock')));
await refused('the connector link over TCP', async () => {
  const response = await fetch(new URL('/api/connectors/link/?EIO=4&transport=polling', tcpUrl));
  if (response.status !== 200) throw Object.assign(new Error(`HTTP ${response.status}`), { code: `HTTP ${response.status}` });
});
await refused('the local health over TCP', async () => {
  const response = await fetch(new URL('/api/local/health', tcpUrl));
  if (response.status !== 200) throw Object.assign(new Error(`HTTP ${response.status}`), { code: `HTTP ${response.status}` });
});
for (const [what, ok, how] of results) console.log(`${ok ? 'refused' : 'REACHED'}  ${what} (${how})`);
process.exitCode = results.every(([, ok]) => ok) ? 0 : 1;
