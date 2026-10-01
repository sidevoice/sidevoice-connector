#!/usr/bin/env node
/** The node service under this machine's real service manager — launchd (macOS) or a systemd user manager
 *  (Linux) — end to end, for CI (`.github/workflows/ci.yml`, jobs `service-macos` and `service-linux`). Not part of
 *  `npm test`: it installs a real LaunchAgent or user unit in this user's home and must run where that is wanted.
 *
 *  The core is the fake (`fake-sidevoice-core.mjs`); everything else is real. The flow (§7, R1-b acceptance):
 *  install with no agents → the core running with no harness → the core killed → backoff → running again → a
 *  login, approximated (below) → stop: the person's, and the launcher refuses → start → uninstall with a client
 *  still connected → no job, no process, no socket, and a login after it starts nothing.
 *
 *  A logout/login cannot be done on a CI runner. It is approximated by what it does to our job: launchd —
 *  `bootout` of the job and `bootstrap` of its plist from ~/Library/LaunchAgents, which is what loginwindow does to
 *  LaunchAgents at logout and login; systemd — a restart of the user's manager (`user@<uid>.service`), which stops
 *  every user unit and starts the enabled ones, as the end and start of the user's last session do. */
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(here, '..', 'cli.mjs');
const home = os.homedir();
const dataDir = path.join(home, '.sidevoice');
const uid = process.getuid();
const kind = process.platform === 'darwin' ? 'launchd' : 'systemd';
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const step = text => console.log(`\n=== ${text}`);
async function until(what, check, timeout = 60_000) {
  const start = Date.now();
  for (;;) { const value = await check(); if (value) return value; if (Date.now() - start > timeout) throw new Error(`timed out: ${what}`); await wait(250); }
}
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

// The fake core, through a wrapper the definition can name by absolute path.
const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-integration-'));
const coreBin = path.join(tools, 'sidevoice-core');
writeFileSync(coreBin, `#!/bin/sh\nexec "${process.execPath}" "${path.join(here, 'fake-sidevoice-core.mjs')}" "$@"\n`, { mode: 0o755 });
const env = { ...process.env, SIDEVOICE_CORE_BIN: coreBin, SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_FROM_SOURCE: '1' };
for (const name of ['SIDEVOICE_SERVICE_MANAGER', 'SIDEVOICE_DATA_DIR', 'SIDEVOICE_URL']) delete env[name];

function sidevoice(...args) {
  const run = spawnSync(process.execPath, [cli, ...args], { env, encoding: 'utf8', timeout: 180_000 });
  if (run.stderr.trim()) console.log(run.stderr.trim().split('\n').map(line => '  | ' + line).join('\n'));
  const last = run.stdout.trim().split('\n').at(-1);
  let json = null; try { json = JSON.parse(last); } catch {}
  console.log(`$ sidevoice ${args.join(' ')} → exit ${run.status}${json ? ' ' + JSON.stringify(json).slice(0, 400) : ''}`);
  return { code: run.status, json, stdout: run.stdout };
}
const status = () => sidevoice('service', 'status', '--json').json;
const jobLoaded = () => (kind === 'launchd'
  ? spawnSync('launchctl', ['print', `gui/${uid}/dev.sidevoice.node`]).status === 0
  : !/LoadState=not-found/.test(spawnSync('systemctl', ['--user', 'show', '-p', 'LoadState', 'sidevoice-node.service'], { encoding: 'utf8' }).stdout));
const definition = kind === 'launchd' ? path.join(home, 'Library', 'LaunchAgents', 'dev.sidevoice.node.plist') : path.join(home, '.config', 'systemd', 'user', 'sidevoice-node.service');

/** What a logout and a login do to the job (see the header). `bootout` returns before launchd lets the job go. */
async function relogin() {
  if (kind === 'launchd') {
    spawnSync('launchctl', ['bootout', `gui/${uid}/dev.sidevoice.node`]);
    await until('the job let go', () => !jobLoaded(), 15_000);
    if (existsSync(definition)) await until('bootstrapped', () => spawnSync('launchctl', ['bootstrap', `gui/${uid}`, definition]).status === 0 || jobLoaded(), 15_000);
  } else {
    execFileSync('sudo', ['systemctl', 'restart', `user@${uid}.service`]);
  }
}
async function managerUp() {
  if (kind === 'systemd') await until('the user manager', () => spawnSync('systemctl', ['--user', 'is-system-running'], { encoding: 'utf8' }).stdout.trim().length > 0 && spawnSync('systemctl', ['--user', 'show-environment']).status === 0);
}

/** A façade's connection that registers a conversation and stays open. */
async function openClient() {
  const socket = net.createConnection(path.join(dataDir, 'connector.sock'));
  await new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); });
  socket.on('error', () => {});
  socket.write(JSON.stringify({ id: 1, method: 'register', params: { client_ref: 'ci-thread', harness: 'http', thread: 'ci-thread', title: 'CI', delivery: { kind: 'http', url: 'http://127.0.0.1:9/none', thread: 'ci-thread' } } }) + '\n');
  return socket;
}

try {
  step(`install (${kind}) with no agents, service by default`);
  const installed = sidevoice('install', '--no-agents', '--json');
  assert.equal(installed.code, 0);
  assert.equal(installed.json.service, kind);
  if (kind === 'systemd') console.log(`linger: ${JSON.stringify(installed.json.linger)}`);
  assert.ok(existsSync(definition), 'the definition is written');
  assert.ok(jobLoaded(), 'and the manager has the job');

  step('the core runs with no harness');
  const first = await until('running', () => { const s = status(); return s?.state === 'running' && s.supervisor ? s : null; });
  assert.equal(first.service, kind);
  const supervisor1 = JSON.parse(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8')).pid;
  console.log(`supervisor pid ${supervisor1}, core pid ${first.core.pid}`);

  step('the core is killed: backoff, then running again');
  process.kill(first.core.pid, 'SIGKILL');
  const states = new Set();
  const second = await until('running again', () => { const s = status(); if (s?.state) states.add(s.state); return s?.state === 'running' && s.core?.pid !== first.core.pid ? s : null; });
  console.log(`states seen: ${[...states].join(', ')}; attempts ${second.attempts}`);
  assert.ok(states.has('backoff') || states.has('starting') || second.attempts === 2);
  assert.equal(second.attempts, 2);

  step('the supervisor dies: the manager brings it back (KeepAlive / Restart=on-failure)');
  process.kill(supervisor1, 'SIGKILL');
  const back = await until('a new supervisor', () => { const s = status(); let pid = 0; try { pid = JSON.parse(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8')).pid; } catch {} return s?.state === 'running' && s.supervisor && pid !== supervisor1 ? s : null; }, 90_000);
  console.log(`back: core ${back.core.pid} (adopted: ${back.core.pid === second.core.pid})`);

  step('a login, approximated');
  await relogin(); await managerUp();
  await until('running after the login', () => { const s = status(); return s?.state === 'running' && s.supervisor ? s : null; }, 90_000);

  step('stop: the person\'s — nothing runs, and the launcher refuses');
  const runningNow = status();
  const supervisorNow = JSON.parse(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8')).pid;
  const stopped = sidevoice('service', 'stop', '--json');
  assert.equal(stopped.json.state, 'stopped-by-person');
  assert.equal(status().state, 'stopped-by-person');
  assert.ok(!alive(supervisorNow) && !alive(runningNow.core.pid), 'supervisor and core are gone');
  const refused = sidevoice('pair-device', '--json');
  assert.equal(refused.code, 1);
  assert.equal(refused.json.error.key, 'node.stopped');
  await wait(2000);
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')), 'and nothing was started meanwhile');

  step('start');
  assert.equal(sidevoice('service', 'start', '--json').json.ok, true);
  await until('running after start', () => { const s = status(); return s?.state === 'running' && s.supervisor ? s : null; });
  const code = sidevoice('pair-device', '--json');
  assert.equal(code.json.ok, true); assert.equal(code.json.reach, 'local-only');

  step('uninstall with a client still connected');
  const client = await openClient();
  await wait(500);
  const before = status();
  const supervisorLast = JSON.parse(readFileSync(path.join(dataDir, 'connector.sock.lock'), 'utf8')).pid;
  const removed = sidevoice('service', 'uninstall', '--json');
  assert.equal(removed.json.ok, true);
  client.destroy();
  assert.ok(!jobLoaded(), 'no job');
  assert.ok(!existsSync(definition), 'no definition');
  assert.ok(!alive(supervisorLast) && !alive(before.core.pid), 'no process');
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')) && !existsSync(path.join(dataDir, 'core', 'local.sock')), 'no socket');

  step('a login after the uninstall starts nothing');
  await relogin(); await managerUp();
  await wait(5000);
  assert.ok(!jobLoaded(), 'still no job');
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')), 'still no socket');
  const ps = spawnSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' }).stdout.split('\n').filter(line => /connector --supervise|fake-sidevoice-core/.test(line));
  assert.deepEqual(ps, [], 'still no process');

  step('sidevoice uninstall: everything else');
  assert.equal(sidevoice('uninstall').code, 0);
  assert.ok(!existsSync(dataDir));
  console.log('\nOK');
} catch (error) {
  console.error(`\nFAILED: ${error.stack || error.message}`);
  for (const file of ['node-service.log', 'core.log', 'connector.log']) {
    try { console.error(`\n--- ${file}\n` + readFileSync(path.join(dataDir, file), 'utf8').split('\n').slice(-60).join('\n')); } catch {}
  }
  if (kind === 'launchd') console.error(spawnSync('launchctl', ['print', `gui/${uid}/dev.sidevoice.node`], { encoding: 'utf8' }).stdout);
  else console.error(spawnSync('systemctl', ['--user', 'status', 'sidevoice-node.service'], { encoding: 'utf8' }).stdout);
  process.exitCode = 1;
}
