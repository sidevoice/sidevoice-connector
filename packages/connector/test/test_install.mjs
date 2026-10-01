/** The install transaction (§4.3): which candidate replaces which installation; an installer killed after each of
 *  its side effects — inside a registration's own replacement too — and the machine put back together; installers
 *  that overlap; a selection that cannot run, undone (to nothing, after a first install); and a supervisor that
 *  finds a transaction under way. Interleavings are held at pause points (`testpoint.mjs`), never raced. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { decide, recover } from '../install-txn.mjs';
import { unitText } from '../service.mjs';
import { ensureLockIdentity } from '../lockfile.mjs';
import { supervisedNode } from './test_core.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 30_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(50); } throw new Error('timed out waiting'); }
const exited = child => new Promise(resolve => (child.exitCode !== null || child.signalCode !== null ? resolve(child.exitCode) : child.once('exit', code => resolve(code))));

test('versions: higher replaces, lower never does, and the same version only as a later nightly build', () => {
  const at = (connector, channel = 'release', build_seq = 0) => ({ connector, channel, build_seq, command: ['node', `/c/${connector}/${build_seq}`] });
  assert.equal(decide(null, at('0.6.0')), 'install');
  assert.equal(decide(at('0.6.0'), at('0.7.0')), 'upgrade');
  assert.equal(decide(at('0.7.0'), at('0.6.0')), 'noop', 'never a downgrade: the app and npx converge on the higher');
  assert.equal(decide(at('0.6.0'), at('0.6.0')), 'noop', 'a release\'s artifacts are immutable');
  assert.equal(decide(at('0.6.0', 'release', 7), at('0.6.0', 'release', 9)), 'noop', 'a release build number orders nothing');
  assert.equal(decide(at('0.6.0', 'nightly', 7), at('0.6.0', 'nightly', 9)), 'upgrade');
  assert.equal(decide(at('0.6.0', 'nightly', 9), at('0.6.0', 'nightly', 9)), 'noop');
  assert.equal(decide(at('0.6.0', 'nightly', 9), at('0.6.0', 'nightly', 7)), 'noop', 'two builds never replace each other in a loop');
  assert.equal(decide(at('0.6.10'), at('0.6.9')), 'noop', 'numerically, not as text');
  assert.equal(decide(at('0.6.0'), { ...at('0.6.0'), channel: 'source', command: ['node', '/checkout/cli.mjs'] }), 'upgrade', 'a checkout points everything at itself');
});

/** A stand-in for `claude` that keeps what it was told: `mcp get` answers with the entry `mcp add` wrote. */

/** A stand-in for `claude` that keeps what it was told: `mcp get` answers with the entry `mcp add` wrote.
 *  `FAKE_CLAUDE_FAIL_ADD` makes `add` fail. */
function fakeClaude(dir) {
  const bin = path.join(dir, 'claude'), entry = path.join(dir, 'claude-entry.txt');
  writeFileSync(bin, `#!/bin/sh
case "$2" in
  get) [ -s "${entry}" ] && cat "${entry}" || exit 1 ;;
  remove) rm -f "${entry}" ;;
  add) [ -n "$FAKE_CLAUDE_FAIL_ADD" ] && { echo "add refused" >&2; exit 1; }; shift 6; cmd="$1"; shift; printf 'sidevoice:\\n  Scope: User config\\n  Type: stdio\\n  Command: %s\\n  Args: %s\\n' "$cmd" "$*" > "${entry}" ;;
esac
`, { mode: 0o755 });
  return { bin, entry, line: () => { try { const text = readFileSync(entry, 'utf8'); return `${text.match(/Command: (.*)/)[1]} ${text.match(/Args: (.*)/)[1]}`; } catch { return null; } } };
}

/** A hooks directory: crash or pause points armed. */
function hooks({ crash = [], pause = [] } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-hooks-'));
  for (const point of crash) writeFileSync(path.join(dir, `crash-${point}`), '');
  for (const point of pause) writeFileSync(path.join(dir, `pause-${point}`), '');
  return { dir, paused: point => until(() => existsSync(path.join(dir, `paused-${point}`))), resume: point => writeFileSync(path.join(dir, `resume-${point}`), '') };
}

/** A machine with an older installation (`0.5.0`): its copy under the copies directory, its unit (a systemd that
 *  answers everything), its Claude Code and Cursor entries and `install.json`, all pointing at it — and this
 *  checkout's package as the candidate. */
function upgradeMachine() {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-txn-'));
  const dataDir = path.join(home, '.sidevoice'), copies = path.join(home, 'xdg', 'sidevoice');
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  ensureLockIdentity(dataDir);   // as the first lock taken in it left it
  const claude = fakeClaude(home);
  const oldCli = path.join(copies, '0.5.0', 'dist', 'cli.mjs');
  mkdirSync(path.dirname(oldCli), { recursive: true });
  writeFileSync(oldCli, '// the previous installation\n');
  const from = { id: '0.5.0', connector: '0.5.0', core: '0.1.0', channel: 'release', build_seq: 0, sha256: null, runtime: 'uv', service: 'systemd',
    copy: path.join(copies, '0.5.0'), command: [process.execPath, oldCli], by: 'cli', at: '2026-09-30T10:00:00Z' };
  writeFileSync(path.join(dataDir, 'install.json'), JSON.stringify(from), { mode: 0o600 });
  const unit = path.join(home, '.config', 'systemd', 'user', 'sidevoice-node.service');
  mkdirSync(path.dirname(unit), { recursive: true });
  writeFileSync(unit, unitText({ program: [...from.command, 'connector', '--supervise'], log: path.join(dataDir, 'node-service.log'), environment: {} }));
  mkdirSync(path.join(home, '.cursor'));
  writeFileSync(path.join(home, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { other: { command: 'x' }, sidevoice: { command: 'node', args: [oldCli, 'mcp'] } } }));
  execFileSync(claude.bin, ['mcp', 'add', '--scope', 'user', 'sidevoice', '--', 'node', oldCli, 'mcp']);
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir,
    SIDEVOICE_CLAUDE_BIN: claude.bin, SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: 'systemd', SIDEVOICE_SYSTEMCTL: '/bin/true' };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_CORE_BIN', 'SIDEVOICE_TEST_HOOKS']) delete env[key];
  const version = JSON.parse(readFileSync(path.join(packageDir, 'package.json'), 'utf8')).version;
  const newCli = path.join(copies, version, 'dist', 'cli.mjs');
  /** Where every artifact points: the copy each one runs. */
  const artifacts = () => ({
    install: existsSync(path.join(dataDir, 'install.json')) ? JSON.parse(readFileSync(path.join(dataDir, 'install.json'), 'utf8')).command[1] : null,
    unit: existsSync(unit) ? readFileSync(unit, 'utf8').match(/^ExecStart="[^"]*" "([^"]*)"/m)[1] : null,
    claude: claude.line()?.split(' ')[1] ?? null,
    cursor: JSON.parse(readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8')).mcpServers.sidevoice?.args[0] ?? null,
  });
  const install = (more = {}, args = ['--harness', 'claude', '--no-core']) => spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', ...args], { env: { ...env, ...more }, encoding: 'utf8' });
  return { home, dataDir, env, from, oldCli, newCli, copies, unit, claude, artifacts, install,
    cursorOther: () => JSON.parse(readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8')).mcpServers.other };
}

/** This package built for real — `build.mjs`, stamped as CI stamps it — and copied to stand as a second package,
 *  as the app's bundled connector against npx's. */
function builtAs(version, stamp = {}) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-pkg-'));
  // The real build, with the stamp CI gives it; only the version is changed afterwards, as a release bump would.
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { env: { ...process.env, ...stamp }, stdio: 'ignore' });
  cpSync(path.join(packageDir, 'dist'), path.join(root, 'dist'), { recursive: true });
  for (const manifest of [path.join(root, 'dist', 'package.json'), path.join(root, 'package.json')]) {
    const shipped = JSON.parse(readFileSync(path.join(root, 'dist', 'package.json'), 'utf8'));
    writeFileSync(manifest, JSON.stringify({ ...shipped, version }, null, 2));
  }
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });   // the checkout's dist as it was
  return path.join(root, 'dist', 'cli.mjs');
}
const installWith = (cli, env) => new Promise(resolve => {
  const child = spawn(process.execPath, [cli, 'install', '--no-agents', '--no-core', '--json'], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', d => { out += d; });
  child.on('exit', code => resolve({ code, ...JSON.parse(out.trim().split('\n').at(-1)) }));
});

test('concurrent installers — two genuinely built packages, the app\'s and npx\'s — converge on the higher version, and same-version nightlies on the higher build', async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-conc-'));
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: 'none' };
  const app = builtAs('0.6.0'), npx = builtAs('0.7.0');
  const [a, b] = await Promise.all([installWith(app, env), installWith(npx, env)]);
  assert.equal(a.ok && b.ok, true);
  const installed = JSON.parse(readFileSync(path.join(env.SIDEVOICE_DATA_DIR, 'install.json'), 'utf8'));
  assert.equal(installed.connector, '0.7.0');
  assert.equal((await installWith(app, env)).action, 'noop');
  assert.equal((await installWith(npx, env)).action, 'noop');
  // Same version, stamped by the build as CI stamps a nightly: the higher run number wins, whichever runs last.
  const home2 = mkdtempSync(path.join(os.tmpdir(), 'sv-conc-'));
  const env2 = { ...env, HOME: home2, XDG_DATA_HOME: path.join(home2, 'xdg'), SIDEVOICE_DATA_DIR: path.join(home2, '.sidevoice') };
  const older = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '10' }), newer = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '12' });
  assert.deepEqual(JSON.parse(readFileSync(path.join(path.dirname(newer), 'package.json'), 'utf8')).sidevoice, { channel: 'nightly', build_seq: 12 }, 'stamped into the shipped manifest');
  await Promise.all([installWith(older, env2), installWith(newer, env2)]);
  assert.equal(JSON.parse(readFileSync(path.join(env2.SIDEVOICE_DATA_DIR, 'install.json'), 'utf8')).build_seq, 12);
  assert.equal((await installWith(older, env2)).action, 'noop');
  assert.equal((await installWith(newer, env2)).action, 'noop');
  assert.ok(existsSync(path.join(env2.XDG_DATA_HOME, 'sidevoice', '0.6.0-nightly.12')), 'a nightly build is a copy of its own');
});

/** A wrapper for the fake core; `mode` is baked in, so a service started from a definition (which carries only
 *  Sidevoice's own settings) runs it too. Its self-test passes either way. */
function coreWrapper(mode = '') {
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-bin-'));
  const bin = path.join(tools, 'sidevoice-core');
  writeFileSync(bin, `#!/bin/sh\n${mode ? `FAKE_CORE_MODE=${mode} ` : ''}exec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  return bin;
}

test('install transaction: an upgrade whose core does not come up is rolled back — install.json and every registration back on the previous one', async () => {
  const machine = upgradeMachine();
  rmSync(machine.unit);
  const env = { SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_CORE_BIN: coreWrapper(), FAKE_CORE_MODE: 'import', SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_VERIFY_MS: '8000' };
  const run = machine.install(env, ['--harness', 'claude', '--no-service', '--json']);
  const result = JSON.parse(run.stdout.trim().split('\n').at(-1));
  assert.equal(run.status, 1);
  assert.equal(result.ok, false);
  // The previous installation here is a placeholder that cannot run either: said apart from a rollback that runs.
  assert.equal(result.error.key, 'install.rollback-failed');
  const { install, claude, cursor } = machine.artifacts();
  assert.deepEqual({ install, claude, cursor }, { install: machine.oldCli, claude: machine.oldCli, cursor: machine.oldCli });
  // Every artifact is back on it, and the
  // journal stays (forced to it) until it is seen running — it is not declared done on files alone.
  const journal = JSON.parse(readFileSync(path.join(machine.dataDir, 'install-txn.json'), 'utf8'));
  assert.equal(journal.selection, 'from');
  supervisedNode({ dataDir: machine.dataDir }).stop();
  // A core self-test that fails stops before the commit: nothing changed at all.
  const fresh = upgradeMachine();
  rmSync(fresh.unit);
  const tested = fresh.install({ ...env, FAKE_CORE_MODE: 'ok', FAKE_CORE_SELF_TEST_FAIL: '1' }, ['--harness', 'claude', '--no-service', '--json']);
  assert.equal(JSON.parse(tested.stdout.trim().split('\n').at(-1)).error.key, 'import.missing-module');
  assert.equal(existsSync(path.join(fresh.dataDir, 'install-txn.json')), false, 'no journal: nothing was committed');
  assert.equal(JSON.parse(readFileSync(path.join(fresh.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
  assert.deepEqual(fresh.artifacts(), { install: fresh.oldCli, unit: null, claude: fresh.oldCli, cursor: fresh.oldCli });
});

test('install transaction: foreign entries named sidevoice — even another program called cli.mjs — are never replaced or removed', async () => {
  const machine = upgradeMachine();
  execFileSync(machine.claude.bin, ['mcp', 'remove', '--scope', 'user', 'sidevoice']);
  execFileSync(machine.claude.bin, ['mcp', 'add', '--scope', 'user', 'sidevoice', '--', 'node', '/opt/unrelated/cli.mjs', 'mcp']);
  writeFileSync(path.join(machine.home, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { sidevoice: { command: 'node', args: ['/opt/unrelated/cli.mjs', 'mcp'] } } }));
  assert.equal(machine.install({}, ['--harness', 'claude', '--harness', 'cursor', '--no-core']).status, 0);
  assert.equal(machine.artifacts().claude, '/opt/unrelated/cli.mjs');
  assert.equal(machine.artifacts().cursor, '/opt/unrelated/cli.mjs');
  spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'uninstall', '--harness', 'claude'], { env: { ...machine.env, SIDEVOICE_SYSTEMCTL: '/bin/true' }, encoding: 'utf8' });
  assert.equal(machine.claude.line(), 'node /opt/unrelated/cli.mjs mcp', 'uninstall leaves it too');
});
