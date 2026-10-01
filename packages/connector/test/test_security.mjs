/** The singleton and install locks under exact interleavings (review finding 1), the file-system trust boundary
 *  (15), and signals only to processes proven to be ours (16). Interleavings are held at pause points
 *  (`testpoint.mjs`), never raced on timing. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, symlinkSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { supervisedNode } from './test_core.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(here, '..', 'cli.mjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 15_000) { const start = Date.now(); while (Date.now() - start < timeout) { const value = await check(); if (value) return value; await wait(20); } throw new Error('timed out waiting'); }
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
const exited = child => new Promise(resolve => (child.exitCode !== null || child.signalCode !== null ? resolve(child.exitCode) : child.once('exit', code => resolve(code))));

/** A hooks directory, with the points to hold armed. */
function hooks(...points) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-hooks-'));
  for (const point of points) writeFileSync(path.join(dir, `pause-${point}`), '');
  return { dir, paused: point => until(() => existsSync(path.join(dir, `paused-${point}`))), resume: point => writeFileSync(path.join(dir, `resume-${point}`), '') };
}
const lockOf = node => JSON.parse(readFileSync(node.socketPath + '.lock', 'utf8'));
/** A pid that certainly names no process now. */
function deadPid() { const child = spawnSync(process.execPath, ['-e', '']); return child.pid; }

test('lock: a connector held between writing its record and publishing it does not also take a lock taken meanwhile — one serves', async () => {
  const node = supervisedNode();
  const hold = hooks('lock-publish-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-publish-connector');
    const second = node.start([]);
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, second.pid);
    hold.resume('lock-publish-connector');
    assert.equal(await exited(first), 0, 'the first sees the lock taken, and defers');
    assert.equal(second.exitCode, null, 'the second serves');
    assert.equal(lockOf(node).pid, second.pid);
  } finally { node.stop(); }
});

test('lock: a lock just published is never taken for abandoned — a second starter defers to its owner', async () => {
  const node = supervisedNode();
  const hold = hooks('lock-held-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-held-connector');
    const second = node.start([]);
    assert.equal(await exited(second), 0, 'the owner is alive: the second exits');
    hold.resume('lock-held-connector');
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, first.pid);
    assert.equal(first.exitCode, null);
  } finally { node.stop(); }
});

test('lock: a stale lock taken over by someone else between judging and reclaiming is put back — the late reclaimer does not also hold it', async () => {
  const node = supervisedNode();
  mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(node.socketPath + '.lock', JSON.stringify({ pid: deadPid(), start: 'gone', kind: 'connector', nonce: 'stale-one', at: new Date().toISOString() }), { mode: 0o600 });
  const hold = hooks('lock-reclaim-connector');
  try {
    const late = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-reclaim-connector');
    const quick = node.start([]);
    await until(() => existsSync(node.socketPath) && lockOf(node).pid === quick.pid);
    hold.resume('lock-reclaim-connector');
    assert.equal(await exited(late), 0, 'it finds the lock is not the stale one it judged, and defers');
    assert.equal(lockOf(node).pid, quick.pid, 'the new owner\'s lock is back in place');
    assert.equal(quick.exitCode, null);
  } finally { node.stop(); }
});

test('lock: an empty or unreadable lock is never deleted as if its owner were dead', async () => {
  const node = supervisedNode();
  mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
  writeFileSync(node.socketPath + '.lock', '', { mode: 0o600 });
  try {
    const child = node.start([]);
    assert.equal(await exited(child), 0);
    assert.equal(readFileSync(node.socketPath + '.lock', 'utf8'), '', 'left exactly as it was');
    assert.equal(existsSync(node.socketPath), false);
  } finally { node.stop(); }
});

test('install lock: an installer held before publishing its lock waits for the one that took it meanwhile', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-ilock-'));
  const log = path.join(dataDir, 'order.log');
  const hold = hooks('lock-publish-install');
  const script = name => `import { takeInstallLock } from ${JSON.stringify(path.join(here, '..', 'core.mjs'))};
    import { appendFileSync } from 'node:fs';
    const release = await takeInstallLock(${JSON.stringify(dataDir)});
    appendFileSync(${JSON.stringify(log)}, '${name} in\\n');
    await new Promise(r => setTimeout(r, 400));
    appendFileSync(${JSON.stringify(log)}, '${name} out\\n');
    release();`;
  const first = spawn(process.execPath, ['--input-type=module', '-e', script('A')], { env: { ...process.env, SIDEVOICE_TEST_HOOKS: hold.dir }, stdio: 'ignore' });
  await hold.paused('lock-publish-install');
  const second = spawn(process.execPath, ['--input-type=module', '-e', script('B')], { stdio: 'ignore' });
  await until(() => existsSync(log) && readFileSync(log, 'utf8').includes('B in'));
  hold.resume('lock-publish-install');
  await Promise.all([exited(first), exited(second)]);
  assert.deepEqual(readFileSync(log, 'utf8').trim().split('\n'), ['B in', 'B out', 'A in', 'A out'], 'never both inside');
});

test('trust boundary: a data directory others can write into is not served from; a socket is bound 0600 whatever the umask', async () => {
  const open = supervisedNode();
  mkdirSync(open.dataDir, { recursive: true }); chmodSync(open.dataDir, 0o777);
  try {
    const refused = open.start([]);
    assert.equal(await exited(refused), 78);
    assert.equal(existsSync(open.socketPath), false);
  } finally { open.stop(); }
  // A data directory others can read (0755), and a umask that lets everything through.
  const node = supervisedNode();
  mkdirSync(node.dataDir, { recursive: true }); chmodSync(node.dataDir, 0o755);
  const umask = process.umask(0);
  try {
    node.start([]);
    await until(() => existsSync(node.socketPath));
    assert.equal(lstatSync(node.socketPath).mode & 0o777, 0o600, 'nobody else can open it');
  } finally { process.umask(umask); node.stop(); }
});

test('trust boundary: an install.json others could have written, or a link, never names a program to run', async () => {
  for (const variant of ['writable-file', 'writable-dir', 'symlink']) {
    const home = mkdtempSync(path.join(os.tmpdir(), 'sv-forged-'));
    const dataDir = path.join(home, '.sidevoice');
    mkdirSync(dataDir, { mode: 0o700 });
    const marker = path.join(home, 'executed');
    const forged = JSON.stringify({ id: 'x', connector: '9.9.9', command: ['/bin/sh', '-c', `touch ${marker}`] });
    if (variant === 'symlink') { writeFileSync(path.join(home, 'elsewhere.json'), forged, { mode: 0o600 }); symlinkSync(path.join(home, 'elsewhere.json'), path.join(dataDir, 'install.json')); }
    else { writeFileSync(path.join(dataDir, 'install.json'), forged, { mode: 0o600 }); chmodSync(path.join(dataDir, 'install.json'), variant === 'writable-file' ? 0o666 : 0o600); }
    if (variant === 'writable-dir') chmodSync(dataDir, 0o777);
    const run = spawnSync(process.execPath, [cli, 'service', 'start', '--json'], { env: { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_SERVICE_MANAGER: 'none' }, encoding: 'utf8' });
    const answer = JSON.parse(run.stdout.trim().split('\n').at(-1));
    assert.equal(run.status, 1, variant);
    assert.equal(answer.ok, false);
    assert.equal(answer.error.key, variant === 'writable-dir' ? 'identity.unsafe-directory' : 'identity.unsafe-file', variant);
    await wait(300);
    assert.equal(existsSync(marker), false, `${variant}: the forged command never ran`);
  }
});

test('signals: a stale lock or ready file naming a reused pid never gets somebody else\'s process killed', async () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-pids-'));
  const dataDir = path.join(home, '.sidevoice');
  mkdirSync(path.join(dataDir, 'core'), { recursive: true, mode: 0o700 });
  const bystander = spawn('sleep', ['30'], { stdio: 'ignore' });
  try {
    const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_SERVICE_MANAGER: 'none', SIDEVOICE_TEARDOWN_MS: '500' };
    const stop = () => spawnSync(process.execPath, [cli, 'service', 'stop', '--json'], { env, encoding: 'utf8' });
    // A bare pid (the old format), a record whose start time is another process's, and a ready file naming it as a core.
    writeFileSync(path.join(dataDir, 'connector.sock.lock'), String(bystander.pid), { mode: 0o600 });
    stop();
    writeFileSync(path.join(dataDir, 'connector.sock.lock'), JSON.stringify({ pid: bystander.pid, start: 'another-process', kind: 'connector', nonce: 'n', at: new Date().toISOString() }), { mode: 0o600 });
    stop();
    writeFileSync(path.join(dataDir, 'core', 'core.json'), JSON.stringify({ pid: bystander.pid, launch_id: 'l-1', socket: path.join(dataDir, 'core', 'local.sock'), connector_id: 'c', token: 't', url: 'http://127.0.0.1:1' }), { mode: 0o600 });
    stop();
    spawnSync(process.execPath, [cli, 'service', 'uninstall', '--json'], { env, encoding: 'utf8' });
    assert.ok(alive(bystander.pid), 'the bystander is still running');
  } finally { bystander.kill('SIGKILL'); }
});
