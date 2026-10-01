/** Installing, updating and rolling back (§2.4): which candidate replaces which release; an installer killed after each
 *  step of a switch, and the machine whole either way; installers that overlap; a release that does not run, flipped
 *  back to the previous one — by the installer, by the installer run again after it was killed, or by a person
 *  (`sidevoice rollback`). Real built packages, the fake core, a stand-in systemd; points crashed at
 *  (`testpoint.mjs`), never raced. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, realpathSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { decide } from '../release.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const fakeCore = path.join(here, 'fake-sidevoice-core.mjs');
const fakeManager = path.join(here, 'fake-service-manager.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

test('versions: higher replaces, lower never does, and the same version only as a later nightly build', () => {
  const at = (connector, channel = 'release', build_seq = 0) => ({ id: connector, connector, channel, build_seq });
  assert.equal(decide(null, at('0.6.0')), 'install');
  assert.equal(decide(at('0.6.0'), at('0.7.0')), 'upgrade');
  assert.equal(decide(at('0.7.0'), at('0.6.0')), 'noop', 'never a downgrade: the app and npx converge on the higher');
  assert.equal(decide(at('0.6.0'), at('0.6.0')), 'noop', 'a release\'s artifacts are immutable');
  assert.equal(decide(at('0.6.0', 'release', 7), at('0.6.0', 'release', 9)), 'noop', 'a release build number orders nothing');
  assert.equal(decide(at('0.6.0', 'nightly', 7), at('0.6.0', 'nightly', 9)), 'upgrade');
  assert.equal(decide(at('0.6.0', 'nightly', 9), at('0.6.0', 'nightly', 9)), 'noop');
  assert.equal(decide(at('0.6.0', 'nightly', 9), at('0.6.0', 'nightly', 7)), 'noop', 'two builds never replace each other in a loop');
  assert.equal(decide(at('0.6.10'), at('0.6.9')), 'noop', 'numerically, not as text');
  const source = { ...at('0.6.0', 'source'), id: '0.6.0-source', source: '/checkout' };
  assert.equal(decide(at('0.6.0'), source), 'upgrade', 'a checkout selects itself');
  assert.equal(decide(source, source), 'noop');
});

/** This package built for real — `build.mjs`, stamped as CI stamps it — and copied to stand as another package, as
 *  the app's bundled connector against npx's, or one version against the next. */
function builtAs(version, stamp = {}) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-pkg-'));
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { env: { ...process.env, ...stamp }, stdio: 'ignore' });
  cpSync(path.join(packageDir, 'dist'), path.join(root, 'dist'), { recursive: true });
  for (const manifest of [path.join(root, 'dist', 'package.json'), path.join(root, 'package.json')]) {
    const shipped = JSON.parse(readFileSync(path.join(root, 'dist', 'package.json'), 'utf8'));
    writeFileSync(manifest, JSON.stringify({ ...shipped, version }, null, 2));
  }
  execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'ignore' });   // the checkout's dist as it was
  return path.join(root, 'dist', 'cli.mjs');
}

/** A wrapper for the fake core whose mode is read from a file beside it at each start (a job carries only Sidevoice's
 *  own settings): `set(mode)` changes how the next start behaves. */
function coreWrapper(mode = 'ok') {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-bin-'));
  const bin = path.join(dir, 'sidevoice-core'), file = path.join(dir, 'mode');
  writeFileSync(file, mode);
  writeFileSync(bin, `#!/bin/sh\nFAKE_CORE_MODE=$(cat "${file}") exec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  return { bin, set: next => writeFileSync(file, next), toString: () => bin };
}

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

/** A machine for installing: its own HOME (Claude Code and Cursor in it), a stand-in systemd (or none). */
function machine(kind = 'systemd') {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-rel-'));
  const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-mgr-'));
  const state = path.join(tools, 'state');
  const manager = path.join(tools, 'systemctl');
  writeFileSync(manager, `#!/bin/sh\nFAKE_MANAGER_DIR="${state}" exec "${process.execPath}" "${fakeManager}" systemctl "$@"\n`, { mode: 0o755 });
  mkdirSync(path.join(home, '.cursor'));
  const claude = fakeClaude(home);
  const dataDir = path.join(home, '.sidevoice');
  const R = path.join(home, 'xdg', 'sidevoice');
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir,
    SIDEVOICE_CLAUDE_BIN: claude.bin, SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: kind, SIDEVOICE_SYSTEMCTL: manager, SIDEVOICE_LOGINCTL: '/bin/false',
    SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_VERIFY_MS: '15000', SIDEVOICE_TEARDOWN_MS: '3000', SIDEVOICE_SERVICE_START_WAIT_MS: '3000' };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_CORE_BIN', 'SIDEVOICE_TEST_HOOKS', 'FAKE_CORE_MODE', 'FAKE_CORE_MODES']) delete env[key];
  /** `<cli> <args…> --json` (a built package's, or this checkout's): its exit, its signal, and its one JSON line. */
  const run = (cli, args, more = {}) => {
    const ran = spawnSync(process.execPath, [cli, ...args, '--json'], { env: { ...env, ...more }, encoding: 'utf8' });
    let answer = null; try { answer = JSON.parse(ran.stdout.trim().split('\n').at(-1)); } catch {}
    return { status: ran.status, signal: ran.signal, answer, stderr: ran.stderr };
  };
  const selected = name => { try { return JSON.parse(readFileSync(path.join(R, name, 'release.json'), 'utf8')).connector; } catch { return null; } };
  const pid = job => { try { return Number(readFileSync(path.join(state, `sidevoice-${job}.service`, 'pid'), 'utf8')); } catch { return null; } };
  return {
    home, dataDir, R, env, claude, run, selected, pid,
    install: (cli, core = coreWrapper(), args = ['--harness', 'claude', '--service'], more = {}) => run(cli, ['install', ...args], { SIDEVOICE_CORE_BIN: String(core), ...more }),
    releases: () => readdirSync(path.join(R, 'releases')).sort(),
    status: () => run(path.join(packageDir, 'cli.mjs'), ['service', 'status']).answer,
    cursor: () => JSON.parse(readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8')).mcpServers?.sidevoice ?? null,
    stop() {
      for (const job of ['core', 'connector']) { try { process.kill(pid(job), 'SIGKILL'); } catch {} }
      try { process.kill(JSON.parse(readFileSync(path.join(dataDir, 'connector.lock'), 'utf8')).pid, 'SIGKILL'); } catch {}
      try { for (const line of readFileSync(path.join(dataDir, 'core', 'said.jsonl'), 'utf8').trim().split('\n')) { try { process.kill(JSON.parse(line).pid, 'SIGKILL'); } catch {} } } catch {}
    },
  };
}
/** A hooks directory with one crash point armed. */
function crashAt(point) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-hooks-'));
  writeFileSync(path.join(dir, `crash-${point}`), '');
  return dir;
}

test('concurrent installers — two genuinely built packages, the app\'s and npx\'s — converge on the higher version, and same-version nightlies on the higher build', async () => {
  const one = machine('none');
  const app = builtAs('0.6.0'), npx = builtAs('0.7.0');
  const both = await Promise.all([app, npx].map(cli => new Promise(resolve => {
    const child = spawn(process.execPath, [cli, 'install', '--no-agents', '--no-core', '--json'], { env: one.env, stdio: ['ignore', 'pipe', 'ignore'] });
    let out = ''; child.stdout.on('data', d => { out += d; });
    child.on('exit', code => resolve({ code, ...JSON.parse(out.trim().split('\n').at(-1)) }));
  })));
  assert.deepEqual(both.map(result => result.ok), [true, true]);
  assert.equal(one.selected('current'), '0.7.0');
  assert.equal(one.run(app, ['install', '--no-agents', '--no-core']).answer.action, 'noop');
  assert.equal(one.run(npx, ['install', '--no-agents', '--no-core']).answer.action, 'noop');
  assert.deepEqual(JSON.parse(readFileSync(path.join(one.dataDir, 'install.json'), 'utf8')), { command: [process.execPath, path.join(one.R, 'current', 'dist', 'cli.mjs')], releases: one.R, definitions: [] }, 'install.json: the stable command, and where the installation is');
  // Same version, stamped by the build as CI stamps a nightly: the higher run number wins, whichever runs last.
  const two = machine('none');
  const older = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '10' }), newer = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '12' });
  assert.deepEqual(JSON.parse(readFileSync(path.join(path.dirname(newer), 'package.json'), 'utf8')).sidevoice, { channel: 'nightly', build_seq: 12 }, 'stamped into the shipped manifest');
  await Promise.all([older, newer].map(cli => new Promise(resolve => spawn(process.execPath, [cli, 'install', '--no-agents', '--no-core', '--json'], { env: two.env, stdio: 'ignore' }).on('exit', resolve))));
  assert.equal(realpathSync(path.join(two.R, 'current')), path.join(realpathSync(two.R), 'releases', '0.6.0-nightly.12'), 'a nightly build is a release of its own');
  assert.equal(two.run(older, ['install', '--no-agents', '--no-core']).answer.action, 'noop');
  assert.equal(two.run(newer, ['install', '--no-agents', '--no-core']).answer.action, 'noop');
});

test('upgrade A→B runs, B→C cannot serve: back on B, restarted on it and verified; registrations through current never touched; a failed self-test changes nothing', async () => {
  const node = machine();
  const [a, b, c] = [builtAs('0.6.1'), builtAs('0.6.2'), builtAs('0.6.3')];
  try {
    const first = node.install(a);
    assert.equal(first.status, 0, JSON.stringify(first.answer) + first.stderr);
    assert.deepEqual([first.answer.action, first.answer.service, first.answer.state], ['install', 'systemd', 'running']);
    const registered = node.claude.line();
    assert.equal(registered, `node ${path.join(node.R, 'current', 'dist', 'cli.mjs')} mcp`, 'Claude Code runs whatever current selects');
    const upgraded = node.install(b);
    assert.equal(upgraded.answer.action, 'upgrade', JSON.stringify(upgraded.answer));
    assert.deepEqual([node.selected('current'), node.selected('previous')], ['0.6.2', '0.6.1']);
    // C's core fails before it is ready (an import): verified, flipped back, B restarted and verified again.
    const broken = node.install(c, coreWrapper('import'));
    assert.equal(broken.status, 1);
    assert.equal(broken.answer.error.key, 'install.rollback', JSON.stringify(broken.answer));
    assert.equal(broken.answer.failure.key, 'import.missing-module');
    assert.match(broken.answer.error.message, /0\.6\.3 failed.*back on 0\.6\.2/);
    assert.equal(node.selected('current'), '0.6.2');
    const after = node.status();
    assert.equal(after.state, 'running', JSON.stringify(after));
    assert.deepEqual(node.releases(), ['0.6.2'], 'the failed release, and the one before the previous, pruned');
    assert.equal(node.claude.line(), registered, 'the registration was never re-pointed');
    // A self-test that fails stops before the switch: nothing changes.
    const tested = node.install(c, coreWrapper('ok'), ['--harness', 'claude', '--service'], { FAKE_CORE_SELF_TEST_FAIL: '1' });
    assert.equal(tested.answer.error.key, 'import.missing-module');
    assert.equal(node.selected('current'), '0.6.2');
    assert.equal(node.status().state, 'running');
  } finally { node.stop(); }
});

test('a first install that cannot serve is left installed and failing, said with the core\'s key, and nothing spins; installing again once it can is the recovery', async () => {
  const node = machine();
  const cli = builtAs('0.6.1');
  const core = coreWrapper('import');
  try {
    const failed = node.install(cli, core);
    assert.equal(failed.status, 1);
    assert.deepEqual([failed.answer.error.key, failed.answer.failure.key], ['install.verify', 'import.missing-module']);
    assert.equal(node.selected('current'), '0.6.1', 'nothing to go back to: it stays selected');
    const status = node.status();
    assert.deepEqual([status.state, status.failure.key], ['failed', 'import.missing-module']);
    await wait(1000);
    assert.equal(alive(node.pid('core')), false, 'a failed start is not started again');
    // What was wrong is fixed (the core it runs can now import): installing again restarts and verifies the selection.
    core.set('ok');
    const again = node.install(cli, core);
    assert.deepEqual([again.status, again.answer.action, again.answer.state], [0, 'noop', 'running'], JSON.stringify(again.answer));
  } finally { node.stop(); }
});

for (const point of ['stage-copied', 'switch-previous', 'switch-current', 'definitions-written']) {
  test(`switch: the installer killed at "${point}" — current names a complete release, old or new, never none or half of one; the next install cleans up and finishes`, async () => {
    const node = machine();
    const [a, b] = [builtAs('0.6.1'), builtAs('0.6.2')];
    try {
      assert.equal(node.install(a).status, 0);
      const killed = node.install(b, coreWrapper(), ['--harness', 'claude', '--service'], { SIDEVOICE_TEST_HOOKS: crashAt(point) });
      assert.equal(killed.signal, 'SIGKILL', `killed at ${point} (${killed.stderr})`);
      const current = path.join(node.R, 'current');
      assert.ok(lstatSync(current).isSymbolicLink());
      assert.equal(node.selected('current'), ['stage-copied', 'switch-previous'].includes(point) ? '0.6.1' : '0.6.2');
      assert.ok(existsSync(path.join(current, 'dist', 'cli.mjs')) && existsSync(path.join(current, 'core', 'bin', 'sidevoice-core')), 'and that release is whole');
      assert.match(readlinkSync(current), /^releases\/0\.6\.[12]$/);
      // The lock went with the installer; the next one removes what it left by name, and finishes.
      const again = node.install(b);
      assert.equal(again.status, 0, JSON.stringify(again.answer) + again.stderr);
      assert.equal(node.selected('current'), '0.6.2');
      assert.equal(node.status().state, 'running');
      assert.deepEqual(readdirSync(node.R).filter(name => name.endsWith('.tmp')), []);
      assert.deepEqual(node.releases().filter(name => name.includes('.tmp-')), []);
    } finally { node.stop(); }
  });
}

test('rollback: `sidevoice rollback` goes back to previous and verifies it; an installer killed after selecting a release that cannot serve — the next install flips back', async () => {
  const node = machine();
  const [a, b, c] = [builtAs('0.6.1'), builtAs('0.6.2'), builtAs('0.6.3')];
  try {
    assert.equal(node.install(a).status, 0);
    assert.equal(node.install(b).status, 0);
    // A person goes back: current ← previous, the jobs restarted on it and verified.
    const back = node.run(path.join(node.R, 'current', 'dist', 'cli.mjs'), ['rollback']);
    assert.deepEqual([back.status, back.answer.installed, back.answer.from], [0, '0.6.1', '0.6.2'], JSON.stringify(back.answer));
    assert.equal(node.selected('current'), '0.6.1');
    assert.equal(node.status().state, 'running');
    assert.equal(node.run(path.join(node.R, 'current', 'dist', 'cli.mjs'), ['rollback']).answer.error.key, 'install.no-previous');
    // Forward again, then C selected and its installer killed before it could verify: C stays selected, and fails.
    assert.equal(node.install(b).status, 0);
    const killed = node.install(c, coreWrapper('import'), ['--harness', 'claude', '--service'], { SIDEVOICE_TEST_HOOKS: crashAt('definitions-written') });
    assert.equal(killed.signal, 'SIGKILL');
    assert.equal(node.selected('current'), '0.6.3');
    // Installing again is the recovery: the same release is no upgrade, so the selection is verified — it does not run,
    // and current goes back to what ran before.
    const recovered = node.install(c, coreWrapper('import'));
    assert.equal(recovered.answer.error.key, 'install.rollback', JSON.stringify(recovered.answer));
    assert.equal(node.selected('current'), '0.6.2');
    assert.equal(node.status().state, 'running');
  } finally { node.stop(); }
});

test('foreign entries named sidevoice — even another program called cli.mjs — are never replaced or removed; ours from before are re-pointed once, through current', async () => {
  const node = machine('none');
  execFileSync(node.claude.bin, ['mcp', 'add', '--scope', 'user', 'sidevoice', '--', 'node', '/opt/unrelated/cli.mjs', 'mcp']);
  writeFileSync(path.join(node.home, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { other: { command: 'x' }, sidevoice: { command: 'node', args: ['/opt/unrelated/cli.mjs', 'mcp'] } } }));
  const cli = path.join(packageDir, 'cli.mjs');
  assert.equal(node.run(cli, ['install', '--harness', 'claude', '--harness', 'cursor', '--no-core'], { SIDEVOICE_INSTALL_FROM_SOURCE: '1' }).status, 0);
  assert.equal(node.claude.line(), 'node /opt/unrelated/cli.mjs mcp');
  assert.deepEqual(node.cursor(), { command: 'node', args: ['/opt/unrelated/cli.mjs', 'mcp'] });
  node.run(cli, ['uninstall', '--harness', 'claude']);
  assert.equal(node.claude.line(), 'node /opt/unrelated/cli.mjs mcp', 'uninstall leaves it too');
  // Ours from an earlier version (a copy under R) is re-pointed once, through current; then never again.
  const old = path.join(node.R, '0.5.0', 'dist', 'cli.mjs');
  writeFileSync(path.join(node.home, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { other: { command: 'x' }, sidevoice: { command: 'node', args: [old, 'mcp'] } } }));
  assert.equal(node.run(cli, ['install', '--no-agents', '--no-core'], { SIDEVOICE_INSTALL_FROM_SOURCE: '1' }).status, 0);
  assert.deepEqual(node.cursor(), { command: 'node', args: [path.join(node.R, 'current', 'dist', 'cli.mjs'), 'mcp'] });
  assert.equal(JSON.parse(readFileSync(path.join(node.home, '.cursor', 'mcp.json'), 'utf8')).mcpServers.other.command, 'x');
});

/* ----- review blockers (R1-b-rebuild-astra.md), one regression each ----- */

test('blocker 3: a full uninstall keeps the stop — a connector already on its way does not serve after it; an explicit install or start clears it', async () => {
  const node = machine('none');
  const cli = path.join(packageDir, 'cli.mjs'), core = coreWrapper();
  const hooks = mkdtempSync(path.join(os.tmpdir(), 'sv-hooks-'));
  writeFileSync(path.join(hooks, 'pause-lock-before-connector'), '');
  let child = null;
  try {
    assert.equal(node.install(cli, core, ['--no-agents', '--no-core'], { SIDEVOICE_INSTALL_FROM_SOURCE: '1' }).status, 0);
    child = spawn(process.execPath, [cli, 'connector'], { env: { ...node.env, SIDEVOICE_TEST_HOOKS: hooks, SIDEVOICE_CORE_BIN: core.bin, SIDEVOICE_CONNECTOR_IDLE_MS: '60000' }, stdio: 'ignore' });
    for (let i = 0; i < 300 && !existsSync(path.join(hooks, 'paused-lock-before-connector')); i++) await wait(20);
    const removed = node.run(cli, ['uninstall']);
    assert.equal(removed.status, 0, JSON.stringify(removed.answer));
    assert.deepEqual(readdirSync(node.dataDir).sort(), ['install.lock', 'node-stopped.json'], 'the stop stays, with the permanent lock');
    writeFileSync(path.join(hooks, 'resume-lock-before-connector'), '');
    const code = await new Promise(resolve => (child.exitCode !== null ? resolve(child.exitCode) : child.once('exit', resolve)));
    assert.equal(code, 0, 'the connector on its way finds the stop and leaves');
    assert.equal(existsSync(path.join(node.dataDir, 'connector.sock')), false, 'nothing serves');
    assert.equal(existsSync(path.join(node.dataDir, 'core', 'core.json')), false, 'and no core was started');
    // An explicit install is a person's start.
    assert.equal(node.install(cli, core, ['--no-agents', '--no-core'], { SIDEVOICE_INSTALL_FROM_SOURCE: '1' }).status, 0);
    assert.equal(existsSync(path.join(node.dataDir, 'node-stopped.json')), false);
  } finally { child?.kill('SIGKILL'); node.stop(); }
});
