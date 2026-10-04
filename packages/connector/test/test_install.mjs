/** Installing, updating and rolling back (§2.4): which candidate replaces which release; an installer killed after each
 *  step of a switch, and the machine whole either way; installers that overlap; a release that does not run, flipped
 *  back to the previous one — by the installer, by the installer run again after it was killed, or by a person
 *  (`sidevoice rollback`). Real built packages, the fake core, a stand-in systemd; points crashed at
 *  (`testpoint.mjs`), never raced. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { createHash } from 'node:crypto';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { chmodSync, cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { decide, point, releaseLayout } from '../release.mjs';
import { RUST_CORE_ENTRYPOINT, RUST_CORE_KIND, rustCoreTarget } from '../rust-core.mjs';
import { apply, rollback } from '../install.mjs';
import { connectorClient } from '../ipc.mjs';
import { launch } from '../launcher.mjs';
import { launch as legacyLaunch } from './fixtures/legacy-launcher-97940ed.mjs';
import { nodeStopped, runtimeSwitching } from '../node-files.mjs';
import { writePrivateFile } from '../secure-fs.mjs';

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
  assert.equal(decide({ ...at('0.6.0', 'nightly', 20), core_build: 'core-new' },
    { ...at('0.6.0', 'nightly', 19), core_build: 'core-old' }), 'noop', 'a changed Core identity cannot replace a newer nightly');
  assert.equal(decide({ ...at('0.6.0', 'nightly', 100), format: 'esm' }, { ...at('0.6.0', 'nightly', 1), format: 'sea' }), 'noop', 'format preference cannot replace a later nightly with an older build');
  assert.equal(decide({ ...at('0.6.0', 'nightly', 1), format: 'sea' }, { ...at('0.6.0', 'nightly', 100), format: 'esm' }), 'upgrade', 'the next nightly build wins even when it uses ESM');
  assert.equal(decide({ ...at('0.6.0', 'nightly', 100), format: 'esm' }, { ...at('0.6.0', 'nightly', 100), format: 'sea' }), 'upgrade', 'SEA is preferred only at the same nightly build');
  assert.equal(decide({ ...at('0.6.0', 'nightly', 100), format: 'esm' }, { ...at('0.6.0', 'release', 0), format: 'sea' }), 'noop', 'a release-format preference cannot cross from a newer nightly');
  assert.equal(decide({ ...at('0.6.0', 'release', 0), format: 'sea' }, { ...at('0.6.0', 'nightly', 100), format: 'esm' }), 'upgrade', 'a later nightly sequence wins across formats');
  assert.equal(decide({ ...at('0.6.0'), runtime_kind: 'javascript' }, { ...at('0.6.0'), runtime_kind: 'rust-native-v1' }), 'upgrade',
    'an equal package version cannot hide a daemon runtime switch');
  assert.equal(decide(at('0.6.0'), { ...at('0.6.0'), runtime_kind: 'rust-native-v1' }), 'upgrade',
    'legacy release metadata is treated as the JavaScript daemon');
  assert.equal(decide({ ...at('0.6.0'), runtime_kind: 'rust-native-v1' }, { ...at('0.6.0'), runtime_kind: 'javascript' }), 'upgrade',
    'runtime changes are bidirectional during verified rollback');
  assert.equal(decide(at('0.6.10'), at('0.6.9')), 'noop', 'numerically, not as text');
  const source = { ...at('0.6.0', 'source'), id: '0.6.0-source', source: '/checkout' };
  assert.equal(decide(at('0.6.0'), source), 'upgrade', 'a checkout selects itself');
  assert.equal(decide(source, source), 'noop');
});

test('a standalone facade starts its own SEA despite an unrelated selected release', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-standalone-launch-'));
  const data = path.join(home, 'other-machine'), marker = path.join(home, 'launched');
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'),
    SIDEVOICE_DATA_DIR: data, SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_LAUNCH_MARKER: marker };
  mkdirSync(data, { recursive: true, mode: 0o700 });
  const layout = releaseLayout(env), id = 'unrelated';
  mkdirSync(path.join(layout.releases, id), { recursive: true, mode: 0o700 });
  writePrivateFile(path.join(layout.releases, id, 'release.json'), JSON.stringify({ id, format: 'sea' }));
  point(env, 'current', id);
  const self = path.join(home, 'standalone.mjs');
  writeFileSync(self, 'import { writeFileSync } from "node:fs"; writeFileSync(process.env.SIDEVOICE_LAUNCH_MARKER, "launched");\n');
  t.after(() => rmSync(home, { recursive: true, force: true }));
  const connected = await launch({ connect: async () => { if (existsSync(marker)) return true; throw new Error('not up'); },
    self: [process.execPath, self], env });
  assert.equal(connected, true);
  assert.equal(readFileSync(marker, 'utf8'), 'launched');
});

test('runtime switch refuses a queued or unreadable outbox before staging or moving the release pointer', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-runtime-switch-outbox-'));
  const data = path.join(home, '.sidevoice');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_INSTALL_FROM_SOURCE: '0' };
  mkdirSync(data, { recursive: true, mode: 0o700 });
  const layout = releaseLayout(env), oldId = 'legacy-javascript';
  const oldDir = path.join(layout.releases, oldId);
  mkdirSync(oldDir, { recursive: true, mode: 0o700 });
  const oldRuntime = 'a'.repeat(64), oldCore = 'legacy-core';
  const oldRelease = { id: oldId, connector: '0.6.0', core: '0.1.0', core_build: oldCore,
    channel: 'release', build_seq: 0, format: 'sea', runtime_kind: 'javascript', runtime_sha256: oldRuntime,
    pair_id: `pair-v1:javascript:${oldRuntime}:core:${oldCore}` };
  writePrivateFile(path.join(oldDir, 'release.json'), `${JSON.stringify(oldRelease)}\n`);
  point(env, 'current', oldId);
  const next = { ...oldRelease, id: 'native-rust-pair', runtime_kind: 'rust-native-v1',
    pair_id: `pair-v1:rust-native-v1:${'b'.repeat(64)}:core:${'c'.repeat(40)}` };
  const outbox = path.join(data, 'outbox.json');
  writePrivateFile(outbox, '[{"event_id":"pending"}]\n');
  t.after(() => rmSync(home, { recursive: true, force: true }));

  await assert.rejects(apply(env, { core: false, candidateRelease: next }), error => error.key === 'install.runtime-switch-outbox');
  assert.equal(readlinkSync(layout.current), path.join('releases', oldId));
  assert.equal(existsSync(path.join(layout.releases, next.id)), false, 'the candidate was not staged');
  assert.equal(existsSync(path.join(data, 'service')), false, 'service state was not changed');

  rmSync(outbox);
  writePrivateFile(outbox, '{malformed\n');
  await assert.rejects(apply(env, { core: false, candidateRelease: next }), error => error.key === 'install.runtime-switch-outbox');
  assert.equal(readlinkSync(layout.current), path.join('releases', oldId));
  assert.equal(existsSync(path.join(layout.releases, next.id)), false, 'an unreadable queue still refuses before staging');

  // The initial pre-stage read is empty. A writer adds speech while staging is paused, so the
  // post-quiescence decision must still refuse without selecting the other runtime.
  rmSync(outbox);
  const staged = { ...next, id: `${next.id}-nocore` };
  mkdirSync(path.join(layout.releases, staged.id), { mode: 0o700 });
  writePrivateFile(path.join(layout.releases, staged.id, 'release.json'), `${JSON.stringify(staged)}\n`);
  const hooks = mkdtempSync(path.join(os.tmpdir(), 'sv-switch-hooks-'));
  writeFileSync(path.join(hooks, 'pause-install-after-quiesce'), '');
  t.after(() => rmSync(hooks, { recursive: true, force: true }));
  const source = `import { apply } from ${JSON.stringify(new URL('../install.mjs', import.meta.url).href)};
    try { await apply(process.env, { core: false, candidateRelease: JSON.parse(process.env.SIDEVOICE_TEST_CANDIDATE) }); process.exitCode = 0; }
    catch (error) { console.log(error.key); process.exitCode = 1; }`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], { env: { ...env, SIDEVOICE_TEST_HOOKS: hooks,
    SIDEVOICE_TEST_CANDIDATE: JSON.stringify(next) }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '', errors = '';
  child.stdout.on('data', chunk => { output += chunk; });
  child.stderr.on('data', chunk => { errors += chunk; });
  const facade = connectorClient(env, { self: [process.execPath, path.join(packageDir, 'cli.mjs')] });
  for (let i = 0; i < 200 && !existsSync(path.join(hooks, 'paused-install-after-quiesce')); i++) await wait(25);
  assert.equal(existsSync(path.join(hooks, 'paused-install-after-quiesce')), true,
    `installer reached the final handoff: ${output} ${errors}`);
  assert.equal(runtimeSwitching(data), true, 'the install lock owns a live launch barrier');
  await assert.rejects(facade.ensure(), error => error.key === 'install.runtime-switching',
  'an existing MCP facade cannot respawn the old daemon after quiescence');
  await assert.rejects(legacyLaunch({ connect: async () => { throw new Error('socket closed'); },
    self: [process.execPath, path.join(packageDir, 'cli.mjs')], env }), error => error.key === 'node.stopped',
  'the already-running baseline facade recognizes the compatibility stop marker');
  assert.equal(existsSync(outbox), false, 'a refused reconnect cannot enqueue after quiescence');
  writePrivateFile(outbox, '[{"event_id":"arrived-during-stage"}]\n');
  writeFileSync(path.join(hooks, 'resume-install-after-quiesce'), '');
  assert.equal(await new Promise(resolve => child.on('exit', resolve)), 1);
  assert.match(output, /install.runtime-switch-outbox/);
  assert.equal(readlinkSync(layout.current), path.join('releases', oldId));
  assert.equal(runtimeSwitching(data), false, 'refusal releases only the transaction launch barrier');

  // A failed candidate may write its own durable row after selection. Recovery keeps that row
  // available for diagnosis while the previous runtime starts with no shared outbox to replay.
  rmSync(outbox);
  point(env, 'verified', oldId);
  mkdirSync(path.join(layout.releases, next.id), { mode: 0o700 });
  writePrivateFile(path.join(layout.releases, next.id, 'release.json'), `${JSON.stringify(next)}\n`);
  rmSync(path.join(hooks, 'pause-install-after-quiesce'));
  writeFileSync(path.join(hooks, 'pause-install-after-commit'), '');
  const recover = `import { apply } from ${JSON.stringify(new URL('../install.mjs', import.meta.url).href)};
    const result = await apply(process.env, { candidateRelease: JSON.parse(process.env.SIDEVOICE_TEST_CANDIDATE) });
    console.log(JSON.stringify({ action: result.action, quarantined: result.quarantined }));`;
  const failed = spawn(process.execPath, ['--input-type=module', '-e', recover], { env: { ...env,
    SIDEVOICE_TEST_HOOKS: hooks, SIDEVOICE_TEST_CANDIDATE: JSON.stringify(next), SIDEVOICE_INSTALL_VERIFY_MS: '1000' },
  stdio: ['ignore', 'pipe', 'pipe'] });
  let recovered = '', recoverErrors = '';
  failed.stdout.on('data', chunk => { recovered += chunk; });
  failed.stderr.on('data', chunk => { recoverErrors += chunk; });
  for (let i = 0; i < 200 && !existsSync(path.join(hooks, 'paused-install-after-commit')); i++) await wait(25);
  assert.equal(existsSync(path.join(hooks, 'paused-install-after-commit')), true,
    `failed candidate reached the selected state: ${recovered} ${recoverErrors}`);
  writePrivateFile(outbox, '[{"event_id":"queued-by-failed-rust"}]\n');
  writeFileSync(path.join(hooks, 'resume-install-after-commit'), '');
  assert.equal(await new Promise(resolve => failed.on('exit', resolve)), 0, recoverErrors);
  const recovery = JSON.parse(recovered.trim());
  assert.equal(recovery.action, 'rollback');
  assert.equal(readlinkSync(layout.current), path.join('releases', oldId));
  assert.equal(existsSync(outbox), false, 'the previous JS runtime cannot replay the failed Rust row');
  assert.match(readFileSync(recovery.quarantined, 'utf8'), /queued-by-failed-rust/);
  assert.equal(existsSync(path.join(data, 'node-stopped.json')), false,
    'the selected baseline JS daemon must not inherit the Rust compatibility gate');
  const legacyStarted = path.join(home, 'legacy-started');
  const legacySelf = path.join(home, 'legacy-self.mjs');
  writeFileSync(legacySelf, `import { writeFileSync } from 'node:fs'; writeFileSync(${JSON.stringify(legacyStarted)}, 'started');\n`);
  assert.equal(await legacyLaunch({ connect: async () => {
    if (existsSync(legacyStarted)) return true;
    throw new Error('socket closed');
  }, self: [process.execPath, legacySelf], env }), true,
  'the actual base launcher can start the selected JS runtime after unmanaged rollback');
});

test('explicit rollback refuses queued speech in either cross-runtime direction', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-rollback-outbox-'));
  const data = path.join(home, '.sidevoice');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data, XDG_DATA_HOME: path.join(home, 'xdg'),
    XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none' };
  mkdirSync(data, { recursive: true, mode: 0o700 });
  const layout = releaseLayout(env);
  for (const [id, runtime_kind] of [['js', 'javascript'], ['rust', 'rust-native-v1']]) {
    const directory = path.join(layout.releases, id);
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    writePrivateFile(path.join(directory, 'release.json'), `${JSON.stringify({ id, runtime_kind })}\n`);
  }
  t.after(() => rmSync(home, { recursive: true, force: true }));
  writePrivateFile(path.join(data, 'outbox.json'), '[{"event_id":"queued"}]\n');
  const humanStop = '{"at":"before-rollback"}\n';
  writePrivateFile(path.join(data, 'node-stopped.json'), humanStop);
  for (const [current, previous] of [['js', 'rust'], ['rust', 'js']]) {
    point(env, 'current', current);
    point(env, 'previous', previous);
    await assert.rejects(rollback(env), error => error.key === 'install.runtime-switch-outbox');
    assert.equal(readlinkSync(layout.current), path.join('releases', current));
    assert.equal(readFileSync(path.join(data, 'node-stopped.json'), 'utf8'), humanStop,
      'a refused rollback preserves the original human stop record');
  }
});

test('a pre-lock JS daemon cannot serve after current switches to Rust', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-delayed-js-owner-'));
  const data = path.join(home, '.sidevoice'), hooks = path.join(home, 'hooks');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_TEST_HOOKS: hooks };
  mkdirSync(data, { recursive: true, mode: 0o700 }); mkdirSync(hooks);
  const layout = releaseLayout(env), oldId = 'old-js', rustId = 'next-rust';
  const oldRelease = { id: oldId, connector: '0.6.0', channel: 'release', build_seq: 0,
    format: 'esm', runtime_kind: 'javascript', core_build: null };
  const next = { ...oldRelease, id: rustId, runtime_kind: 'rust-native-v1', format: 'sea',
    pair_id: 'pair-for-delayed-start' };
  for (const release of [oldRelease, { ...next, id: `${rustId}-nocore` }]) {
    const directory = path.join(layout.releases, release.id);
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    writePrivateFile(path.join(directory, 'release.json'), `${JSON.stringify(release)}\n`);
  }
  point(env, 'current', oldId);
  writePrivateFile(path.join(data, 'install.json'), JSON.stringify({ releases: layout.root,
    command: [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')], definitions: [] }));
  writeFileSync(path.join(hooks, 'pause-lock-before-connector'), '');
  const old = spawn(process.execPath, [path.join(packageDir, 'cli.mjs'), 'connector'], { env,
    stdio: ['ignore', 'pipe', 'pipe'] });
  t.after(() => { old.kill('SIGKILL'); rmSync(home, { recursive: true, force: true }); });
  for (let i = 0; i < 200 && !existsSync(path.join(hooks, 'paused-lock-before-connector')); i++) await wait(25);
  assert.equal(existsSync(path.join(hooks, 'paused-lock-before-connector')), true);
  // A process already past startup may wait here while a managed installer changes selection.
  point(env, 'current', `${rustId}-nocore`);
  assert.equal(readlinkSync(layout.current), path.join('releases', `${rustId}-nocore`));
  assert.equal(existsSync(path.join(data, 'node-stopped.json')), false,
    'the release fence must work even after a service start clears stop intent');
  writeFileSync(path.join(hooks, 'resume-lock-before-connector'), '');
  assert.equal(await new Promise(resolve => old.on('exit', resolve)), 0);
  assert.equal(existsSync(path.join(data, 'connector.sock')), false, 'stale JS did not bind the socket');
  assert.equal(existsSync(path.join(data, 'outbox.json')), false, 'stale JS did not load or write speech');
});

test('an installed unmanaged JS profile refuses Rust promotion while a base facade may still run', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-unmanaged-legacy-promotion-'));
  const data = path.join(home, '.sidevoice'), marker = path.join(home, 'legacy-launched');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none' };
  mkdirSync(data, { recursive: true, mode: 0o700 });
  const layout = releaseLayout(env), old = { id: 'base-js', connector: '0.6.0', runtime_kind: 'javascript' };
  mkdirSync(path.join(layout.releases, old.id), { recursive: true, mode: 0o700 });
  writePrivateFile(path.join(layout.releases, old.id, 'release.json'), JSON.stringify(old));
  point(env, 'current', old.id);
  const installRecord = JSON.stringify({ releases: layout.root,
    command: [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')], definitions: [] });
  writePrivateFile(path.join(data, 'install.json'), installRecord);
  const outbox = path.join(data, 'outbox.json'), rows = '[]\n';
  writePrivateFile(outbox, rows);
  const selected = { ...old, id: 'next-rust', runtime_kind: 'rust-native-v1', format: 'sea' };
  t.after(() => rmSync(home, { recursive: true, force: true }));
  await assert.rejects(apply(env, { core: false, candidateRelease: selected }),
    error => error.key === 'install.runtime-switch-service-required');
  assert.equal(readlinkSync(layout.current), path.join('releases', old.id));
  assert.equal(readFileSync(outbox, 'utf8'), rows);
  assert.equal(readFileSync(path.join(data, 'install.json'), 'utf8'), installRecord);
  assert.equal(existsSync(path.join(data, 'node-stopped.json')), false);
  assert.equal(existsSync(path.join(layout.releases, `${selected.id}-nocore`)), false);
  const self = path.join(home, 'legacy-self.mjs');
  writeFileSync(self, `import { writeFileSync } from 'node:fs'; writeFileSync(${JSON.stringify(marker)}, 'started');\n`);
  assert.equal(await legacyLaunch({ connect: async () => {
    if (existsSync(marker)) return true;
    throw new Error('socket closed');
  }, self: [process.execPath, self], env }), true,
  'the existing base facade still starts its selected JS path after refusal');
});

test('a switch-current crash and noop retry retain the old-base fence under Rust selection', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-rust-switch-crash-'));
  const data = path.join(home, '.sidevoice'), hooks = path.join(home, 'hooks');
  const manager = path.join(home, 'systemctl');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'systemd', SIDEVOICE_SYSTEMCTL: manager, SIDEVOICE_TEST_HOOKS: hooks };
  mkdirSync(data, { recursive: true, mode: 0o700 }); mkdirSync(hooks);
  writeFileSync(manager, '#!/bin/sh\ncase "$*" in *show*) printf "LoadState=not-found\\nActiveState=inactive\\n" ;; esac\n', { mode: 0o755 });
  const definition = path.join(env.XDG_CONFIG_HOME, 'systemd', 'user', 'sidevoice-connector.service');
  mkdirSync(path.dirname(definition), { recursive: true, mode: 0o700 });
  writeFileSync(definition, '[Service]\nExecStart=/bin/true\n');
  const layout = releaseLayout(env), old = { id: 'old-js', connector: '0.6.0', runtime_kind: 'javascript', format: 'esm' };
  const next = { ...old, id: 'selected-rust', runtime_kind: 'rust-native-v1', format: 'sea' };
  for (const release of [old, { ...next, id: `${next.id}-nocore` }]) {
    const directory = path.join(layout.releases, release.id);
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    writePrivateFile(path.join(directory, 'release.json'), JSON.stringify(release));
  }
  point(env, 'current', old.id);
  writePrivateFile(path.join(data, 'install.json'), JSON.stringify({ releases: layout.root,
    command: [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')], definitions: [definition] }));
  writeFileSync(path.join(hooks, 'crash-switch-current'), '');
  t.after(() => rmSync(home, { recursive: true, force: true }));
  const source = `import { apply } from ${JSON.stringify(new URL('../install.mjs', import.meta.url).href)};
    await apply(process.env, { core: false, candidateRelease: JSON.parse(process.env.SIDEVOICE_TEST_CANDIDATE) });`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], { env: {
    ...env, SIDEVOICE_TEST_CANDIDATE: JSON.stringify(next) }, stdio: ['ignore', 'pipe', 'pipe'] });
  const signal = await new Promise(resolve => child.on('exit', (_, exitSignal) => resolve(exitSignal)));
  assert.equal(signal, 'SIGKILL');
  assert.equal(readlinkSync(layout.current), path.join('releases', `${next.id}-nocore`));
  assert.equal(existsSync(path.join(data, 'node-stopped.json')), true);
  rmSync(path.join(hooks, 'crash-switch-current'));
  const retried = await apply(env, { core: false, candidateRelease: next });
  assert.equal(retried.action, 'noop');
  assert.equal(nodeStopped(data, `${next.id}-nocore`), false, 'selected Rust may start');
  await assert.rejects(legacyLaunch({ connect: async () => { throw new Error('socket closed'); },
    self: [process.execPath, path.join(packageDir, 'cli.mjs')], env }), error => error.key === 'node.stopped',
  'the frozen base facade still cannot start its old JS daemon');
});

test('noop Rust install --service without a manager leaves on-demand launch available', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-rust-no-manager-'));
  const data = path.join(home, '.sidevoice'), hooks = path.join(home, 'hooks'), marker = path.join(home, 'launched');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_TEST_HOOKS: hooks, SIDEVOICE_LAUNCH_MARKER: marker };
  mkdirSync(data, { recursive: true, mode: 0o700 }); mkdirSync(hooks);
  const layout = releaseLayout(env), id = 'selected-rust', target = rustCoreTarget();
  const sourceSha = 'a'.repeat(40), archiveSha = 'b'.repeat(64), coreBuild = `rust-native-v1-${target}-${sourceSha}-${archiveSha}`;
  const executable = path.join(layout.releases, id, 'dist', 'sidevoice-rust');
  mkdirSync(path.dirname(executable), { recursive: true, mode: 0o700 });
  const bytes = `#!/bin/sh\necho launched > "${marker}"\n`;
  writeFileSync(executable, bytes); chmodSync(executable, 0o700);
  const sha = createHash('sha256').update(bytes).digest('hex');
  const release = { id, connector: '0.6.0', core: '0.1.0', channel: 'release', build_seq: 0,
    format: 'sea', runtime_kind: 'rust-native-v1', runtime_target: target, runtime_build_sha: sourceSha,
    runtime_sha256: sha, runtime_size: Buffer.byteLength(bytes), core_kind: 'rust-native-v1',
    core_target: target, core_source_sha: sourceSha, core_archive_sha256: archiveSha,
    core_cargo_lock_sha256: 'c'.repeat(64), core_manifest_sha256: 'd'.repeat(64), core_archive_size: 1,
    core_entrypoint: RUST_CORE_ENTRYPOINT, core_build: coreBuild,
    pair_id: `pair-v1:rust-native-v1:${sha}:core:${coreBuild}` };
  writePrivateFile(path.join(layout.releases, id, 'release.json'), `${JSON.stringify(release)}\n`);
  const coreExecutable = path.join(layout.releases, id, 'core', RUST_CORE_ENTRYPOINT);
  mkdirSync(path.dirname(coreExecutable), { recursive: true, mode: 0o700 });
  writeFileSync(coreExecutable, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`);
  chmodSync(coreExecutable, 0o700);
  point(env, 'current', id);
  point(env, 'verified', id);
  writePrivateFile(path.join(data, 'install.json'), JSON.stringify({ releases: layout.root,
    command: [path.join(layout.current, 'dist', 'sidevoice')], definitions: [] }));
  writeFileSync(path.join(hooks, 'pause-install-after-quiesce'), '');
  writeFileSync(path.join(hooks, 'pause-install-after-commit'), '');
  const identity = { version: release.connector, runtime_kind: release.runtime_kind,
    runtime_build_sha: release.runtime_build_sha, runtime_sha256: release.runtime_sha256,
    runtime_target: release.runtime_target, release_id: release.id, executable: realpathSync(executable),
    managed: false, pid: process.pid };
  const server = net.createServer(socket => socket.on('data', chunk => {
    const request = JSON.parse(String(chunk).split('\n')[0]);
    socket.end(JSON.stringify({ id: request.id, ok: true, result: request.method === 'identity'
      ? identity : { version: release.connector } }) + '\n');
  }));
  await new Promise(resolve => server.listen(path.join(data, 'connector.sock'), resolve));
  chmodSync(path.join(data, 'connector.sock'), 0o600);
  const source = `import { apply } from ${JSON.stringify(new URL('../install.mjs', import.meta.url).href)};
    const result = await apply(process.env, { service: true, candidateRelease: JSON.parse(process.env.SIDEVOICE_TEST_CANDIDATE) });
    console.log(result.action);`;
  const installer = spawn(process.execPath, ['--input-type=module', '-e', source], { env: { ...env,
    SIDEVOICE_TEST_CANDIDATE: JSON.stringify(release) }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '', errors = '';
  installer.stdout.on('data', chunk => { output += chunk; });
  installer.stderr.on('data', chunk => { errors += chunk; });
  t.after(() => { installer.kill('SIGKILL'); server.close();
    try { const ready = JSON.parse(readFileSync(path.join(data, 'core', 'core.json'), 'utf8')); process.kill(ready.pid, 'SIGTERM'); } catch {}
    rmSync(home, { recursive: true, force: true }); });
  for (let i = 0; i < 200 && !existsSync(path.join(hooks, 'paused-install-after-commit')); i++) await wait(25);
  assert.equal(existsSync(path.join(hooks, 'paused-install-after-commit')), true,
    'a manager-none noop reaches verification without quiescing the same release');
  assert.equal(existsSync(path.join(hooks, 'paused-install-after-quiesce')), false);
  assert.equal(runtimeSwitching(data), false);
  assert.equal(nodeStopped(data, id), false);
  assert.equal(await launch({ connect: async () => { if (existsSync(marker)) return true; throw new Error('closed'); },
    self: [process.execPath, path.join(packageDir, 'cli.mjs')], env }), true);
  writeFileSync(path.join(hooks, 'resume-install-after-commit'), '');
  assert.equal(await new Promise(resolve => installer.on('exit', resolve)), 0, `${output} ${errors}`);
  assert.match(output, /noop/);
});

test('same-kind Rust rollback stops the old selected owner before changing current', async t => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-rust-rollback-owner-'));
  const data = path.join(home, '.sidevoice');
  const hooks = path.join(home, 'hooks');
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: data, SIDEVOICE_TEST_HOOKS: hooks,
    XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'),
    SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_INSTALL_VERIFY_MS: '1000', SIDEVOICE_TEARDOWN_MS: '1000' };
  mkdirSync(data, { recursive: true, mode: 0o700 });
  mkdirSync(hooks);
  writeFileSync(path.join(hooks, 'pause-rollback-after-quiesce'), '');
  const layout = releaseLayout(env);
  const target = rustCoreTarget(), sourceSha = 'a'.repeat(40), archiveSha = 'b'.repeat(64);
  const coreBuild = `rust-native-v1-${target}-${sourceSha}-${archiveSha}`;
  const releases = ['old-rust', 'new-rust'].map((id, index) => {
    const dir = path.join(layout.releases, id), executable = path.join(dir, 'dist', 'sidevoice-rust');
    mkdirSync(path.dirname(executable), { recursive: true, mode: 0o700 });
    const bytes = `#!/bin/sh\n# ${id}\nexit 0\n`;
    writeFileSync(executable, bytes); chmodSync(executable, 0o700);
    const sha = createHash('sha256').update(bytes).digest('hex');
    const release = { id, connector: `0.6.${index}`, core: '0.1.0', channel: 'release', build_seq: 0,
      format: 'sea', runtime_kind: 'rust-native-v1', runtime_target: target, runtime_build_sha: sourceSha,
      runtime_sha256: sha, runtime_size: Buffer.byteLength(bytes), core_kind: 'rust-native-v1',
      core_target: target, core_source_sha: sourceSha, core_archive_sha256: archiveSha, core_build: coreBuild,
      pair_id: `pair-v1:rust-native-v1:${sha}:core:${coreBuild}` };
    writePrivateFile(path.join(dir, 'release.json'), `${JSON.stringify(release)}\n`);
    return release;
  });
  point(env, 'current', releases[0].id);
  point(env, 'previous', releases[1].id);
  const owner = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
  let shutdown = false;
  const connections = new Set();
  const identity = { ...releases[0], version: releases[0].connector, release_id: releases[0].id,
    executable: realpathSync(path.join(layout.current, 'dist', 'sidevoice-rust')), managed: false, pid: owner.pid };
  const server = net.createServer(socket => {
    connections.add(socket);
    socket.on('close', () => connections.delete(socket));
    let input = '';
    socket.on('data', chunk => {
      input += chunk;
      const end = input.indexOf('\n');
      if (end < 0) return;
      const request = JSON.parse(input.slice(0, end));
      const result = request.method === 'identity' ? identity : request.method === 'status' ? { version: identity.version } : { ok: true };
      if (request.method === 'shutdown') {
        shutdown = true;
        assert.equal(readlinkSync(layout.current), path.join('releases', releases[0].id));
        owner.kill('SIGTERM');
      }
      socket.end(JSON.stringify({ id: request.id, ok: true, result }) + '\n');
      if (request.method === 'shutdown') {
        server.close();
        setImmediate(() => { for (const connection of connections) connection.destroy(); });
      }
    });
  });
  await new Promise(resolve => server.listen(path.join(data, 'connector.sock'), resolve));
  chmodSync(path.join(data, 'connector.sock'), 0o600);
  const facade = connectorClient(env);
  await facade.ensure();
  t.after(() => { facade.end(); owner.kill('SIGKILL'); server.close(); rmSync(home, { recursive: true, force: true }); });
  const source = `import { rollback } from ${JSON.stringify(new URL('../install.mjs', import.meta.url).href)};
    try { await rollback(process.env); } catch (error) { console.log(error.key); }`;
  const back = spawn(process.execPath, ['--input-type=module', '-e', source], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  for (let i = 0; i < 200 && !existsSync(path.join(hooks, 'paused-rollback-after-quiesce')); i++) await wait(25);
  assert.equal(existsSync(path.join(hooks, 'paused-rollback-after-quiesce')), true, 'rollback reached its pre-switch barrier');
  assert.equal(shutdown, true, 'the selected old Rust daemon received identity-checked shutdown');
  assert.equal(facade.connected, false, 'the old socket closed with a facade connected');
  assert.equal(readlinkSync(layout.current), path.join('releases', releases[0].id));
  writeFileSync(path.join(hooks, 'resume-rollback-after-quiesce'), '');
  await new Promise(resolve => back.on('exit', resolve));
  assert.equal(readlinkSync(layout.current), path.join('releases', releases[1].id));
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
  const launchctl = path.join(tools, 'launchctl');
  writeFileSync(manager, `#!/bin/sh\nFAKE_MANAGER_DIR="${state}" exec "${process.execPath}" "${fakeManager}" systemctl "$@"\n`, { mode: 0o755 });
  writeFileSync(launchctl, `#!/bin/sh\nFAKE_MANAGER_DIR="${state}" exec "${process.execPath}" "${fakeManager}" launchctl "$@"\n`, { mode: 0o755 });
  mkdirSync(path.join(home, '.cursor'));
  const claude = fakeClaude(home);
  const dataDir = path.join(home, '.sidevoice');
  const R = path.join(home, 'xdg', 'sidevoice');
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir,
    SIDEVOICE_CLAUDE_BIN: claude.bin, SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: kind, SIDEVOICE_SYSTEMCTL: manager,
    SIDEVOICE_LAUNCHCTL: launchctl, SIDEVOICE_LOGINCTL: '/bin/false',
    SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_VERIFY_MS: '15000', SIDEVOICE_TEARDOWN_MS: '3000', SIDEVOICE_SERVICE_START_WAIT_MS: '3000' };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_CORE_BIN', 'SIDEVOICE_TEST_HOOKS', 'FAKE_CORE_MODE', 'FAKE_CORE_MODES']) delete env[key];
  /** `<cli> <args…> --json` (a built package's, or this checkout's): its exit, its signal, and its one JSON line. */
  const run = (cli, args, more = {}) => {
    const ran = spawnSync(process.execPath, [cli, ...args, '--json'], { env: { ...env, ...more }, encoding: 'utf8' });
    let answer = null; try { answer = JSON.parse(ran.stdout.trim().split('\n').at(-1)); } catch {}
    return { status: ran.status, signal: ran.signal, answer, stderr: ran.stderr };
  };
  const selected = name => { try { return JSON.parse(readFileSync(path.join(R, name, 'release.json'), 'utf8')).connector; } catch { return null; } };
  const pid = job => { try { return Number(readFileSync(path.join(state, kind === 'launchd' ? `dev.sidevoice.${job}` : `sidevoice-${job}.service`, 'pid'), 'utf8')); } catch { return null; } };
  return {
    home, dataDir, R, state, env, claude, run, selected, pid,
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
  assert.deepEqual(JSON.parse(readFileSync(path.join(one.dataDir, 'install.json'), 'utf8')), { command: [process.execPath, path.join(one.R, 'current', 'dist', 'cli.mjs')], nodeExecutable: process.execPath, releases: one.R, definitions: [] }, 'install.json: the stable command, its rollback interpreter, and where the installation is');
  // Same version, stamped by the build as CI stamps a nightly: the higher run number wins, whichever runs last.
  const two = machine('none');
  const older = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '10' }), newer = builtAs('0.6.0', { SIDEVOICE_CHANNEL: 'nightly', SIDEVOICE_BUILD_SEQ: '12' });
  const stamped = JSON.parse(readFileSync(path.join(path.dirname(newer), 'package.json'), 'utf8')).sidevoice;
  assert.deepEqual({ channel: stamped.channel, build_seq: stamped.build_seq }, { channel: 'nightly', build_seq: 12 }, 'stamped into the shipped manifest');
  assert.match(stamped.connector_sha, /^[0-9a-f]{40}$/, 'the shipped package carries the source commit identity');
  await Promise.all([older, newer].map(cli => new Promise(resolve => spawn(process.execPath, [cli, 'install', '--no-agents', '--no-core', '--json'], { env: two.env, stdio: 'ignore' }).on('exit', resolve))));
  assert.equal(realpathSync(path.join(two.R, 'current')), path.join(realpathSync(two.R), 'releases', '0.6.0-nightly.12-nocore'), 'a nightly build is a release of its own');
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

for (const kind of ['launchd', 'systemd']) {
  test(`same-version Core switch recovery (${kind}): a crash after definitions leaves the old Core running, retry restarts the selected native pair`, async () => {
    const node = machine(kind), oldPackage = builtAs('0.6.1'), oldCore = coreWrapper();
    try {
      assert.equal(node.install(oldPackage, oldCore).status, 0);
      const dataCore = path.join(node.dataDir, 'core', 'core.json');
      const previousReady = JSON.parse(readFileSync(dataCore, 'utf8'));
      const target = rustCoreTarget(), sourceSha = 'b41840e41e3eb81905d285514c7deb35bd8efe57';
      const archiveSha = 'a'.repeat(64), runtimeSha = 'c'.repeat(64);
      const coreBuild = `${RUST_CORE_KIND}-${target}-${sourceSha}-${archiveSha}`;
      const id = `0.6.1-native-${target}`;
      const release = { id, connector: '0.6.1', core: '0.1.0', core_build: coreBuild,
        channel: 'release', build_seq: 0, format: 'sea', pair_id: `pair-v1:javascript:${runtimeSha}:core:${coreBuild}`,
        runtime_kind: 'javascript', runtime_build_sha: null, runtime_sha256: runtimeSha,
        core_kind: RUST_CORE_KIND, core_source_sha: sourceSha, core_cargo_lock_sha256: '1'.repeat(64),
        core_manifest_sha256: '2'.repeat(64), core_target: target, core_archive_sha256: archiveSha,
        core_archive_size: 123, core_entrypoint: RUST_CORE_ENTRYPOINT };
      const releaseDir = path.join(node.R, 'releases', id);
      const nativeBin = path.join(releaseDir, 'core', RUST_CORE_ENTRYPOINT);
      mkdirSync(path.dirname(nativeBin), { recursive: true, mode: 0o700 });
      cpSync(path.dirname(oldPackage), path.join(releaseDir, 'dist'), { recursive: true });
      writeFileSync(path.join(releaseDir, 'dist', 'sidevoice'),
        `#!/bin/sh\nexec "${process.execPath}" "${path.join(releaseDir, 'dist', 'cli.mjs')}" "$@"\n`, { mode: 0o755 });
      writeFileSync(nativeBin, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
      writePrivateFile(path.join(releaseDir, 'release.json'), `${JSON.stringify(release)}\n`);

      const candidate = { ...release, source: null };
      const applyUrl = new URL('../install.mjs', import.meta.url).href;
      const source = `import { apply } from ${JSON.stringify(applyUrl)}; await apply(process.env, { candidateRelease: JSON.parse(process.env.SIDEVOICE_TEST_CANDIDATE) });`;
      const runApply = hooks => spawnSync(process.execPath, ['--input-type=module', '-e', source], { encoding: 'utf8', env: {
        ...node.env, SIDEVOICE_TEST_CANDIDATE: JSON.stringify(candidate), ...(hooks ? { SIDEVOICE_TEST_HOOKS: hooks } : {}) } });
      const killed = runApply(crashAt('definitions-written'));
      assert.equal(killed.signal, 'SIGKILL', `crash after definitions (${killed.stderr})`);
      assert.equal(node.status().state, 'running', 'the old same-version Python Core is still live before retry');
      assert.equal(JSON.parse(readFileSync(dataCore, 'utf8')).launch_id, previousReady.launch_id);
      const managerJob = kind === 'launchd' ? 'dev.sidevoice.core' : 'sidevoice-core.service';
      const loadedBefore = JSON.parse(readFileSync(path.join(node.state, managerJob, 'loaded-spec.json'), 'utf8'));
      const definitionFile = kind === 'launchd'
        ? path.join(node.home, 'Library', 'LaunchAgents', 'dev.sidevoice.core.plist')
        : path.join(node.home, '.config', 'systemd', 'user', 'sidevoice-core.service');
      assert.equal(loadedBefore.program[0], path.join(node.R, 'current', 'core', 'bin', 'sidevoice-core'), 'the manager still has the prior Python command loaded');
      assert.match(readFileSync(definitionFile, 'utf8'), /sidevoice-core-rust/, 'the on-disk definition already names the native command');

      const recovered = runApply(null);
      assert.equal(recovered.status, 0, recovered.stderr + recovered.stdout);
      const ready = JSON.parse(readFileSync(dataCore, 'utf8'));
      assert.notEqual(ready.launch_id, previousReady.launch_id, 'verification requires a Core launch created by the retry');
      assert.equal(node.pid('core'), ready.pid, 'the service manager owns that fresh launch');
      const loaded = JSON.parse(readFileSync(path.join(node.state, managerJob, 'spec.json'), 'utf8'));
      assert.equal(loaded.program[0], path.join(node.R, 'current', 'core', RUST_CORE_ENTRYPOINT), 'the manager loaded the selected native executable');
      assert.equal(JSON.parse(readFileSync(path.join(node.R, 'current', 'release.json'), 'utf8')).core_kind, RUST_CORE_KIND);
      assert.equal(JSON.parse(readFileSync(path.join(node.R, 'verified', 'release.json'), 'utf8')).id, id);
      assert.equal(node.status().state, 'running');
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
    assert.deepEqual(readdirSync(node.dataDir).sort(), ['agents.lock', 'install.lock', 'node-stopped.json'], 'the stop stays, with the permanent locks');
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

test('blocker 4: with no manager, installing again verifies the selection — a broken one left by a dead installer goes back to the verified one', async () => {
  const node = machine('none');
  const [a, b] = [builtAs('0.6.81'), builtAs('0.6.82')];
  try {
    assert.equal(node.install(a, coreWrapper(), ['--no-agents']).status, 0);
    assert.equal(node.install(b, coreWrapper('import'), ['--no-agents'], { SIDEVOICE_TEST_HOOKS: crashAt('switch-current') }).signal, 'SIGKILL');
    node.stop();   // a reboot: nothing of the old release runs any more
    await wait(250);
    const again = node.install(b, coreWrapper('import'), ['--no-agents']);
    assert.equal(again.answer.error?.key, 'install.rollback', JSON.stringify(again.answer));
    assert.equal(node.selected('current'), '0.6.81');
    assert.equal(node.status().reachable, true, 'the verified release serves again');
  } finally { node.stop(); }
});

test('blocker 5: the rollback target is the last verified release — A runs, B is left selected unverified by a dead installer, C cannot serve: back on A', async () => {
  const node = machine();
  const [a, b, c] = [builtAs('0.6.81'), builtAs('0.6.82'), builtAs('0.6.83')];
  try {
    assert.equal(node.install(a, coreWrapper(), ['--no-agents', '--service']).status, 0);
    assert.equal(node.selected('verified'), '0.6.81');
    assert.equal(node.install(b, coreWrapper('import'), ['--no-agents', '--service'], { SIDEVOICE_TEST_HOOKS: crashAt('switch-current') }).signal, 'SIGKILL');
    assert.deepEqual([node.selected('current'), node.selected('verified')], ['0.6.82', '0.6.81'], 'B selected, never verified');
    const broken = node.install(c, coreWrapper('import'), ['--no-agents', '--service']);
    assert.equal(broken.answer.error?.key, 'install.rollback', JSON.stringify(broken.answer));
    assert.deepEqual([node.selected('current'), node.selected('previous'), node.selected('verified')], ['0.6.81', '0.6.81', '0.6.81']);
    assert.equal(node.status().state, 'running');
    assert.deepEqual(node.releases(), ['0.6.81'], 'B and C pruned; A kept');
  } finally { node.stop(); }
});

test('blocker 6: a release installed without its core does not satisfy an install that needs one — a complete one is staged, and the jobs run it', async () => {
  const node = machine();
  const cli = builtAs('0.6.81');
  try {
    const coreless = node.install(cli, coreWrapper(), ['--no-agents', '--no-core']);
    assert.deepEqual([coreless.status, coreless.answer.installed], [0, '0.6.81-nocore']);
    const full = node.install(cli, coreWrapper(), ['--no-agents', '--service']);
    assert.deepEqual([full.status, full.answer.action, full.answer.installed, full.answer.service, full.answer.state], [0, 'upgrade', '0.6.81', 'systemd', 'running'], JSON.stringify(full.answer));
    assert.ok(existsSync(path.join(node.home, '.config', 'systemd', 'user', 'sidevoice-core.service')));
    // And a coreless install afterwards changes nothing.
    assert.equal(node.install(cli, coreWrapper(), ['--no-agents', '--no-core']).answer.action, 'noop');
  } finally { node.stop(); }
});

test('re-check: with no manager, a core still running from the verified release never verifies another — A runs, B is left selected (A\'s core alive), installing B again goes back to A; C broken: back on A, and rollback still works', async () => {
  const node = machine('none');
  const [a, b, c] = [builtAs('0.6.81'), builtAs('0.6.82'), builtAs('0.6.83')];
  try {
    assert.equal(node.install(a, coreWrapper(), ['--no-agents']).status, 0);
    const corePid = () => { try { return JSON.parse(readFileSync(path.join(node.dataDir, 'core', 'core.json'), 'utf8')).pid; } catch { return null; } };
    const first = corePid();
    assert.ok(first && alive(first));
    assert.equal(node.install(b, coreWrapper('import'), ['--no-agents'], { SIDEVOICE_TEST_HOOKS: crashAt('switch-current') }).signal, 'SIGKILL');
    assert.ok(alive(first), 'A\'s detached core outlives the installer');
    assert.deepEqual([node.selected('current'), node.selected('verified')], ['0.6.82', '0.6.81']);
    // B's own core is what is verified — not A's, still answering: B cannot serve, so back on A.
    const again = node.install(b, coreWrapper('import'), ['--no-agents']);
    assert.equal(again.answer.error?.key, 'install.rollback', JSON.stringify(again.answer));
    assert.deepEqual([node.selected('current'), node.selected('verified')], ['0.6.81', '0.6.81']);
    assert.notEqual(corePid(), first, 'the core serving now is one started from A, after A\'s old one was stopped');
    const broken = node.install(c, coreWrapper('import'), ['--no-agents']);
    assert.equal(broken.answer.error?.key, 'install.rollback', JSON.stringify(broken.answer));
    assert.deepEqual([node.selected('current'), node.selected('verified')], ['0.6.81', '0.6.81']);
    assert.equal(node.status().reachable, true);
    // A healthy update, then a person goes back: rollback still works.
    const d = builtAs('0.6.84');
    assert.equal(node.install(d, coreWrapper(), ['--no-agents']).status, 0);
    assert.equal(node.selected('verified'), '0.6.84');
    const back = node.run(path.join(node.R, 'current', 'dist', 'cli.mjs'), ['rollback']);
    assert.deepEqual([back.status, back.answer.installed], [0, '0.6.81'], JSON.stringify(back.answer));
  } finally { node.stop(); }
});
