/** The connector and install locks — plain `flock(2)`, taken in one call and gone with the holder (review finding 1;
 *  R1-SIMPLIFY-DECISION §2) — under exact interleavings, the file-system trust boundary (15), and signals only to
 *  processes proven to be ours (16). Interleavings are held at pause points (`testpoint.mjs`), never raced on timing. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { fakeNode } from './test_core.mjs';
import { tryLock } from '../lockfile.mjs';

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
const lockOf = node => JSON.parse(readFileSync(path.join(node.dataDir, 'connector.lock'), 'utf8'));
/** A pid that certainly names no process now. */
function deadPid() { const child = spawnSync(process.execPath, ['-e', '']); return child.pid; }

test('lock: a connector held before taking the lock does not also take it once another has — one serves', async () => {
  const node = fakeNode();
  const hold = hooks('lock-before-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-before-connector');
    const second = node.start([]);
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, second.pid);
    hold.resume('lock-before-connector');
    assert.equal(await exited(first), 0, 'the first finds the lock held, and defers');
    assert.equal(second.exitCode, null, 'the second serves');
    assert.equal(lockOf(node).pid, second.pid);
  } finally { node.stop(); }
});

test('lock: a lock just taken is held — a second starter defers to its owner; the connector job waits for it instead of exiting', async () => {
  const node = fakeNode();
  const hold = hooks('lock-held-connector');
  try {
    const first = node.start([], { SIDEVOICE_TEST_HOOKS: hold.dir });
    await hold.paused('lock-held-connector');
    const second = node.start([]);
    assert.equal(await exited(second), 0, 'held: the second exits');
    const job = node.start(['--service']);
    await wait(1500);
    assert.equal(job.exitCode, null, 'the job waits for the holder rather than exiting into its manager\'s restart loop');
    hold.resume('lock-held-connector');
    await until(() => existsSync(node.socketPath));
    assert.equal(lockOf(node).pid, first.pid);
    first.kill('SIGKILL');
    await until(() => { try { return lockOf(node).pid === job.pid; } catch { return false; } });
  } finally { node.stop(); }
});

/** A process that takes the lock (`kind`) as soon as it can and says when it holds it and when it lets go. */
function contender(file, kind, log, name, { hold = 300, hooksDir = null, forever = false, child = false } = {}) {
  const script = `import { tryLock } from ${JSON.stringify(path.join(here, '..', 'lockfile.mjs'))};
    import { appendFileSync } from 'node:fs';
    import { spawn } from 'node:child_process';
    for (;;) {
      const taken = await tryLock(${JSON.stringify(file)}, { kind: ${JSON.stringify(kind)} });
      if (taken.held) {
        if (${child}) appendFileSync(${JSON.stringify(log)}, 'child ' + spawn('sleep', ['30'], { detached: true, stdio: 'ignore' }).pid + '\\n');
        appendFileSync(${JSON.stringify(log)}, '${name} in ' + Date.now() + '\\n');
        if (${forever}) await new Promise(() => setInterval(() => {}, 60_000));
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
    else if (what === 'out') { assert.equal(inside, name); inside = null; }
  }
}

test('lock: three contenders — one holding, two released together at the very moment — never two holders, and each gets it in turn', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-three-'));
  const file = path.join(dir, 'install.lock'), log = path.join(dir, 'order.log');
  const held = hooks('lock-held-install');
  const holder = contender(file, 'install', log, 'A', { hooksDir: held.dir, hold: 200 });
  await held.paused('lock-held-install');
  // B and C both wait at the point of taking the lock, and are let go together while A still holds it.
  const gate = hooks('lock-before-install');
  const b = contender(file, 'install', log, 'B', { hooksDir: gate.dir }), c = contender(file, 'install', log, 'C', { hooksDir: gate.dir });
  await until(() => readdirSync(gate.dir).filter(name => name.startsWith('paused-lock-before-install-')).length === 2);
  gate.resume('lock-before-install');
  held.resume('lock-held-install');
  await Promise.all([exited(holder), exited(b), exited(c)]);
  oneAtATime(log);
  assert.deepEqual(readFileSync(log, 'utf8').trim().split('\n').filter(line => line.includes(' in ')).map(line => line.split(' ')[0]).sort(), ['A', 'B', 'C']);
});

test('lock: a holder killed with SIGKILL frees the lock at once — even with a child of its own still running — and nothing stale is left to reclaim', async () => {
  for (const kind of ['install', 'connector']) {
    const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-kill-'));
    const file = path.join(dir, `${kind}.lock`), log = path.join(dir, 'order.log');
    const holder = contender(file, kind, log, 'A', { forever: true, child: true });
    await until(() => existsSync(log) && readFileSync(log, 'utf8').includes('A in'));
    const child = Number(readFileSync(log, 'utf8').match(/^child (\d+)$/m)[1]);
    const inode = statSync(file).ino;
    holder.kill('SIGKILL');
    await exited(holder);
    const killedAt = Date.now();
    try {
      assert.equal(alive(child), true, 'its child runs on — and holds nothing: the descriptor is close-on-exec');
      const next = contender(file, kind, log, 'B', { hold: 800 });
      await until(() => readFileSync(log, 'utf8').includes('B in'));
      assert.equal(JSON.parse(readFileSync(file, 'utf8')).pid, next.pid, 'the record says who holds it now');
      await exited(next);
      const took = Number(readFileSync(log, 'utf8').trim().split('\n').find(line => line.startsWith('B in')).split(' ')[2]);
      assert.ok(took - killedAt < 1500, `${kind}: taken ${took - killedAt} ms after the holder died`);
      assert.equal(statSync(file).ino, inode, 'the same lock file: never replaced, never deleted');
    } finally { try { process.kill(child, 'SIGKILL'); } catch {} }
  }
});

test('lock: a record that is not a holder\'s — empty, unreadable, a dead pid — blocks nothing', async () => {
  for (const content of ['', '{garbage', JSON.stringify({ pid: deadPid(), start: 'x', kind: 'connector' })]) {
    const node = fakeNode();
    mkdirSync(node.dataDir, { recursive: true, mode: 0o700 });
    writeFileSync(path.join(node.dataDir, 'connector.lock'), content, { mode: 0o600 });
    try {
      const child = node.start([]);
      await until(() => existsSync(node.socketPath));
      assert.equal(lockOf(node).pid, child.pid, JSON.stringify(content));
    } finally { node.stop(); }
  }
});

test('lock: a lock file that is a link is refused, never followed; and with no flock to take it, the error says so', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-link-'));
  writeFileSync(path.join(dir, 'elsewhere'), '');
  symlinkSync(path.join(dir, 'elsewhere'), path.join(dir, 'install.lock'));
  await assert.rejects(tryLock(path.join(dir, 'install.lock'), { kind: 'install' }), error => ['ELOOP', 'EMLINK'].includes(error.code));
  if (process.platform === 'linux') {
    await assert.rejects(tryLock(path.join(dir, 'other.lock'), { kind: 'install', env: { SIDEVOICE_FLOCK: '/nonexistent/flock' } }), error => error.key === 'lock.unavailable');
  }
});

test('install lock: an installer held before taking the lock waits for the one that took it meanwhile', async () => {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-ilock-'));
  const log = path.join(dataDir, 'order.log');
  const hold = hooks('lock-before-install');
  const script = name => `import { takeInstallLock } from ${JSON.stringify(path.join(here, '..', 'core.mjs'))};
    import { appendFileSync } from 'node:fs';
    const release = await takeInstallLock(${JSON.stringify(dataDir)});
    appendFileSync(${JSON.stringify(log)}, '${name} in\\n');
    await new Promise(r => setTimeout(r, 400));
    appendFileSync(${JSON.stringify(log)}, '${name} out\\n');
    release();`;
  const first = spawn(process.execPath, ['--input-type=module', '-e', script('A')], { env: { ...process.env, SIDEVOICE_TEST_HOOKS: hold.dir }, stdio: 'ignore' });
  await hold.paused('lock-before-install');
  const second = spawn(process.execPath, ['--input-type=module', '-e', script('B')], { stdio: 'ignore' });
  await until(() => existsSync(log) && readFileSync(log, 'utf8').includes('B in'));
  hold.resume('lock-before-install');
  await Promise.all([exited(first), exited(second)]);
  assert.deepEqual(readFileSync(log, 'utf8').trim().split('\n'), ['B in', 'B out', 'A in', 'A out'], 'never both inside');
});

test('trust boundary: a data directory others can write into is not served from; a socket is bound 0600 whatever the umask', async () => {
  const open = fakeNode();
  mkdirSync(open.dataDir, { recursive: true }); chmodSync(open.dataDir, 0o777);
  try {
    const refused = open.start([]);
    assert.equal(await exited(refused), 78);
    assert.equal(existsSync(open.socketPath), false);
  } finally { open.stop(); }
  // A data directory others can read (0755), and a umask that lets everything through.
  const node = fakeNode();
  mkdirSync(node.dataDir, { recursive: true }); chmodSync(node.dataDir, 0o755);
  const umask = process.umask(0);
  try {
    node.start([]);
    // listen() creates the socket path before its callback; the connector chmods it to 0600 before serving.
    // Wait for that completed bind rather than observing the transient 0700 mode imposed by its umask.
    await until(() => {
      try { return (lstatSync(node.socketPath).mode & 0o777) === 0o600; } catch { return false; }
    });
    assert.equal(lstatSync(node.socketPath).mode & 0o777, 0o600, 'nobody else can open it');
  } finally { process.umask(umask); node.stop(); }
});

test('trust boundary: an install.json others could have written, or a link, never names a program to run', async () => {
  for (const variant of ['writable-file', 'writable-dir', 'symlink']) {
    const home = mkdtempSync(path.join(os.tmpdir(), 'sv-forged-'));
    const dataDir = path.join(home, '.sidevoice');
    mkdirSync(dataDir, { mode: 0o700 });
    const marker = path.join(home, 'executed');
    const forged = JSON.stringify({ command: ['/bin/sh', '-c', `touch ${marker}`] });
    // An installation selected (`R/current`), so `service install` goes on to write definitions from `install.json`.
    const release = path.join(home, 'xdg', 'sidevoice', 'releases', '9.9.9');
    mkdirSync(release, { recursive: true, mode: 0o700 });
    writeFileSync(path.join(release, 'release.json'), JSON.stringify({ id: '9.9.9', connector: '9.9.9', core: '0.1.0', core_build: 'external', channel: 'release', build_seq: 0 }), { mode: 0o600 });
    symlinkSync(path.join('releases', '9.9.9'), path.join(home, 'xdg', 'sidevoice', 'current'));
    if (variant === 'symlink') { writeFileSync(path.join(home, 'elsewhere.json'), forged, { mode: 0o600 }); symlinkSync(path.join(home, 'elsewhere.json'), path.join(dataDir, 'install.json')); }
    else { writeFileSync(path.join(dataDir, 'install.json'), forged, { mode: 0o600 }); chmodSync(path.join(dataDir, 'install.json'), variant === 'writable-file' ? 0o666 : 0o600); }
    if (variant === 'writable-dir') chmodSync(dataDir, 0o777);
    const run = spawnSync(process.execPath, [cli, 'service', 'install', '--json'], { env: { ...process.env, HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, '.config'), SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_SERVICE_MANAGER: 'systemd', SIDEVOICE_SYSTEMCTL: '/bin/true' }, encoding: 'utf8' });
    const answer = JSON.parse(run.stdout.trim().split('\n').at(-1));
    assert.equal(run.status, 1, variant);
    assert.equal(answer.ok, false);
    assert.equal(answer.error.key, variant === 'writable-dir' ? 'identity.unsafe-directory' : 'identity.unsafe-file', variant);
    await wait(300);
    assert.equal(existsSync(marker), false, `${variant}: the forged command never ran`);
    assert.equal(existsSync(path.join(home, '.config', 'systemd', 'user', 'sidevoice-connector.service')), false, `${variant}: and no definition names it`);
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
    // A lock record nobody holds naming it — with its very start time, then another's — and a ready file naming it as
    // a core: none of them is proof that the process is ours.
    const start = (await import('../proc.mjs')).processIdentity(bystander.pid).start;
    writeFileSync(path.join(dataDir, 'connector.lock'), JSON.stringify({ pid: bystander.pid, start, kind: 'connector', at: new Date().toISOString() }), { mode: 0o600 });
    stop();
    writeFileSync(path.join(dataDir, 'connector.lock'), JSON.stringify({ pid: bystander.pid, start: 'another-process', kind: 'connector', at: new Date().toISOString() }), { mode: 0o600 });
    stop();
    writeFileSync(path.join(dataDir, 'core', 'core.json'), JSON.stringify({ pid: bystander.pid, launch_id: 'l-1', socket: path.join(dataDir, 'core', 'local.sock'), connector_id: 'c', token: 't', url: 'http://127.0.0.1:1' }), { mode: 0o600 });
    stop();
    spawnSync(process.execPath, [cli, 'service', 'uninstall', '--json'], { env, encoding: 'utf8' });
    assert.ok(alive(bystander.pid), 'the bystander is still running');
  } finally { bystander.kill('SIGKILL'); }
});
