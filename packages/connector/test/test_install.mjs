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

/** Every side effect of a commit, in order — `claude-removed` is inside Claude Code's own replacement (its entry
 *  removed, the new one not yet added) — and which side recovery must select after a kill there. */
const CRASH_POINTS = [['txn-journal', 'from'], ['txn-copy', 'from'], ['txn-definition', 'from'], ['claude-removed', 'from'],
  ['txn-register:claude', 'from'], ['txn-register:cursor', 'from'], ['txn-install.json', 'to']];

for (const [point, selected] of CRASH_POINTS) {
  test(`install transaction: killed right after "${point}", then recovered — every artifact points at the selected installation (${selected})`, async () => {
    const machine = upgradeMachine();
    const armed = hooks({ crash: [point] });
    const run = machine.install({ SIDEVOICE_TEST_HOOKS: armed.dir });
    assert.equal(run.signal, 'SIGKILL', `killed (${run.stderr})`);
    assert.ok(existsSync(path.join(machine.dataDir, 'install-txn.json')), 'the journal was written before any side effect');
    if (point === 'claude-removed') assert.equal(machine.claude.line(), null, 'killed with no Claude entry at all');
    const outcome = await recover(machine.env, { mode: 'artifacts' });
    const cli = selected === 'to' ? machine.newCli : machine.oldCli;
    assert.equal(outcome.side, selected);
    assert.deepEqual(machine.artifacts(), { install: cli, unit: cli, claude: cli, cursor: cli });
    assert.ok(existsSync(path.dirname(path.dirname(cli))), 'its copy is there');
    assert.ok(existsSync(path.dirname(path.dirname(machine.oldCli))), 'and the previous copy is never deleted by a transaction');
    assert.deepEqual(readdirSync(machine.copies).filter(name => name.endsWith('.staging')), [], 'no staging left');
    assert.equal(existsSync(path.join(machine.dataDir, 'install-txn.json')), false, 'the journal is gone once every artifact is verified');
    assert.deepEqual(machine.cursorOther(), { command: 'x' }, 'a foreign entry is untouched');
    // And the next install completes the upgrade from there.
    const again = machine.install();
    assert.equal(again.status, 0, again.stderr);
    assert.deepEqual(machine.artifacts(), { install: machine.newCli, unit: machine.newCli, claude: machine.newCli, cursor: machine.newCli });
  });
}

test('install transaction: a registration that cannot be put back keeps the journal, and says so', async () => {
  const machine = upgradeMachine();
  const armed = hooks({ crash: ['claude-removed'] });
  assert.equal(machine.install({ SIDEVOICE_TEST_HOOKS: armed.dir }).signal, 'SIGKILL');
  await assert.rejects(recover({ ...machine.env, FAKE_CLAUDE_FAIL_ADD: '1' }, { mode: 'artifacts' }), /add refused/);
  assert.ok(existsSync(path.join(machine.dataDir, 'install-txn.json')), 'kept: the machine is not yet what it should be');
  await recover(machine.env, { mode: 'artifacts' });
  assert.equal(machine.artifacts().claude, machine.oldCli);
  assert.equal(existsSync(path.join(machine.dataDir, 'install-txn.json')), false);
});

test('install transaction: an installer held before its registrations keeps the lock; a newer installer waits, then wins — every artifact on the newer', async () => {
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  const machine = upgradeMachine();
  const older = builtAs('0.6.0'), newer = builtAs('0.7.0');
  const held = hooks({ pause: ['txn-register-claude'] });
  const env = { ...machine.env, SIDEVOICE_INSTALL_FROM_SOURCE: '0' };
  const first = spawn(process.execPath, [older, 'install', '--harness', 'claude', '--no-core'], { env: { ...env, SIDEVOICE_TEST_HOOKS: held.dir }, stdio: 'ignore' });
  await held.paused('txn-register-claude');
  const second = spawn(process.execPath, [newer, 'install', '--harness', 'claude', '--no-core'], { env, stdio: 'ignore' });
  await wait(1000);
  assert.equal(second.exitCode, null, 'the newer installer waits for the lock');
  held.resume('txn-register-claude');
  assert.deepEqual([await exited(first), await exited(second)], [0, 0]);
  const cli = path.join(machine.copies, '0.7.0', 'dist', 'cli.mjs');
  assert.deepEqual(machine.artifacts(), { install: cli, unit: cli, claude: cli, cursor: cli });
});

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

test('install transaction: a first install whose core passes its self-test but cannot serve is undone to nothing — never ok, nothing left pointing anywhere', async () => {
  for (const variant of ['no-service', 'service']) {
    const home = mkdtempSync(path.join(os.tmpdir(), 'sv-first-'));
    const dataDir = path.join(home, '.sidevoice');
    mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const claude = fakeClaude(home);
    const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-mgr-'));
    const manager = path.join(tools, 'systemctl');
    writeFileSync(manager, `#!/bin/sh\nexec "${process.execPath}" "${path.join(here, 'fake-service-manager.mjs')}" systemctl "$@"\n`, { mode: 0o755 });
    const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir,
      SIDEVOICE_CLAUDE_BIN: claude.bin, SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_CORE_BIN: coreWrapper('import'), SIDEVOICE_CORE_PORT: '0',
      SIDEVOICE_SERVICE_MANAGER: variant === 'service' ? 'systemd' : 'none', SIDEVOICE_SYSTEMCTL: manager, FAKE_MANAGER_DIR: path.join(tools, 'state'),
      SIDEVOICE_INSTALL_VERIFY_MS: '8000', SIDEVOICE_BACKOFF_MS: '50', SIDEVOICE_TEARDOWN_MS: '3000' };
    delete env.SIDEVOICE_TEST_HOOKS;
    const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--harness', 'claude', '--harness', 'cursor', ...(variant === 'service' ? [] : ['--no-service']), '--json'], { env, encoding: 'utf8' });
    const result = JSON.parse(run.stdout.trim().split('\n').at(-1));
    assert.equal(run.status, 1, `${variant}: ${run.stdout}`);
    assert.equal(result.ok, false);
    assert.equal(result.error.key, 'install.rollback');
    assert.equal(existsSync(path.join(dataDir, 'install.json')), false, `${variant}: nothing selected`);
    assert.equal(existsSync(path.join(dataDir, 'install-txn.json')), false, `${variant}: nothing left to recover`);
    assert.equal(existsSync(path.join(home, '.config', 'systemd', 'user', 'sidevoice-node.service')), false, `${variant}: no definition`);
    assert.equal(claude.line(), null, `${variant}: no registration`);
    assert.equal(existsSync(path.join(home, 'xdg', 'sidevoice', JSON.parse(readFileSync(path.join(packageDir, 'package.json'), 'utf8')).version)), false, `${variant}: no copy`);
    supervisedNode({ dataDir }).stop();
    try { process.kill(Number(readFileSync(path.join(tools, 'state', 'pid'), 'utf8')), 'SIGKILL'); } catch {}
  }
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

/** Two real copies of this package (built), as two installations of it would be, and a journal between them. */
function twoInstallations() {
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  const node = supervisedNode();
  const copies = path.join(path.dirname(node.dataDir), path.basename(node.dataDir) + '-copies');
  const record = id => {
    const copy = path.join(copies, id);
    cpSync(path.join(packageDir, 'dist'), path.join(copy, 'dist'), { recursive: true });
    return { id, connector: id, core: '0.1.0', channel: 'release', build_seq: 0, runtime: 'external', service: 'none', copy, command: [process.execPath, path.join(copy, 'dist', 'cli.mjs')] };
  };
  const from = record('0.5.0'), to = record('0.6.0');
  mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
  const journal = { from, to, harnesses: { claude: { before: 'absent', want: false }, cursor: { before: 'absent', want: false } }, service: { kind: 'none', before: true, want: true }, at: new Date().toISOString() };
  writeFileSync(path.join(node.dataDir, 'install-txn.json'), JSON.stringify(journal), { mode: 0o600 });
  const startAs = installation => {
    const child = spawn(process.execPath, [installation.command[1], 'connector', '--supervise'], { env: { ...node.env, HOME: path.dirname(node.dataDir), XDG_DATA_HOME: path.dirname(copies), SIDEVOICE_SERVICE: 'none' }, stdio: 'ignore' });
    node.children.push(child); return child;
  };
  const holder = () => { try { const pid = JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid; return readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').join(' '); } catch { return ''; } };
  return { node, from, to, startAs, holder };
}

test('supervisor recovery: a committed upgrade that cannot run is rolled back by the supervisor, which hands over to the previous installation; the journal goes once that runs', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, from, to, startAs, holder } = twoInstallations();
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(to), { mode: 0o600 });
  writeFileSync(node.modesFile, ['import', 'import', 'import', 'import', 'import', 'ok'].join('\n') + '\n');
  try {
    startAs(to);
    await until(() => !existsSync(path.join(node.dataDir, 'install-txn.json')) && holder().includes(from.copy), 60_000);
    const status = await node.status(s => s.state === 'running', 30_000);
    assert.deepEqual(status.command, from.command, 'the previous installation\'s program serves');
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
  } finally { node.stop(); }
});

test('supervisor recovery: a supervisor started from the new program while the selection is still the old one hands over to the old one', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, from, to, startAs, holder } = twoInstallations();
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(from), { mode: 0o600 });
  try {
    startAs(to);
    await until(() => holder().includes(from.copy), 30_000);
    const status = await node.status(s => s.state === 'running', 30_000);
    assert.deepEqual(status.command, from.command);
    await until(() => !existsSync(path.join(node.dataDir, 'install-txn.json')), 15_000);
  } finally { node.stop(); }
});

test('supervisor recovery: a committed upgrade whose core runs but speaks another link protocol is a failure — rolled back to the previous installation, never declared done', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, from, to, startAs, holder } = twoInstallations();
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(to), { mode: 0o600 });
  writeFileSync(node.modesFile, ['ok+link:999', 'ok'].join('\n') + '\n');
  try {
    startAs(to);
    await until(() => !existsSync(path.join(node.dataDir, 'install-txn.json')) && holder().includes(from.copy), 60_000);
    const status = await node.status(s => s.state === 'running', 30_000);
    assert.deepEqual(status.command, from.command);
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
    assert.match(node.log(), /does not run compatibly \(install\.incompatible\): rolling back/);
  } finally { node.stop(); }
});

/** A machine where a built 0.5.0 copy is installed and its plain connector runs, and this checkout is the candidate. */
function runningPlainMachine(modes = ['ok']) {
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  const node = supervisedNode({ modes });
  const home = path.dirname(node.dataDir);
  const xdg = path.join(home, path.basename(node.dataDir) + '-xdg');
  const copy = path.join(xdg, 'sidevoice', '0.5.0');
  cpSync(path.join(packageDir, 'dist'), path.join(copy, 'dist'), { recursive: true });
  const from = { id: '0.5.0', connector: '0.5.0', core: '0.1.0', channel: 'release', build_seq: 0, runtime: 'external', runtime_id: 'external', service: 'none', copy, command: [process.execPath, path.join(copy, 'dist', 'cli.mjs')] };
  mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(from), { mode: 0o600 });
  const env = { ...node.env, XDG_DATA_HOME: xdg, SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_INSTALL_VERIFY_MS: '15000' };
  return { node, env, from, xdg };
}

test('no-service upgrade: the running connector of the previous installation is replaced by the selected one, and that is what is verified', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, env, from, xdg } = runningPlainMachine();
  try {
    const old = spawn(from.command[0], [from.command[1], 'connector'], { env, stdio: 'ignore' });
    node.children.push(old);
    await until(() => existsSync(node.socketPath));
    await node.ask('node.ensure');
    const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--no-agents', '--no-service', '--json'], { env, encoding: 'utf8' });
    const answer = JSON.parse(run.stdout.trim().split('\n').at(-1));
    assert.equal(run.status, 0, run.stdout + run.stderr);
    assert.equal(answer.action, 'upgrade');
    const selected = JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8'));
    const status = await node.ask('node.status');
    assert.deepEqual(status.command, selected.command, 'the connector answering is the selected installation\'s');
    assert.ok(status.command[1].startsWith(path.join(xdg, 'sidevoice')) && !status.command[1].includes('0.5.0'));
    await until(() => old.exitCode !== null, 10_000);
    assert.equal(existsSync(path.join(node.dataDir, 'install-txn.json')), false);
  } finally { node.stop(); }
});

test('rollback to nothing: a first install whose core serves but speaks another link protocol leaves no supervisor, connector or core running — with and without a service', async () => {
  for (const service of [true, false]) {
    const node = supervisedNode({ modes: ['ok+link:999'] });
    const home = path.dirname(node.dataDir);
    const env = { ...node.env, XDG_DATA_HOME: path.join(home, path.basename(node.dataDir) + '-xdg'), SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_INSTALL_VERIFY_MS: '8000' };
    try {
      const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--no-agents', ...(service ? [] : ['--no-service']), '--json'], { env, encoding: 'utf8' });
      const answer = JSON.parse(run.stdout.trim().split('\n').at(-1));
      assert.equal(run.status, 1, `service ${service}: ${run.stdout}`);
      assert.equal(answer.error.key, 'install.rollback');
      assert.equal(existsSync(path.join(node.dataDir, 'install.json')), false);
      assert.equal(existsSync(path.join(node.dataDir, 'install-txn.json')), false);
      const cores = [...new Set(node.said().filter(line => line.event === 'started').map(line => line.pid))];
      assert.ok(cores.length >= 1, 'a core was started and judged');
      for (const pid of cores) assert.equal((() => { try { process.kill(pid, 0); return true; } catch { return false; } })(), false, `service ${service}: core ${pid} is gone`);
      assert.equal(await node.ask('node.status').then(() => true, () => false), false, `service ${service}: nothing answers on the socket`);
    } finally { node.stop(); }
  }
});

/** A stand-in for `systemd-run --user`: runs the command after `--`, detached, as a transient unit would. */
function systemdRunShim(dir) {
  const shim = path.join(dir, 'systemd-run');
  writeFileSync(shim, `#!/bin/sh\nwhile [ "$1" != "--" ]; do shift; done; shift\nexec "$@"\n`, { mode: 0o755 });
  return shim;
}
const running = pid => { try { process.kill(pid, 0); return !/^\S+ \(.*\) Z/.test(readFileSync(`/proc/${pid}/stat`, 'utf8')); } catch { return false; } };

test('supervisor recovery to nothing: a first install committed by an installer that died, whose core cannot serve — the supervisor never signals itself; recovery outside it stops everything, verifies, and the next start finds nothing', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  for (const manager of ['none', 'systemd']) {
    const node = supervisedNode({ modes: ['ok+link:999'] });
    // A home of its own, its paths the defaults: a unit's process sees HOME and the settings, not this shell's XDG_*.
    const home = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
    const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-mgr-'));
    // Under a manager the core sees only the installation's settings: its modes go in its wrapper.
    writeFileSync(node.bin, `#!/bin/sh\nFAKE_CORE_MODES="${node.modesFile}" FAKE_CORE_WRAPPER="${node.bin}" exec "${process.execPath}" "${path.join(here, 'fake-sidevoice-core.mjs')}" "$@"\n`, { mode: 0o755 });
    const systemctl = path.join(tools, 'systemctl');
    // Its state directory written into it: a unit's own `systemctl` calls (the supervisor's) find it too.
    writeFileSync(systemctl, `#!/bin/sh\nFAKE_MANAGER_DIR="${path.join(tools, 'state')}" exec "${process.execPath}" "${path.join(here, 'fake-service-manager.mjs')}" systemctl "$@"\n`, { mode: 0o755 });
    const env = { ...node.env, HOME: home,
      SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_INSTALL_VERIFY_MS: '8000', SIDEVOICE_TEARDOWN_MS: '3000', SIDEVOICE_SERVICE_MANAGER: manager,
      SIDEVOICE_SYSTEMCTL: systemctl, FAKE_MANAGER_DIR: path.join(tools, 'state'), SIDEVOICE_SYSTEMD_RUN: systemdRunShim(tools) };
    for (const name of ['XDG_DATA_HOME', 'XDG_CONFIG_HOME']) delete env[name];
    const unit = path.join(home, '.config', 'systemd', 'user', 'sidevoice-node.service');
    try {
      const armed = hooks({ crash: ['txn-install.json'] });
      const died = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--no-agents', '--json'], { env: { ...env, SIDEVOICE_TEST_HOOKS: armed.dir }, encoding: 'utf8' });
      assert.equal(died.signal, 'SIGKILL', `${manager}: ${died.stdout}${died.stderr}`);
      assert.ok(existsSync(path.join(node.dataDir, 'install.json')) && existsSync(path.join(node.dataDir, 'install-txn.json')), 'committed, journal left');
      const started = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'service', 'start', '--json'], { env, encoding: 'utf8' });
      assert.equal(started.status, 0, `${manager}: ${started.stdout}${started.stderr}`);
      const supervisor = await until(() => { try { return JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid; } catch { return null; } });
      await until(() => !existsSync(path.join(node.dataDir, 'install-txn.json')), 60_000);
      assert.equal(existsSync(path.join(node.dataDir, 'install.json')), false, `${manager}: nothing selected`);
      assert.equal(existsSync(unit), false, `${manager}: no definition`);
      assert.equal(running(supervisor), false, `${manager}: the supervisor is gone once the journal is`);
      for (const pid of new Set(node.said().map(line => line.pid))) assert.equal(running(pid), false, `${manager}: core ${pid} is gone`);
      assert.equal(await node.ask('node.status').then(() => true, () => false), false, `${manager}: nothing answers`);
      assert.doesNotMatch(node.log(), /stopping \(pid \d+\).*\b${supervisor}\b|signal.*itself/i);
      // The next start has nothing to start, and starts nothing.
      const again = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'service', 'start', '--json'], { env, encoding: 'utf8' });
      await wait(1500);
      assert.equal(await node.ask('node.status').then(() => true, () => false), false, `${manager}: still nothing (${again.stdout.trim()})`);
      assert.equal(existsSync(path.join(node.dataDir, 'install-txn.json')), false);
    } finally {
      node.stop();
      try { process.kill(Number(readFileSync(path.join(tools, 'state', 'pid'), 'utf8')), 'SIGKILL'); } catch {}
    }
  }
});

test('supervisor recovery to a plain installation: an upgrade from a connector with no service, committed and failing — rolled back outside the supervisor; the previous plain connector serves, no supervisor left', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, from, to, startAs, holder } = twoInstallations();
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-run-'));
  const journalFile = path.join(node.dataDir, 'install-txn.json');
  writeFileSync(journalFile, JSON.stringify({ ...JSON.parse(readFileSync(journalFile, 'utf8')), service: { kind: 'none', before: false, want: true } }), { mode: 0o600 });
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(to), { mode: 0o600 });
  writeFileSync(node.modesFile, ['ok+link:999', 'ok'].join('\n') + '\n');
  try {
    const supervisor = startAs(to);
    await until(() => !existsSync(journalFile), 60_000);
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
    await until(() => supervisor.exitCode !== null || supervisor.signalCode !== null, 10_000);
    const status = await node.status(s => s.state === 'running', 30_000);
    assert.deepEqual(status.command, from.command, 'the previous installation\'s connector serves');
    assert.equal(status.supervisor, false, 'as it was: plain, no supervisor');
    assert.ok(holder().includes(from.copy) && !holder().includes('--supervise'));
  } finally { node.stop(); }
});

test('detached hand-off: each installation\'s supervisor runs with its own settings — the candidate\'s on upgrade, the previous one\'s (a setting the candidate deleted included) on rollback, never the caller\'s', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const { node, from, to } = twoInstallations();
  const journalFile = path.join(node.dataDir, 'install-txn.json');
  const journal = JSON.parse(readFileSync(journalFile, 'utf8'));
  rmSync(journalFile);
  const home = path.dirname(node.dataDir), xdg = path.dirname(path.dirname(from.copy));
  const base = Object.fromEntries(Object.entries(node.env).filter(([name]) => name.startsWith('SIDEVOICE_')));
  from.settings = { ...base, SIDEVOICE_SHARED: 'from', SIDEVOICE_ONLY_FROM: 'from' };
  to.settings = { ...base, SIDEVOICE_SHARED: 'to' };   // the candidate deleted SIDEVOICE_ONLY_FROM
  writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(from), { mode: 0o600 });
  writeFileSync(node.modesFile, ['ok', 'slow:3000+link:999', 'ok'].join('\n') + '\n');
  const holder = () => { try { return JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8')).pid; } catch { return null; } };
  const settingsOf = pid => {
    const environ = Object.fromEntries(readFileSync(`/proc/${pid}/environ`, 'utf8').split('\0').filter(Boolean).map(line => [line.slice(0, line.indexOf('=')), line.slice(line.indexOf('=') + 1)]));
    return { command: readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0')[1], SHARED: environ.SIDEVOICE_SHARED, ONLY_FROM: environ.SIDEVOICE_ONLY_FROM, CALLER: environ.SIDEVOICE_CALLER_ONLY, SERVICE: environ.SIDEVOICE_SERVICE };
  };
  try {
    // Started by a caller whose environment has settings of its own: they are not the installation's.
    const caller = { ...node.env, HOME: home, XDG_DATA_HOME: xdg, SIDEVOICE_SHARED: 'caller', SIDEVOICE_CALLER_ONLY: 'caller' };
    const started = spawnSync(process.execPath, [from.command[1], 'service', 'start', '--json'], { env: caller, encoding: 'utf8' });
    assert.equal(started.status, 0, started.stdout + started.stderr);
    const first = await until(() => { const pid = holder(); return pid && settingsOf(pid).command === from.command[1] ? pid : null; });
    assert.deepEqual(settingsOf(first), { command: from.command[1], SHARED: 'from', ONLY_FROM: 'from', CALLER: undefined, SERVICE: 'none' }, 'started: the installation\'s settings');
    // The candidate committed (as an installer killed after its commit point leaves it): the supervisor hands over.
    writeFileSync(journalFile, JSON.stringify({ ...journal, from, to, service: { kind: 'none', before: true, want: true } }), { mode: 0o600 });
    writeFileSync(path.join(node.dataDir, 'install.json'), JSON.stringify(to), { mode: 0o600 });
    const upgraded = await until(() => { const pid = holder(); try { return pid && settingsOf(pid).command === to.command[1] ? pid : null; } catch { return null; } });
    assert.deepEqual(settingsOf(upgraded), { command: to.command[1], SHARED: 'to', ONLY_FROM: undefined, CALLER: undefined, SERVICE: 'none' }, 'upgrade: the candidate\'s settings, the deleted one gone');
    // Its core speaks another link protocol: rolled back, and the previous installation runs with its own again.
    await until(() => !existsSync(journalFile) && holder() !== upgraded && (() => { try { return settingsOf(holder()).command === from.command[1]; } catch { return false; } })(), 60_000);
    assert.deepEqual(settingsOf(holder()), { command: from.command[1], SHARED: 'from', ONLY_FROM: 'from', CALLER: undefined, SERVICE: 'none' }, 'rollback: the previous installation\'s settings, the deleted one back');
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
  } finally { node.stop(); }
});

test('installer rollback after the candidate spent its whole start budget: the previous installation starts with a budget of its own and runs; when it cannot either, that is said apart', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  for (const back of ['runs', 'fails']) {
    const tail = back === 'runs' ? ['ok'] : ['import', 'import', 'import', 'import', 'import', 'import'];
    const { node, env, from } = runningPlainMachine(['ok', 'import', 'import', 'import', 'import', 'import', ...tail]);
    try {
      const started = spawnSync(process.execPath, [from.command[1], 'service', 'start', '--json'], { env, encoding: 'utf8' });
      assert.equal(started.status, 0, started.stdout + started.stderr);
      await node.status(s => s.state === 'running' && s.supervisor);
      const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--no-agents', '--json'], { env: { ...env, SIDEVOICE_INSTALL_VERIFY_MS: '20000' }, encoding: 'utf8' });
      const answer = JSON.parse(run.stdout.trim().split('\n').at(-1));
      assert.equal(run.status, 1, `${back}: ${run.stdout}${run.stderr}`);
      assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'install.json'), 'utf8')).id, '0.5.0', `${back}: the previous installation is selected`);
      if (back === 'runs') {
        assert.equal(answer.error.key, 'install.rollback', run.stdout);
        const status = await node.ask('node.status');
        assert.deepEqual(status.command, from.command, 'the previous installation serves');
        assert.equal(status.state, 'running');
        assert.equal(status.attempts, 1, 'with its own budget: one start, not the candidate\'s five');
        assert.equal(existsSync(path.join(node.dataDir, 'install-txn.json')), false);
      } else {
        assert.equal(answer.error.key, 'install.rollback-failed', run.stdout);
        assert.match(answer.error.message, /0\.5\.0/);
        assert.ok(existsSync(path.join(node.dataDir, 'install-txn.json')), 'the journal stays until the selection is seen running');
      }
    } finally { node.stop(); }
  }
});

test('rollback at the crash point: the supervisor killed between spawning its core and recording it — the installer\'s rollback finds that core by its launch id and ends it before saying nothing runs', { skip: process.platform !== 'linux' && 'reads /proc' }, async () => {
  const node = supervisedNode({ modes: ['deaf'] });
  const home = path.dirname(node.dataDir);
  const armed = hooks({ pause: ['core-spawned'] });
  const env = { ...node.env, XDG_DATA_HOME: path.join(home, path.basename(node.dataDir) + '-xdg'), SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_INSTALL_VERIFY_MS: '5000', SIDEVOICE_TEST_HOOKS: armed.dir };
  const cores = () => {
    const found = [];
    for (const line of execFileSync('ps', ['-axo', 'pid=,stat=,command='], { encoding: 'utf8' }).split('\n')) {
      const match = line.trim().match(/^(\d+)\s+(\S+)\s+(.*)$/);
      if (match && !match[2].includes('Z') && match[3].includes(`--data-dir ${path.join(node.dataDir, 'core')}`)) found.push(Number(match[1]));
    }
    return found;
  };
  try {
    const installer = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--no-agents', '--json'], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = ''; installer.stdout.on('data', d => { out += d; });
    await armed.paused('core-spawned');
    const supervisor = Number(readFileSync(path.join(armed.dir, 'paused-core-spawned'), 'utf8'));
    const [core] = await until(() => { const found = cores(); return found.length ? found : null; });
    assert.equal(JSON.parse(readFileSync(path.join(node.dataDir, 'core-launch.json'), 'utf8')).pid, null, 'spawned, not recorded');
    process.kill(supervisor, 'SIGKILL');
    assert.equal(await exited(installer), 1, out);
    assert.equal(JSON.parse(out.trim().split('\n').at(-1)).error.key, 'install.rollback');
    assert.equal(existsSync(path.join(node.dataDir, 'install.json')), false);
    assert.equal(existsSync(path.join(node.dataDir, 'install-txn.json')), false, 'absence reported only once nothing runs');
    assert.deepEqual(cores(), [], `the starting core ${core} is gone`);
  } finally { node.stop(); }
});
