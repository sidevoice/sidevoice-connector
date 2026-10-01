/** The install transaction (§4.3): which candidate replaces which installation, an installer killed after each
 *  of its side effects and the machine put back together, two installers at once, and a rollback. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { decide, recover } from '../install-txn.mjs';
import { unitText } from '../service.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');

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
function fakeClaude(dir) {
  const bin = path.join(dir, 'claude'), entry = path.join(dir, 'claude-entry.txt');
  writeFileSync(bin, `#!/bin/sh
case "$2" in
  get) [ -s "${entry}" ] && cat "${entry}" || exit 1 ;;
  remove) rm -f "${entry}" ;;
  add) shift 6; cmd="$1"; shift; printf 'sidevoice:\\n  Scope: User config\\n  Type: stdio\\n  Command: %s\\n  Args: %s\\n' "$cmd" "$*" > "${entry}" ;;
esac
`, { mode: 0o755 });
  return { bin, line: () => { try { const text = readFileSync(entry, 'utf8'); return `${text.match(/Command: (.*)/)[1]} ${text.match(/Args: (.*)/)[1]}`; } catch { return null; } } };
}

/** A machine with an older installation (`0.5.0`): its copy, its unit (a stand-in systemd), its Claude Code and
 *  Cursor entries and `install.json`, all pointing at it — and this checkout's package as the candidate. */
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
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_CORE_BIN']) delete env[key];
  const version = JSON.parse(readFileSync(path.join(packageDir, 'package.json'), 'utf8')).version;
  const newCli = path.join(copies, version, 'dist', 'cli.mjs');
  /** Where every artifact points: the copy each one runs. */
  const artifacts = () => ({
    install: JSON.parse(readFileSync(path.join(dataDir, 'install.json'), 'utf8')).command[1],
    unit: existsSync(unit) ? readFileSync(unit, 'utf8').match(/^ExecStart="[^"]*" "([^"]*)"/m)[1] : null,
    claude: claude.line().split(' ')[1],
    cursor: JSON.parse(readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8')).mcpServers.sidevoice.args[0],
  });
  return { home, dataDir, env, from, oldCli, newCli, copies, artifacts, cursorOther: () => JSON.parse(readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8')).mcpServers.other };
}

const CRASH_POINTS = ['journal', 'copy', 'definition', 'register:claude', 'register:cursor', 'install.json'];

for (const point of CRASH_POINTS) {
  test(`install transaction: killed right after "${point}", then recovered — every artifact points at the selected installation`, async () => {
    const machine = upgradeMachine();
    const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--harness', 'claude', '--no-core'],
      { env: { ...machine.env, SIDEVOICE_TXN_CRASH_AFTER: point }, encoding: 'utf8' });
    assert.equal(run.signal, 'SIGKILL', `killed (${run.stderr})`);
    assert.ok(existsSync(path.join(machine.dataDir, 'install-txn.json')), 'the journal was written before any side effect');
    // Recovery: `to` only if install.json — the commit point — already names it.
    const outcome = await recover(machine.env);
    const selected = point === 'install.json' ? machine.newCli : machine.oldCli;
    assert.equal(outcome.recovered, point === 'install.json' ? path.basename(path.dirname(path.dirname(machine.newCli))) : '0.5.0');
    assert.deepEqual(machine.artifacts(), { install: selected, unit: selected, claude: selected, cursor: selected });
    assert.ok(existsSync(path.dirname(path.dirname(selected))), 'its copy is there');
    assert.ok(existsSync(path.dirname(path.dirname(machine.oldCli))), 'and the previous copy is never deleted by a transaction');
    assert.deepEqual(readdirSync(machine.copies).filter(name => name.endsWith('.staging')), [], 'no staging left');
    assert.equal(existsSync(path.join(machine.dataDir, 'install-txn.json')), false, 'the journal is gone');
    assert.deepEqual(machine.cursorOther(), { command: 'x' }, 'a foreign entry is untouched');
    // And the next install completes the upgrade from there.
    const again = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--harness', 'claude', '--no-core'], { env: machine.env, encoding: 'utf8' });
    assert.equal(again.status, 0, again.stderr);
    assert.deepEqual(machine.artifacts(), { install: machine.newCli, unit: machine.newCli, claude: machine.newCli, cursor: machine.newCli });
  });
}

/** This package built and copied somewhere else under another version (and channel/build), as a second
 *  installer — the app's bundled connector against npx's — would be. */
function packageAs(version, sidevoice = null) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-pkg-'));
  cpSync(path.join(packageDir, 'dist'), path.join(root, 'dist'), { recursive: true });
  const manifest = JSON.parse(readFileSync(path.join(packageDir, 'package.json'), 'utf8'));
  const stamped = { ...manifest, version, ...(sidevoice ? { sidevoice } : {}) };
  writeFileSync(path.join(root, 'package.json'), JSON.stringify(stamped));
  writeFileSync(path.join(root, 'dist', 'package.json'), JSON.stringify(stamped));
  return path.join(root, 'dist', 'cli.mjs');
}
const installWith = (cli, env) => new Promise(resolve => {
  const child = spawn(process.execPath, [cli, 'install', '--no-agents', '--no-core', '--json'], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', d => { out += d; });
  child.on('exit', code => resolve({ code, ...JSON.parse(out.trim().split('\n').at(-1)) }));
});

test('concurrent installers — the app\'s bundled connector and npx\'s — converge on the higher version, and neither undoes the other', async () => {
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-conc-'));
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: 'none' };
  const app = packageAs('0.6.0'), npx = packageAs('0.7.0');
  const [a, b] = await Promise.all([installWith(app, env), installWith(npx, env)]);
  assert.equal(a.ok && b.ok, true);
  const installed = JSON.parse(readFileSync(path.join(env.SIDEVOICE_DATA_DIR, 'install.json'), 'utf8'));
  assert.equal(installed.connector, '0.7.0');
  assert.ok(installed.command[1].includes(path.join('sidevoice', '0.7.0')));
  // Run either again: nothing changes.
  assert.equal((await installWith(app, env)).action, 'noop');
  assert.equal((await installWith(npx, env)).action, 'noop');
  // Same version, nightly: the higher build wins, whichever runs last, and an equal one is a no-op.
  const home2 = mkdtempSync(path.join(os.tmpdir(), 'sv-conc-'));
  const env2 = { ...env, HOME: home2, XDG_DATA_HOME: path.join(home2, 'xdg'), SIDEVOICE_DATA_DIR: path.join(home2, '.sidevoice') };
  const older = packageAs('0.6.0', { channel: 'nightly', build_seq: 10 }), newer = packageAs('0.6.0', { channel: 'nightly', build_seq: 12 });
  await Promise.all([installWith(older, env2), installWith(newer, env2)]);
  assert.equal(JSON.parse(readFileSync(path.join(env2.SIDEVOICE_DATA_DIR, 'install.json'), 'utf8')).build_seq, 12);
  assert.equal((await installWith(older, env2)).action, 'noop');
  assert.equal((await installWith(newer, env2)).action, 'noop');
  assert.ok(existsSync(path.join(env2.XDG_DATA_HOME, 'sidevoice', '0.6.0-nightly.12')), 'a nightly build is a copy of its own');
});

test('install transaction: an upgrade whose core does not come up is rolled back — install.json and every registration back on the previous one', async () => {
  const machine = upgradeMachine();
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-bin-'));
  const bin = path.join(tools, 'sidevoice-core');
  writeFileSync(bin, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  // No service here: the new core is started directly, and it fails to import.
  const unit = path.join(machine.home, '.config', 'systemd', 'user', 'sidevoice-node.service');
  const { rmSync } = await import('node:fs');
  rmSync(unit);
  const env = { ...machine.env, SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_CORE_BIN: bin, FAKE_CORE_MODE: 'import', SIDEVOICE_CORE_PORT: '0' };
  const run = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--harness', 'claude', '--no-service', '--json'], { env, encoding: 'utf8' });
  const result = JSON.parse(run.stdout.trim().split('\n').at(-1));
  assert.equal(run.status, 1);
  assert.equal(result.error.key, 'install.rollback');
  assert.equal(result.failure.key, 'import.missing-module');
  const installed = JSON.parse(readFileSync(path.join(machine.dataDir, 'install.json'), 'utf8'));
  assert.equal(installed.id, '0.5.0');
  const { install, claude, cursor } = machine.artifacts();
  assert.deepEqual({ install, claude, cursor }, { install: machine.oldCli, claude: machine.oldCli, cursor: machine.oldCli });
  assert.equal(existsSync(path.join(machine.dataDir, 'install-txn.json')), false);
  // A core self-test that fails stops before the commit: nothing changed at all.
  const failing = { ...env, FAKE_CORE_MODE: 'ok', FAKE_CORE_SELF_TEST_FAIL: '1' };
  const tested = spawnSync(process.execPath, [path.join(packageDir, 'cli.mjs'), 'install', '--harness', 'claude', '--no-service', '--json'], { env: failing, encoding: 'utf8' });
  assert.equal(JSON.parse(tested.stdout.trim().split('\n').at(-1)).error.key, 'import.missing-module');
  assert.equal(existsSync(path.join(machine.dataDir, 'install-txn.json')), false, 'no journal: nothing was committed');
  assert.equal(JSON.parse(readFileSync(path.join(machine.dataDir, 'install.json'), 'utf8')).id, '0.5.0');
});
