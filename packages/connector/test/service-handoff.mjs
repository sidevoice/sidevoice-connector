#!/usr/bin/env node
/** The supervisor's hand-off under the real service manager — launchd or a systemd user manager — for CI (one step
 *  in each of the `service-macos` and `service-linux` jobs, after `service-integration.mjs`). Not part of `npm test`:
 *  it installs a real LaunchAgent or user unit.
 *
 *  Three built packages of this connector (versions 9.0.0, 9.1.0, 9.2.0) stand for three installations. A is installed
 *  with its service. B is then committed by an installer killed right after its commit point (`install.json`): the
 *  definition already names B, the running supervisor is still A, and nobody restarts it — the supervisor finds the
 *  journal, sees it is not the selected program, and hands over through the `service reload` helper; the manager must
 *  end up running B. C is committed the same way, but C's core cannot serve: B's supervisor hands over to C, C's
 *  supervisor finds its core failing, rolls the installation back to B and hands over again; the manager must end up
 *  running B, with the journal gone. The core is the fake. */
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.join(here, '..');
const dataDir = path.join(os.homedir(), '.sidevoice');
const kind = process.platform === 'darwin' ? 'launchd' : 'systemd';
const uid = process.getuid();
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const step = text => console.log(`\n=== ${text}`);
async function until(what, check, timeout = 120_000) {
  const start = Date.now();
  for (;;) { const value = await check(); if (value) return value; if (Date.now() - start > timeout) throw new Error(`timed out: ${what}`); await wait(500); }
}

// The fake core; it fails to import whenever the installation's settings ask it to (C's do).
const tools = mkdtempSync(path.join(os.tmpdir(), 'sv-handoff-'));
const coreBin = path.join(tools, 'sidevoice-core');
writeFileSync(coreBin, `#!/bin/sh\n[ -n "$SIDEVOICE_TEST_FAIL_CORE" ] && export FAKE_CORE_MODE=import\nexec "${process.execPath}" "${path.join(here, 'fake-sidevoice-core.mjs')}" "$@"\n`, { mode: 0o755 });

/** This package, built, copied to stand as version `version`. */
function built(version) {
  const root = mkdtempSync(path.join(tools, `pkg-${version}-`));
  cpSync(path.join(packageDir, 'dist'), path.join(root, 'dist'), { recursive: true });
  for (const manifest of [path.join(root, 'dist', 'package.json'), path.join(root, 'package.json')]) {
    const shipped = JSON.parse(readFileSync(path.join(packageDir, 'dist', 'package.json'), 'utf8'));
    writeFileSync(manifest, JSON.stringify({ ...shipped, version }, null, 2));
  }
  return path.join(root, 'dist', 'cli.mjs');
}
execFileSync(process.execPath, [path.join(packageDir, 'build.mjs')], { stdio: 'inherit' });
const [a, b, c] = ['9.0.0', '9.1.0', '9.2.0'].map(built);

const base = { ...process.env, SIDEVOICE_CORE_BIN: coreBin, SIDEVOICE_CORE_PORT: '0', SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_BACKOFF_MS: '200', SIDEVOICE_INSTALL_VERIFY_MS: '60000' };
for (const name of ['SIDEVOICE_SERVICE_MANAGER', 'SIDEVOICE_DATA_DIR', 'SIDEVOICE_URL', 'SIDEVOICE_TEST_HOOKS']) delete base[name];
function run(cli, args, env = base) {
  const result = spawnSync(process.execPath, [cli, ...args], { env, encoding: 'utf8', timeout: 240_000 });
  console.log(`$ ${path.basename(path.dirname(path.dirname(cli)))} ${args.join(' ')} → exit ${result.status ?? result.signal} ${result.stdout.trim().split('\n').at(-1)?.slice(0, 300) ?? ''}`);
  return result;
}
const status = () => { const out = run(a, ['service', 'status', '--json']).stdout.trim().split('\n').at(-1); try { return JSON.parse(out); } catch { return null; } };
const selected = () => JSON.parse(readFileSync(path.join(dataDir, 'install.json'), 'utf8'));
const journal = () => existsSync(path.join(dataDir, 'install-txn.json'));
/** What the manager runs now: its job's program, from the manager itself. */
function managerRuns() {
  if (kind === 'launchd') {
    const printed = spawnSync('launchctl', ['print', `gui/${uid}/dev.sidevoice.node`], { encoding: 'utf8' }).stdout;
    return { running: /^\s*state = running/m.test(printed), program: printed.match(/arguments = \{([\s\S]*?)\}/)?.[1] ?? '' };
  }
  const shown = spawnSync('systemctl', ['--user', 'show', '-p', 'ActiveState,ExecStart', 'sidevoice-node.service'], { encoding: 'utf8' }).stdout;
  return { running: /ActiveState=active/.test(shown), program: shown.match(/^ExecStart=(.*)$/m)?.[1] ?? '' };
}
/** The installation the manager runs and the supervisor answering are both `cli`'s — the installed copy's CLI, as the
 *  selection names it — and nothing is left to recover. */
const servedBy = cli => until(`the manager running ${cli}`, () => {
  const now = status(), manager = managerRuns();
  return now?.supervisor && now.state === 'running' && now.command?.[1] === cli && manager.running && manager.program.includes(cli) && !journal() ? now : null;
});
/** Commit `cli`'s installation and kill the installer right after its commit point: the hand-off is the supervisor's. */
function commitAndDie(cli, extra = {}) {
  const hooks = mkdtempSync(path.join(tools, 'hooks-'));
  writeFileSync(path.join(hooks, 'crash-txn-install.json'), '');
  const result = run(cli, ['install', '--no-agents', '--json'], { ...base, ...extra, SIDEVOICE_TEST_HOOKS: hooks });
  assert.equal(result.signal, 'SIGKILL', 'the installer died right after its commit point');
  assert.ok(journal(), 'and left its journal');
}

try {
  step(`A installed with its service (${kind})`);
  assert.equal(run(a, ['install', '--no-agents', '--json']).status, 0);
  const installedA = selected().command[1];
  await servedBy(installedA);

  step('B committed by an installer that died: the running supervisor (A) hands over through `service reload`');
  commitAndDie(b);
  const installedB = selected().command[1];
  assert.ok(installedB !== installedA && installedB.includes(`${path.sep}9.1.0${path.sep}`), installedB);
  const handedOver = await servedBy(installedB);
  console.log(`the manager runs B; supervisor attempts ${handedOver.attempts}`);

  step('C committed the same way, but its core cannot serve: C rolls back to B and hands over again');
  commitAndDie(c, { SIDEVOICE_TEST_FAIL_CORE: '1' });
  const installedC = selected().command[1];
  assert.ok(installedC.includes(`${path.sep}9.2.0${path.sep}`), installedC);
  await until('C\'s supervisor took over', () => status()?.command?.[1] === installedC || selected().command[1] === installedB);
  const back = await servedBy(installedB);
  assert.equal(selected().command[1], installedB, 'the installation is B again');
  console.log(`rolled back: the manager runs B; core ${back.core?.pid}`);

  step('uninstall');
  assert.equal(run(b, ['uninstall']).status, 0);
  console.log('\nOK');
} catch (error) {
  console.error(`\nFAILED: ${error.stack || error.message}`);
  for (const file of ['node-service.log', 'core.log']) {
    try { console.error(`\n--- ${file}\n` + readFileSync(path.join(dataDir, file), 'utf8').split('\n').slice(-80).join('\n')); } catch {}
  }
  console.error(JSON.stringify(managerRuns()));
  process.exitCode = 1;
}
