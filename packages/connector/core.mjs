/** This machine's core — the Python process that holds its conversations and runs voice — installed
 *  the first time a conversation needs it, started when it is not running, and found when it is.
 *
 *  Installed with `uv`, never Docker, at the one version this package pins: `CORE_VERSION`, bumped by
 *  hand like every other pin here. Where it is installed from, in order: `SIDEVOICE_CORE_SPEC` (a wheel,
 *  a directory, a git URL, a requirement — anything `uv pip install` takes); the wheel the published
 *  package carries beside its bundle (`dist/core/`) — or, run from a checkout, the wheel put beside these
 *  sources (`packages/connector/core/`, ignored by git; `SIDEVOICE_CORE_WHEEL_DIR` names another folder);
 *  `sidevoice-core==CORE_VERSION` from the index.
 *  `SIDEVOICE_CORE_BIN` skips installing altogether and names a `sidevoice-core` someone installed.
 *
 *  Installing a Python program is heavy the first time (a few hundred megabytes of wheels) and nothing
 *  afterwards: one virtual environment per version under the data directory, and the other versions'
 *  are removed once this one is in place. Models are not part of it; the core loads Silero and
 *  smart-turn from its wheels, and nothing else unless a person picks a local engine.
 *
 *  Started detached and left running: the core outlives this connector on purpose (a call may be
 *  going on) and exits on its own when nothing has used it for a while. The handshake is its ready
 *  file, `core/core.json`: where it listens and the credential this connector links with. */
import { spawn, execFileSync } from 'node:child_process';
import { accessSync, constants, existsSync, mkdirSync, openSync, closeSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, writeSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const CORE_VERSION = '0.1.0';
export const DEFAULT_PORT = 8768;
const here = path.dirname(fileURLToPath(import.meta.url));
const INSTALL_TIMEOUT_MS = 20 * 60_000;

export const NO_UV = 'Sidevoice installs its core with uv, and uv is not on this machine. Install it '
  + '(macOS/Linux: curl -LsSf https://astral.sh/uv/install.sh | sh — or brew install uv) and ask for voice again.';

export function coreData(dataDir) { return path.join(dataDir, 'core'); }
function runtimeRoot(dataDir) { return path.join(dataDir, 'core-runtime'); }
function logPath(dataDir) { return path.join(dataDir, 'core.log'); }

/** What `uv pip install` is given for the pinned version. */
export function coreSpec(env = process.env) {
  if (env.SIDEVOICE_CORE_SPEC) return env.SIDEVOICE_CORE_SPEC;
  const wheel = path.join(env.SIDEVOICE_CORE_WHEEL_DIR || path.join(here, 'core'), `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`);
  return existsSync(wheel) ? wheel : `sidevoice-core==${CORE_VERSION}`;
}

/** What names an installed copy: the spec, and for a wheel file its size and time too, so a wheel rebuilt in
 *  the same place (a checkout's) is installed again instead of taken for the one already there. */
function specIdentity(spec) {
  try { const stat = statSync(spec); if (stat.isFile()) return `${spec}@${stat.size}-${Math.round(stat.mtimeMs)}`; } catch {}
  return spec;
}

function executable(file) {
  try { accessSync(file, constants.X_OK); return statSync(file).isFile(); } catch { return false; }
}

/** uv, wherever this machine keeps it: named, on the PATH, or where its own installers put it. A
 *  connector started by a harness may have a thinner PATH than the person's shell. */
export function findUv(env = process.env) {
  const name = process.platform === 'win32' ? 'uv.exe' : 'uv';
  const home = env.HOME || os.homedir();
  const candidates = [env.SIDEVOICE_UV,
    ...(env.PATH || '').split(path.delimiter).filter(Boolean).map(dir => path.join(dir, name)),
    path.join(home, '.local', 'bin', name), path.join(home, '.cargo', 'bin', name),
    '/opt/homebrew/bin/uv', '/usr/local/bin/uv'];
  return candidates.find(candidate => candidate && executable(candidate)) || null;
}

/** Why uv failed, in words a person can act on: what it printed last, and what usually fixes it. A corporate
 *  proxy that re-signs HTTPS (UnknownIssuer) was the first real one, 2026-09-30. */
export function explainUvFailure(step, code, lines, log) {
  const text = lines.join('\n');
  const last = lines.filter(line => line.trim()).slice(-3).map(line => line.trim()).join(' | ');
  let hint = '';
  if (/UnknownIssuer|invalid peer certificate|certificate verify|CERTIFICATE_VERIFY_FAILED|self[- ]signed certificate|unable to get local issuer/i.test(text)) {
    hint = ' uv could not verify a TLS certificate — usually a corporate proxy that re-signs HTTPS. Use the system\'s certificate store: run it again with UV_SYSTEM_CERTS=1 (uv 0.12 or later; older uv: UV_NATIVE_TLS=1), or point SSL_CERT_FILE at your organisation\'s CA bundle.';
  } else if (/dns error|failed to lookup address|could not connect|connection refused|connection reset|timed out|error sending request|network is unreachable|tcp connect error/i.test(text)) {
    hint = ' uv could not reach the network (PyPI, and GitHub for its Python): check the connection, and set HTTPS_PROXY if this machine goes through a proxy.';
  }
  return new Error(`Installing this machine's Sidevoice core failed at "uv ${step}" (exit ${code})${last ? ': ' + last : ''}.${hint} Full output: ${log}`);
}

/** Run uv, its output appended to the log and handed line by line to `progress`. */
function run(command, args, { log, env, progress = () => {} }) {
  return new Promise((resolve, reject) => {
    const out = openSync(log, 'a', 0o600);
    const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'], env });
    const tail = []; let partial = '';
    const take = chunk => {
      try { writeSync(out, chunk); } catch {}
      partial += chunk;
      let index;
      while ((index = partial.indexOf('\n')) >= 0) {
        const line = partial.slice(0, index); partial = partial.slice(index + 1);
        tail.push(line); if (tail.length > 60) tail.shift();
        try { progress(line); } catch {}
      }
    };
    child.stdout.on('data', take); child.stderr.on('data', take);
    const timer = setTimeout(() => child.kill('SIGTERM'), INSTALL_TIMEOUT_MS);
    child.on('error', error => { clearTimeout(timer); closeSync(out); reject(error); });
    child.on('close', code => {
      clearTimeout(timer); closeSync(out);
      if (partial) tail.push(partial);
      code === 0 ? resolve() : reject(explainUvFailure(args[0], code, tail, log));
    });
  });
}

/** Who is installing the core right now, if anyone: `sidevoice install` and a connector share one data dir,
 *  and two uv runs into one environment break it. The lock is a file with the installer's pid. */
function lockPath(dataDir) { return path.join(dataDir, 'core-install.lock'); }
function pidAlive(pid) { try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; } }
export function installInProgress(dataDir) {
  let lock; try { lock = JSON.parse(readFileSync(lockPath(dataDir), 'utf8')); } catch { return null; }
  if (!lock?.pid || !pidAlive(lock.pid)) return null;
  let last = null;
  try { last = readFileSync(logPath(dataDir), 'utf8').trim().split('\n').filter(line => line.trim()).at(-1)?.trim() || null; } catch {}
  return { pid: lock.pid, since: lock.since, seconds: Math.round((Date.now() - lock.since) / 1000), last, log: logPath(dataDir) };
}
async function takeInstallLock(dataDir, log) {
  const file = lockPath(dataDir);
  const deadline = Date.now() + INSTALL_TIMEOUT_MS;
  let said = false;
  for (;;) {
    try { writeFileSync(file, JSON.stringify({ pid: process.pid, since: Date.now() }), { flag: 'wx', mode: 0o600 }); return () => { try { rmSync(file, { force: true }); } catch {} }; }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
    const held = installInProgress(dataDir);
    if (!held) { try { rmSync(file, { force: true }); } catch {} continue; }   // left by an installer that is gone
    if (!said) { log(`another process (pid ${held.pid}) is installing the core; waiting for it`); said = true; }
    if (Date.now() > deadline) throw new Error(`Another process (pid ${held.pid}) has been installing the core for ${held.seconds} s; see ${held.log}`);
    await wait(500);
  }
}

/** The `sidevoice-core` to start, installing the pinned version first if it is not there. `progress` gets uv's
 *  output line by line (`sidevoice install` shows it). */
export async function ensureInstalled({ dataDir, env = process.env, log = () => {}, progress = () => {} }) {
  if (env.SIDEVOICE_CORE_BIN) return env.SIDEVOICE_CORE_BIN;
  const spec = coreSpec(env), identity = specIdentity(spec);
  const home = path.join(runtimeRoot(dataDir), CORE_VERSION);
  const venv = path.join(home, 'venv');
  const windows = process.platform === 'win32';
  const bin = path.join(venv, windows ? 'Scripts' : 'bin', windows ? 'sidevoice-core.exe' : 'sidevoice-core');
  const python = path.join(venv, windows ? 'Scripts' : 'bin', windows ? 'python.exe' : 'python');
  const marker = path.join(home, 'installed.json');
  const installed = () => { try { return executable(bin) && JSON.parse(readFileSync(marker, 'utf8')).spec === identity; } catch { return false; } };
  if (installed()) return bin;
  const uv = findUv(env);
  if (!uv) throw new Error(NO_UV);
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  const release = await takeInstallLock(dataDir, log);
  try {
    if (installed()) return bin;   // whoever held the lock installed this very spec
    mkdirSync(home, { recursive: true, mode: 0o700 });
    log(`installing sidevoice-core ${CORE_VERSION} with ${uv} from ${spec} (first time only; output in ${logPath(dataDir)})`);
    const started = Date.now();
    const options = { log: logPath(dataDir), env: { ...env, UV_NO_PROGRESS: '1' }, progress };
    await run(uv, ['venv', '--clear', '--python', '3.12', venv], options);
    await run(uv, ['pip', 'install', '--python', python, spec], options);
    if (!executable(bin)) throw new Error(`uv installed ${spec} but there is no ${bin}; see ${logPath(dataDir)}`);
    writeFileSync(marker, JSON.stringify({ version: CORE_VERSION, spec: identity, at: new Date().toISOString() }), { mode: 0o600 });
    // One current copy: the versions this connector no longer pins are disposable.
    for (const name of readdirSync(runtimeRoot(dataDir))) {
      if (name !== CORE_VERSION) rmSync(path.join(runtimeRoot(dataDir), name), { recursive: true, force: true });
    }
    log(`sidevoice-core ${CORE_VERSION} installed in ${Math.round((Date.now() - started) / 1000)} s`);
    return bin;
  } finally { release(); }
}

/** The file the core dials the room with: this machine's pairing, followed by the core (it may not exist yet). */
export function roomCredentialPath(dataDir, env = process.env) {
  return env.SIDEVOICE_CREDENTIALS || path.join(dataDir, 'credentials.json');
}

/** Whether a started core answers: its discovery route, the one that needs no token. */
export async function coreAnswers(ready, timeout = 10_000) {
  try {
    const response = await fetch(new URL('/api/rendezvous', ready.url), { signal: AbortSignal.timeout(timeout) });
    return response.ok && (await response.json())?.kind === 'node';
  } catch { return false; }
}

/** Whether the pinned version's install is the one the spec names now: false after a wheel was rebuilt in place. */
function installCurrent(dataDir, env) {
  try { return JSON.parse(readFileSync(path.join(runtimeRoot(dataDir), CORE_VERSION, 'installed.json'), 'utf8')).spec === specIdentity(coreSpec(env)); }
  catch { return false; }
}

/** What the running core wrote about itself, or null. */
export function readReady(dataDir) {
  try {
    const ready = JSON.parse(readFileSync(path.join(coreData(dataDir), 'core.json'), 'utf8'));
    return ready && ready.pid && ready.url && ready.connector_id && ready.token ? ready : null;
  } catch { return null; }
}

/** Whether that pid is a live Sidevoice core — not merely a live pid: pids are reused. */
export function coreAlive(pid) {
  if (!pid) return false;
  try { process.kill(pid, 0); } catch (error) { if (error.code !== 'EPERM') return false; }
  try {
    const args = execFileSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8', timeout: 3000 }).trim();
    return /sidevoice[-_]core/.test(args);
  } catch { return true; }
}

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** The running core's ready file: the one already running, or one this call starts. A core of another
 *  version is asked to leave first — this connector links with the version it pins. */
export async function ensureRunning({ dataDir, env = process.env, log = () => {}, progress = () => {}, timeout = 120_000, roomCredential = roomCredentialPath(dataDir, env) }) {
  const running = readReady(dataDir);
  if (running && coreAlive(running.pid)) {
    if (env.SIDEVOICE_CORE_BIN) return running;
    const pinned = !running.version || running.version === CORE_VERSION;
    if (pinned && installCurrent(dataDir, env)) return running;
    log(pinned ? `the running core was installed from an earlier build of ${coreSpec(env)}: asking it to leave`
      : `the running core is ${running.version}, this connector pins ${CORE_VERSION}: asking it to leave`);
    try { process.kill(running.pid, 'SIGTERM'); } catch {}
    const deadline = Date.now() + 15_000;
    while (coreAlive(running.pid) && Date.now() < deadline) await wait(100);
  }
  const bin = await ensureInstalled({ dataDir, env, log, progress });
  const data = coreData(dataDir);
  mkdirSync(data, { recursive: true, mode: 0o700 });
  const out = openSync(logPath(dataDir), 'a', 0o600);
  // The machine's pairing is this connector's file (`pair.mjs` writes it); the core reads it to dial the room.
  const args = ['--data-dir', data, '--port', String(env.SIDEVOICE_CORE_PORT ?? DEFAULT_PORT)];
  if (roomCredential) args.push('--room-credential', roomCredential);
  const child = spawn(bin, args,
    { detached: true, stdio: ['ignore', out, out], env: { ...env, SIDEVOICE_CORE_DATA_DIR: data } });
  closeSync(out);
  let exited = null;
  child.on('exit', code => { exited = code ?? 'signal'; });
  child.on('error', error => { exited = error.message; });
  child.unref();
  log(`started sidevoice-core (pid ${child.pid}); waiting for ${path.join(data, 'core.json')}`);
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const ready = readReady(dataDir);
    if (ready && ready.pid === child.pid) return ready;
    if (exited !== null) break;
    await wait(100);
  }
  let tail = '';
  try { tail = readFileSync(logPath(dataDir), 'utf8').trim().split('\n').slice(-3).join(' | '); } catch {}
  throw new Error(`sidevoice-core did not start (${exited !== null ? 'it exited: ' + exited : 'no ready file in time'})${tail ? ': ' + tail : ''}`);
}
