/** The singleton and install locks under exact interleavings (review finding 1), the file-system trust boundary
 *  (15), and signals only to processes proven to be ours (16). Interleavings are held at pause points
 *  (`testpoint.mjs`), never raced on timing. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
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

test('lock: a connector held before taking the lock does not also take it once another has — one serves', async () => {
  const node = supervisedNode();
  const hold = hooks('lock-publish-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-publish-connector');
    const second = node.start([]);
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, second.pid);
    hold.resume('lock-publish-connector');
    assert.equal(await exited(first), 0, 'the first finds the lock held, and defers');
    assert.equal(second.exitCode, null, 'the second serves');
    assert.equal(lockOf(node).pid, second.pid);
  } finally { node.stop(); }
});

test('lock: a lock just taken is held — a second starter defers to its owner', async () => {
  const node = supervisedNode();
  const hold = hooks('lock-held-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-held-connector');
    const second = node.start([]);
    assert.equal(await exited(second), 0, 'held: the second exits');
    hold.resume('lock-held-connector');
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, first.pid);
    assert.equal(first.exitCode, null);
  } finally { node.stop(); }
});

/** A process that takes the lock (\`kind\`) as soon as it can and says when it holds it and when it lets go. */
function contender(file, kind, log, name, { hold = 300, hooksDir = null, forever = false } = {}) {
  const script = `import { tryLock } from ${JSON.stringify(path.join(here, '..', 'lockfile.mjs'))};
    import { appendFileSync } from 'node:fs';
    for (;;) {
      const taken = await tryLock(${JSON.stringify(file)}, { kind: ${JSON.stringify(kind)} });
      if (taken.held) {
        appendFileSync(${JSON.stringify(log)}, '${name} in ' + Date.now() + '\\n');
        if (${forever}) await new Promise(() => setInterval(() => {}, 60_000));   // the lock is unref'd: keep this holder alive
        await new Promise(r => setTimeout(r, ${hold}));
        appendFileSync(${JSON.stringify(log)}, '${name} out ' + Date.now() + '\\n');
        taken.release(); break;
      }
      await new Promise(r => setTimeout(r, 5));
    }`;
  return spawn(process.execPath, ['--input-type=module', '-e', script], { env: { ...process.env, ...(hooksDir ? { SIDEVOICE_TEST_HOOKS: hooksDir } : {}) }, stdio: 'ignore' });
}
/** The holders in a contender log never overlap. */
function oneAtATime(log) {
  let inside = null;
  for (const [name, what] of readFileSync(log, 'utf8').trim().split('\n').map(line => line.split(' '))) {
    if (what === 'in') { assert.equal(inside, null, `${name} took the lock while ${inside} held it`); inside = name; }
    else { assert.equal(inside, name); inside = null; }
  }
}

test('lock: three contenders — one holding, two released together at the very moment — never two holders, and each gets it in turn', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-three-'));
  const file = path.join(dir, 'install.lock'), log = path.join(dir, 'order.log');
  const held = hooks('lock-held-install');
  const holder = contender(file, 'install', log, 'A', { hooksDir: held.dir, hold: 200 });
  await held.paused('lock-held-install');
  // B and C both wait at the point of taking the lock, and are let go together while A still holds it.
  const gate = hooks('lock-publish-install');
  const b = contender(file, 'install', log, 'B', { hooksDir: gate.dir }), c = contender(file, 'install', log, 'C', { hooksDir: gate.dir });
  await until(() => readdirSync(gate.dir).filter(name => name.startsWith('paused-lock-publish-install-')).length === 2);
  gate.resume('lock-publish-install');
  held.resume('lock-held-install');
  await Promise.all([exited(holder), exited(b), exited(c)]);
  oneAtATime(log);
  assert.deepEqual(readFileSync(log, 'utf8').trim().split('\n').filter(line => line.includes(' in ')).map(line => line.split(' ')[0]).sort(), ['A', 'B', 'C']);
});

test('lock: a holder killed with SIGKILL frees the lock at once — nothing stale is left to reclaim', async () => {
  for (const kind of ['install', 'connector']) {
    const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-kill-'));
    const file = path.join(dir, `${kind}.lock`), log = path.join(dir, 'order.log');
    const holder = contender(file, kind, log, 'A', { forever: true });
    await until(() => existsSync(log));
    holder.kill('SIGKILL');
    await exited(holder);
    const killedAt = Date.now();
    const next = contender(file, kind, log, 'B', { hold: 800 });
    await until(() => readFileSync(log, 'utf8').includes('B in'));
    assert.equal(JSON.parse(readFileSync(file, 'utf8')).pid, next.pid, 'the record says who holds it now');
    await exited(next);
    const took = Number(readFileSync(log, 'utf8').trim().split('\n').find(line => line.startsWith('B in')).split(' ')[2]);
    assert.ok(took - killedAt < 1000, `${kind}: taken ${took - killedAt} ms after the holder died`);
  }
});

test('lock: a record that is not a holder\'s — empty, unreadable, a dead pid in the old format — blocks nothing', async () => {
  for (const content of ['', '{garbage', String(deadPid())]) {
    const node = supervisedNode();
    mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
    writeFileSync(node.socketPath + '.lock', content, { mode: 0o600 });
    try {
      const child = node.start(content ? ['--supervise'] : []);
      await until(() => existsSync(node.socketPath));
      assert.equal(lockOf(node).pid, child.pid, JSON.stringify(content));
    } finally { node.stop(); }
  }
});

test('install lock: an installer held before taking the lock waits for the one that took it meanwhile', async () => {
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
