#!/usr/bin/env node
/** Sidevoice's two jobs under this machine's real service manager — launchd (macOS) or a systemd user manager
 *  (Linux) — end to end, for CI (`.github/workflows/ci.yml`, jobs `service-macos` and `service-linux`). Not part of
 *  `npm test`: it installs real LaunchAgents or user units in this user's home and must run where that is wanted.
 *
 *  Three real builds of this package (0.6.1, 0.6.2, 0.6.3), installed as a person's machine would install them; the
 *  core is the fake (`fake-sidevoice-core.mjs`), through a wrapper whose behaviour the script switches; everything
 *  else is real. The flow (Opus §3.3): install with no agents → both jobs up with no harness → the core killed (-9):
 *  the manager restarts it → a core that fails its import: `failed{import.missing-module}` and no new process for three
 *  times the throttle → `service restart` with a healthy core: running → a login, approximated → stop (the launcher
 *  refuses) → start → upgrade 0.6.1→0.6.2 (healthy) → upgrade 0.6.2→0.6.3 (its core cannot serve): back on 0.6.2 →
 *  uninstall with a client connected: no job, process, socket or release, and a login after it starts nothing.
 *
 *  A logout/login cannot be done on a CI runner. It is approximated by what it does to our jobs: launchd — `bootout` of
 *  each job and `bootstrap` of its plist from ~/Library/LaunchAgents, which is what loginwindow does to LaunchAgents at
 *  logout and login; systemd — a restart of the user's manager (`user@<uid>.service`), which stops every user unit and
 *  starts the enabled ones, as the end and start of the user's last session do. */
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const home = os.homedir();
const dataDir = path.join(home, '.sidevoice');
const R = path.join(process.env.XDG_DATA_HOME || path.join(home, '.local', 'share'), 'sidevoice');
const uid = process.getuid();
const kind = process.platform === 'darwin' ? 'launchd' : 'systemd';
const THROTTLE_S = 10;   // launchd ThrottleInterval, systemd RestartSec of the core job
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const step = text => console.log(`\n=== ${text}`);
async function until(what, check, timeout = 90_000) {
  const start = Date.now();
  for (;;) { const value = await check(); if (value) return value; if (Date.now() - start > timeout) throw new Error(`timed out: ${what}`); await wait(500); }
}
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

/** This package built for real and copied, as version `version`. */
function builtAs(version) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-pkg-'));
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  cpSync(path.join(packageDir, 'dist'), path.join(root, 'dist'), { recursive: true });
  for (const manifest of [path.join(root, 'dist', 'package.json'), path.join(root, 'package.json')]) {
    writeFileSync(manifest, JSON.stringify({ ...JSON.parse(readFileSync(path.join(root, 'dist', 'package.json'), 'utf8')), version }, null, 2));
  }
  return path.join(root, 'dist', 'cli.mjs');
}
/** The fake core, through a wrapper a release links to by absolute path; its mode is read at each start. */
function coreWrapper(mode) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-core-'));
  const bin = path.join(dir, 'sidevoice-core'), file = path.join(dir, 'mode');
  writeFileSync(file, mode);
  writeFileSync(bin, `#!/bin/sh\nFAKE_CORE_MODE=$(cat "${file}") exec "${process.execPath}" "${path.join(here, 'fake-sidevoice-core.mjs')}" "$@"\n`, { mode: 0o755 });
  return { bin, set: next => writeFileSync(file, next) };
}

const env = { ...process.env, SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_FROM_SOURCE: '0' };
for (const name of ['SIDEVOICE_SERVICE_MANAGER', 'SIDEVOICE_DATA_DIR', 'SIDEVOICE_URL', 'SIDEVOICE_CORE_BIN']) delete env[name];
const current = () => path.join(R, 'current', 'dist', 'cli.mjs');
function sidevoice(cli, args, more = {}) {
  const run = spawnSync(process.execPath, [cli, ...args], { env: { ...env, ...more }, encoding: 'utf8', timeout: 240_000 });
  if (run.stderr.trim()) console.log(run.stderr.trim().split('\n').map(line => '  | ' + line).join('\n'));
  let json = null; try { json = JSON.parse(run.stdout.trim().split('\n').at(-1)); } catch {}
  console.log(`$ sidevoice ${args.join(' ')} → exit ${run.status}${json ? ' ' + JSON.stringify(json).slice(0, 500) : ''}`);
  return { code: run.status, json };
}
const status = () => sidevoice(current(), ['service', 'status', '--json']).json;
const selected = () => { try { return JSON.parse(readFileSync(path.join(R, 'current', 'release.json'), 'utf8')).connector; } catch { return null; } };

const LABEL = { core: 'dev.sidevoice.core', connector: 'dev.sidevoice.connector' };
const UNIT = { core: 'sidevoice-core.service', connector: 'sidevoice-connector.service' };
const definition = job => kind === 'launchd' ? path.join(home, 'Library', 'LaunchAgents', `${LABEL[job]}.plist`) : path.join(home, '.config', 'systemd', 'user', UNIT[job]);
const loaded = job => (kind === 'launchd'
  ? spawnSync('launchctl', ['print', `gui/${uid}/${LABEL[job]}`]).status === 0
  : !/LoadState=not-found/.test(spawnSync('systemctl', ['--user', 'show', '-p', 'LoadState', UNIT[job]], { encoding: 'utf8' }).stdout));
/** The pid the manager runs a job as, or null. */
function jobPid(job) {
  if (kind === 'launchd') return Number(spawnSync('launchctl', ['print', `gui/${uid}/${LABEL[job]}`], { encoding: 'utf8' }).stdout.match(/^\s*pid = (\d+)/m)?.[1]) || null;
  return Number(spawnSync('systemctl', ['--user', 'show', '-p', 'MainPID', '--value', UNIT[job]], { encoding: 'utf8' }).stdout.trim()) || null;
}
/** What the connector job answers about itself (`status`): its version. */
function connectorVersion() {
  return new Promise(resolve => {
    const socket = net.createConnection(path.join(dataDir, 'connector.sock'));
    let buffer = '';
    const timer = setTimeout(() => { socket.destroy(); resolve(null); }, 3000);
    socket.on('error', () => { clearTimeout(timer); resolve(null); });
    socket.on('connect', () => socket.write(JSON.stringify({ id: 1, method: 'status', params: {} }) + '\n'));
    socket.on('data', chunk => { buffer += chunk; const i = buffer.indexOf('\n'); if (i < 0) return; clearTimeout(timer); socket.destroy(); try { resolve(JSON.parse(buffer.slice(0, i)).result.version); } catch { resolve(null); } });
  });
}

/** What a logout and a login do to the jobs (see the header). `bootout` returns before launchd lets a job go. */
async function relogin() {
  if (kind === 'launchd') {
    for (const job of ['connector', 'core']) {
      spawnSync('launchctl', ['bootout', `gui/${uid}/${LABEL[job]}`]);
      await until(`${job} let go`, () => !loaded(job), 15_000);
    }
    for (const job of ['core', 'connector']) if (existsSync(definition(job))) await until(`${job} bootstrapped`, () => spawnSync('launchctl', ['bootstrap', `gui/${uid}`, definition(job)]).status === 0 || loaded(job), 15_000);
  } else {
    execFileSync('sudo', ['systemctl', 'restart', `user@${uid}.service`]);
    await until('the user manager', () => spawnSync('systemctl', ['--user', 'show-environment']).status === 0);
  }
}

/** A façade's connection that registers a conversation and stays open. */
async function openClient() {
  const socket = net.createConnection(path.join(dataDir, 'connector.sock'));
  await new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); });
  socket.on('error', () => {});
  socket.write(JSON.stringify({ id: 1, method: 'register', params: { client_ref: 'ci-thread', harness: 'http', thread: 'ci-thread', title: 'CI', delivery: { kind: 'http', url: 'http://127.0.0.1:9/none', thread: 'ci-thread' } } }) + '\n');
  return socket;
}
/** This machine's Sidevoice processes: a connector of a release under R, or a core of this data directory. */
const ours = () => spawnSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' }).stdout.split('\n')
  .filter(line => (line.includes(' connector') && line.includes(R)) || line.includes(`--data-dir ${path.join(dataDir, 'core')}`));

try {
  step('three builds of this package');
  const [a, b, c] = ['0.6.1', '0.6.2', '0.6.3'].map(builtAs);
  const core = coreWrapper('ok'), broken = coreWrapper('import');

  step(`install 0.6.1 (${kind}) with no agents, as a login service`);
  const installed = sidevoice(a, ['install', '--no-agents', '--service', '--json'], { SIDEVOICE_CORE_BIN: core.bin });
  assert.equal(installed.code, 0);
  assert.deepEqual([installed.json.service, installed.json.state, selected()], [kind, 'running', '0.6.1']);
  if (kind === 'systemd') console.log(`linger: ${JSON.stringify(installed.json.linger)}`);
  for (const job of ['core', 'connector']) { assert.ok(existsSync(definition(job)), `${job}: defined`); assert.ok(loaded(job), `${job}: loaded`); }
  if (kind === 'systemd') {
    // The units as systemd itself parses them: every directive ours wrote is one it accepts, as written.
    const verified = spawnSync('systemd-analyze', ['--user', 'verify', definition('core'), definition('connector')], { encoding: 'utf8' });
    console.log(`systemd-analyze verify → exit ${verified.status}\n${verified.stdout}${verified.stderr}`);
    assert.equal(verified.status, 0);
    assert.ok(!/sidevoice-(core|connector)\.service:\d+:/.test(verified.stderr), 'no complaint about any line of either unit');
  }

  step('both jobs up with no harness');
  const first = await until('running', () => { const s = status(); return s?.state === 'running' && s.connector.running ? s : null; });
  assert.equal(first.core.pid, jobPid('core'), 'the core answering is the core job');
  assert.equal(await connectorVersion(), '0.6.1');

  step('kill -9 the core: the manager starts it again');
  process.kill(first.core.pid, 'SIGKILL');
  const states = new Set();
  const second = await until('running again', () => { const s = status(); if (s?.state) states.add(s.state); return s?.state === 'running' && s.core?.pid !== first.core.pid ? s : null; });
  console.log(`states seen: ${[...states].join(', ')}; attempts ${second.attempts}, limit ${second.limit}`);

  step(`a core that fails its import: failed, and no new process for ${3 * THROTTLE_S} s`);
  core.set('import');
  process.kill(second.core.pid, 'SIGKILL');
  const failed = await until('failed', () => { const s = status(); return s?.state === 'failed' ? s : null; });
  assert.equal(failed.failure.key, 'import.missing-module', JSON.stringify(failed));
  const starts = () => { try { return readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').split('\n').filter(line => line.includes('"started"')).length; } catch { return 0; } };
  const before = starts();
  await wait(3 * THROTTLE_S * 1000);
  assert.equal(starts(), before, 'nothing started it again: a failed start exits 0');
  assert.equal(jobPid('core'), null);
  assert.equal(status().failure.key, 'import.missing-module');

  step('service restart with a healthy core: running');
  core.set('ok');
  const restarted = sidevoice(current(), ['service', 'restart', '--json']);
  assert.equal(restarted.json.state, 'running', JSON.stringify(restarted.json));

  step('a login, approximated');
  await relogin();
  await until('running after the login', () => { const s = status(); return s?.state === 'running' && s.connector.running ? s : null; });

  step('stop: the person\'s — nothing runs, and the launcher refuses');
  const pids = [jobPid('core'), jobPid('connector')];
  const stopped = sidevoice(current(), ['service', 'stop', '--json']);
  assert.equal(stopped.json.state, 'stopped-by-person');
  assert.equal(status().state, 'stopped-by-person');
  assert.ok(pids.every(pid => !alive(pid)), 'both jobs are gone');
  const refused = sidevoice(current(), ['pair-device', '--json']);
  assert.equal(refused.code, 1);
  assert.equal(refused.json.error.key, 'node.stopped');
  await wait(2000);
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')), 'and nothing was started meanwhile');

  step('start');
  assert.equal(sidevoice(current(), ['service', 'start', '--json']).json.state, 'running');
  const code = await until('a device code', () => { const run = sidevoice(current(), ['pair-device', '--json']); return run.json?.ok ? run.json : null; }, 30_000);
  assert.equal(code.reach, 'local-only');

  step('upgrade 0.6.1 → 0.6.2: the jobs restarted on it and verified');
  const upgraded = sidevoice(b, ['install', '--no-agents', '--json'], { SIDEVOICE_CORE_BIN: core.bin });
  assert.deepEqual([upgraded.code, upgraded.json.action, upgraded.json.state, selected()], [0, 'upgrade', 'running', '0.6.2']);
  assert.equal(await connectorVersion(), '0.6.2');

  step('upgrade 0.6.2 → 0.6.3, whose core cannot serve: back on 0.6.2');
  const rolled = sidevoice(c, ['install', '--no-agents', '--json'], { SIDEVOICE_CORE_BIN: broken.bin });
  assert.equal(rolled.code, 1);
  assert.deepEqual([rolled.json.error?.key, rolled.json.failure?.key, selected()], ['install.rollback', 'import.missing-module', '0.6.2'], JSON.stringify(rolled.json));
  await until('running on 0.6.2', async () => status()?.state === 'running' && (await connectorVersion()) === '0.6.2');
  assert.deepEqual(readdirSync(path.join(R, 'releases')), ['0.6.2'], 'the broken release pruned');

  step('uninstall with a client still connected');
  const client = await openClient();
  await wait(500);
  const last = [jobPid('core'), jobPid('connector')];
  const removed = sidevoice(current(), ['uninstall', '--json']);
  assert.equal(removed.code, 0);
  client.destroy();
  for (const job of ['core', 'connector']) { assert.ok(!loaded(job), `${job}: no job`); assert.ok(!existsSync(definition(job)), `${job}: no definition`); }
  assert.ok(last.every(pid => !alive(pid)), 'no process');
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')) && !existsSync(path.join(dataDir, 'core', 'local.sock')), 'no socket');
  assert.ok(!existsSync(R), 'no release');
  assert.deepEqual(readdirSync(dataDir).sort(), ['connector.lock', 'install.lock'].filter(name => existsSync(path.join(dataDir, name))), 'only the permanent lock files');

  step('a login after the uninstall starts nothing');
  await relogin();
  await wait(5000);
  for (const job of ['core', 'connector']) assert.ok(!loaded(job), `${job}: still no job`);
  assert.ok(!existsSync(path.join(dataDir, 'connector.sock')), 'still no socket');
  assert.deepEqual(ours(), [], 'still no process');
  console.log('\nOK');
} catch (error) {
  console.error(`\nFAILED: ${error.stack || error.message}`);
  for (const file of ['connector.log', 'core.log', 'core.stderr.log']) {
    try { console.error(`\n--- ${file}\n` + readFileSync(path.join(dataDir, file), 'utf8').split('\n').slice(-60).join('\n')); } catch {}
  }
  for (const job of ['core', 'connector']) {
    if (kind === 'launchd') console.error(spawnSync('launchctl', ['print', `gui/${uid}/${LABEL[job]}`], { encoding: 'utf8' }).stdout);
    else console.error(spawnSync('systemctl', ['--user', 'status', UNIT[job]], { encoding: 'utf8' }).stdout + spawnSync('journalctl', ['--user', '-u', UNIT[job], '-n', '40', '--no-pager'], { encoding: 'utf8' }).stdout);
  }
  process.exitCode = 1;
}
