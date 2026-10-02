/** This machine's core — installed from the signed R4-a platform bundle, or as an attested wheel with uv on an
 *  unsupported platform. Source checkouts retain local wheel/spec overrides; an installed connector accepts an
 *  explicit local path but never a network requirement as a developer override. `SIDEVOICE_CORE_BIN` names an
 *  executable installed by hand and skips installation altogether.
 *
 *  Each immutable runtime is stored under `core-runtime/<build>` and linked by a release (`release.mjs`). Models
 *  are not part of it; the core loads Silero and smart-turn from its wheels, and nothing else unless a person picks
 *  a local engine.
 *
 *  Who starts it (§2.6): the service manager, as the core job (`service.mjs`) — then nobody else does; with no
 *  job, a connector, detached and left running — that core outlives its connector on purpose (a call may be
 *  going on) and exits on its own when nothing has used it for a while. The handshake is the ready file,
 *  `core/core.json`: where it listens (its socket, `core-socket.mjs`), its launch id, and the credential the
 *  connector links with. A core that died before serving says why in `core/core-failure.json`; it writes its own
 *  log, `core.log`, and one data directory has one core (its `flock` on `core/core.lock`). */
import { spawn, execFileSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { accessSync, constants, existsSync, mkdirSync, openSync, closeSync, readFileSync, rmSync, statSync, writeFileSync, writeSync, renameSync, readdirSync, fsyncSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ensureCoreDirectory, localHealth } from './core-socket.mjs';
import { installLockPath, readLock, takeLock } from './lockfile.mjs';
import { findProcess, isProcess, signalVerified } from './proc.mjs';
import { keyed, t } from './i18n.mjs';
import { nodeFiles, readJson } from './node-files.mjs';
import { BUILD_PACKAGE, BUILD_PACKAGE_DIR, CORE_MANIFEST } from './build-info.mjs';
import { runningAsSea } from './sea-runtime.mjs';
import { coreTarget, fetchVerifiedCoreWheel, prepareVerifiedCoreBundle, validateCoreManifest } from './core-bundle.mjs';
import { refusal, sha256 } from './core-attestation.mjs';
import { verifyPrivateDir, writePrivateFile } from './secure-fs.mjs';

export const CORE_VERSION = '0.1.0';
export const DEFAULT_PORT = 8768;
/** The client surface (`api`) and the connector link (`link`, the `protocol` in `core.json`) this connector
 *  speaks (§4.3): a core outside either range is not one it can use. */
export const API_RANGE = [1, 1];
export const LINK_RANGE = [2, 2];
const here = path.dirname(fileURLToPath(import.meta.url));
const INSTALL_TIMEOUT_MS = 20 * 60_000;
/** How long a started core has to be ready: its ready file with this launch id, and its health answering. */
export const READY_TIMEOUT_MS = Number(process.env.SIDEVOICE_CORE_READY_MS || 60_000);
/** How long a core asked to leave (SIGTERM) has before it is killed. */
export const STOP_GRACE_MS = Number(process.env.SIDEVOICE_CORE_STOP_GRACE_MS || 15_000);

export const NO_UV = 'Sidevoice installs its core with uv, and uv is not on this machine. Install it '
  + '(macOS/Linux: curl -LsSf https://astral.sh/uv/install.sh | sh — or brew install uv) and ask for voice again.';

export function coreData(dataDir) { return path.join(dataDir, 'core'); }
export function runtimeRoot(dataDir) { return path.join(dataDir, 'core-runtime'); }
export function logPath(dataDir) { return path.join(dataDir, 'core.log'); }
export function socketPathOf(dataDir) { return path.join(coreData(dataDir), 'local.sock'); }
export function failurePath(dataDir) { return path.join(coreData(dataDir), 'core-failure.json'); }

/** What `uv pip install` is given for the pinned version. */
export function coreSpec(env = process.env) {
  if (env.SIDEVOICE_CORE_SPEC) return env.SIDEVOICE_CORE_SPEC;
  const wheel = path.join(env.SIDEVOICE_CORE_WHEEL_DIR || path.join(here, 'core'), `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`);
  return existsSync(wheel) ? wheel : `sidevoice-core==${CORE_VERSION}`;
}

/** Whether this build carries a signed bundle for the current platform and can install its core without uv. */
export function hasEmbeddedCoreBundle(manifest = CORE_MANIFEST, target = coreTarget()) {
  if (!manifest || !target) return false;
  validateCoreManifest(manifest, CORE_VERSION);
  return manifest.bundles.some(item => item.os === target.os && item.arch === target.arch);
}

function sourceCheckout(env = process.env) {
  if (runningAsSea()) return false;
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '0') return false;
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '1') return true;
  const packageDir = existsSync(path.join(BUILD_PACKAGE_DIR, '..', 'package.json'))
    ? path.dirname(BUILD_PACKAGE_DIR) : BUILD_PACKAGE_DIR;
  return existsSync(path.join(packageDir, '..', '..', '.git'));
}

function localSpecPath(spec) {
  if (typeof spec !== 'string' || !spec) return null;
  if (spec.startsWith('file://')) {
    try { return fileURLToPath(spec); } catch { return null; }
  }
  if (path.isAbsolute(spec)) return spec;
  if (spec.startsWith('./') || spec.startsWith('../')) return path.resolve(spec);
  return null;
}

function localDeveloperSpec(env = process.env) {
  const explicit = env.SIDEVOICE_CORE_SPEC;
  if (explicit) {
    const local = localSpecPath(explicit);
    if (!local) throw refusal('developer-override', 'SIDEVOICE_CORE_SPEC may name only a local path; network specs require a verified manifest');
    if (sourceCheckout(env) || existsSync(local)) return explicit.startsWith('file://') ? local : explicit;
    throw refusal('developer-override', 'the local SIDEVOICE_CORE_SPEC path does not exist');
  }
  const spec = coreSpec(env);
  const local = localSpecPath(spec);
  if (local && existsSync(local) && (sourceCheckout(env) || env.SIDEVOICE_CORE_WHEEL_DIR)) return local;
  return null;
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


/** Who is installing right now, if anyone (`install.lock`'s record — information only): enough to wait for an
 *  install and say whose it is. */
export function installInProgress(dataDir) {
  const lock = readLock(installLockPath(dataDir));
  if (!lock || !isProcess(lock.pid, { start: lock.start ?? null })) return null;
  let last = null;
  try { last = readFileSync(logPath(dataDir), 'utf8').trim().split('\n').filter(line => line.trim()).at(-1)?.trim() || null; } catch {}
  const since = Date.parse(lock.at) || Date.now();
  return { pid: lock.pid ?? null, since, seconds: Math.round((Date.now() - since) / 1000), last, log: logPath(dataDir) };
}
/** The install lock, waited for (`lockfile.mjs`): two uv runs into one runtime would break it. */
export function takeInstallLock(dataDir, log = () => {}, options = {}) {
  return takeLock(installLockPath(dataDir), { kind: 'install', log, ...options });
}

/** One runtime per build of the core, never changed once made: `core-runtime/<version>-<hash of the spec>` (a wheel's
 *  identity includes its size and time, so a wheel rebuilt in place is another build). An install never clears a
 *  runtime something may be running from: a new build goes to a new directory, and one no release links to is
 *  pruned by the installer (`release.mjs`). `SIDEVOICE_CORE_BIN` names a core installed by hand: no runtime of ours. */
export function runtimeIdentity(env = process.env, verifiedSha256 = null) {
  if (env.SIDEVOICE_CORE_BIN) return { id: 'external', spec: null };
  const spec = coreSpec(env);
  if (verifiedSha256 !== null) {
    if (!/^[0-9a-f]{64}$/.test(verifiedSha256)) throw refusal('sha256', 'the verified wheel digest is malformed');
    return { id: `${CORE_VERSION}-wheel-${verifiedSha256}`, spec };
  }
  return { id: `${CORE_VERSION}-${createHash('sha256').update(specIdentity(spec)).digest('hex').slice(0, 12)}`, spec };
}
export function runtimePaths(dataDir, id) {
  const home = path.join(runtimeRoot(dataDir), id), venv = path.join(home, 'venv');
  const windows = process.platform === 'win32';
  return { home, venv, marker: path.join(home, 'installed.json'),
    bin: path.join(venv, windows ? 'Scripts' : 'bin', windows ? 'sidevoice-core.exe' : 'sidevoice-core'),
    python: path.join(venv, windows ? 'Scripts' : 'bin', windows ? 'python.exe' : 'python'),
    bundlePython: path.join(home, 'python', 'bin', process.platform === 'win32' ? 'python.exe' : 'python3'),
    bundleMarker: path.join(home, '.sidevoice-runtime.json') };
}
export function verifiedWheelCachePath(dataDir, sha256) {
  if (!/^[0-9a-f]{64}$/.test(sha256)) throw refusal('sha256', 'the verified wheel digest is malformed');
  return path.join(dataDir, 'core-wheel-cache', sha256, `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`);
}
const runtimeComplete = paths => {
  try {
    if (executable(paths.bundlePython) && JSON.parse(readFileSync(paths.bundleMarker, 'utf8')).kind === 'bundle') return true;
    return executable(paths.bin) && !!JSON.parse(readFileSync(paths.marker, 'utf8')).spec;
  } catch { return false; }
};

/** This package's build of the core, installed if it is not there: `{id, bin, venv}` (`venv` null for an external one). */
export async function installRuntime({ dataDir, env = process.env, log = () => {}, progress = () => {}, verifiedSha256 = null }) {
  if (env.SIDEVOICE_CORE_BIN) return { id: 'external', bin: env.SIDEVOICE_CORE_BIN, venv: null };
  const { id, spec } = runtimeIdentity(env, verifiedSha256);
  if (!localSpecPath(spec)) throw refusal('developer-override', 'unverified network core specifications are disabled');
  const paths = runtimePaths(dataDir, id);
  if (runtimeComplete(paths)) return { id, bin: paths.bin, venv: paths.venv, kind: 'uv' };
  const uv = findUv(env);
  if (!uv) throw new Error(NO_UV);
  const release = await takeInstallLock(dataDir, log);
  try {
    if (runtimeComplete(paths)) return { id, bin: paths.bin, venv: paths.venv, kind: 'uv' };   // whoever held the lock installed this very build
    // A directory of this build with no marker is an install of it that did not finish: nothing links to or runs it.
    rmSync(paths.home, { recursive: true, force: true });
    mkdirSync(paths.home, { recursive: true, mode: 0o700 });
    log(`installing sidevoice-core ${CORE_VERSION} with ${uv} from ${spec} (first time only; output in ${logPath(dataDir)})`);
    const started = Date.now();
    const options = { log: logPath(dataDir), env: { ...env, UV_NO_PROGRESS: '1' }, progress };
    // uv's own Python, never the machine's: the core then runs on one known build everywhere, whatever a
    // system Python, Homebrew or a version manager left on the PATH.
    await run(uv, ['venv', '--python-preference', 'only-managed', '--python', '3.12', paths.venv], options);
    await run(uv, ['pip', 'install', '--python', paths.python, spec], options);
    if (!executable(paths.bin)) throw new Error(`uv installed ${spec} but there is no ${paths.bin}; see ${logPath(dataDir)}`);
    writeFileSync(paths.marker, JSON.stringify({ version: CORE_VERSION, id, spec: specIdentity(spec), at: new Date().toISOString() }), { mode: 0o600 });
    log(`sidevoice-core ${CORE_VERSION} installed in ${Math.round((Date.now() - started) / 1000)} s`);
    return { id, bin: paths.bin, venv: paths.venv, kind: 'uv' };
  } finally { release(); }
}

function syncRuntimeTree(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) syncRuntimeTree(full);
    else if (entry.isFile()) { const fd = openSync(full, 'r'); try { fsyncSync(fd); } finally { closeSync(fd); } }
  }
  const fd = openSync(dir, 'r'); try { fsyncSync(fd); } finally { closeSync(fd); }
}

async function installVerifiedBundle({ dataDir, env, log, progress, channel, target }) {
  validateCoreManifest(CORE_MANIFEST, CORE_VERSION);
  const entry = CORE_MANIFEST.bundles.find(item => item.os === target.os && item.arch === target.arch);
  if (!entry) throw refusal('platform', `the embedded manifest has no core bundle for ${target.os}/${target.arch}`);
  const id = `${CORE_VERSION}-bundle-${entry.sha256.slice(0, 12)}`;
  const paths = runtimePaths(dataDir, id);
  if (runtimeComplete(paths)) return { id, bin: paths.bundlePython, venv: null, kind: 'bundle', root: paths.home };
  verifyPrivateDir(runtimeRoot(dataDir), { create: true });
  const release = await takeInstallLock(dataDir, log);
  const temporary = `${paths.home}.staging-${randomUUID()}`;
  try {
    if (runtimeComplete(paths)) return { id, bin: paths.bundlePython, venv: null, kind: 'bundle', root: paths.home };
    rmSync(paths.home, { recursive: true, force: true });
    mkdirSync(temporary, { mode: 0o700 });
    progress(t('install.progress.bundle', { platform: `${target.os}/${target.arch}` }, env));
    const prepared = await prepareVerifiedCoreBundle({ manifest: CORE_MANIFEST, coreVersion: CORE_VERSION, target,
      directory: temporary, channel, tufCachePath: path.join(dataDir, 'sigstore') });
    const python = path.join(prepared.payload, 'python', 'bin', 'python3');
    if (!executable(python)) throw keyed('install.self-test', { detail: `the verified bundle has no executable ${python}` });
    selfTest(python, env, { bundle: true });
    writePrivateFile(path.join(prepared.payload, '.sidevoice-runtime.json'), JSON.stringify({ kind: 'bundle', id,
      core: CORE_VERSION, sha256: entry.sha256, at: new Date().toISOString() }) + '\n');
    syncRuntimeTree(prepared.payload);
    renameSync(prepared.payload, paths.home);
    const parent = openSync(runtimeRoot(dataDir), 'r'); try { fsyncSync(parent); } finally { closeSync(parent); }
    return { id, bin: paths.bundlePython, venv: null, kind: 'bundle', root: paths.home };
  } finally {
    rmSync(temporary, { recursive: true, force: true });
    release();
  }
}

async function installVerifiedWheel({ dataDir, env, log, progress, channel }) {
  validateCoreManifest(CORE_MANIFEST, CORE_VERSION);
  const uv = findUv(env);
  if (!uv) throw keyed('install.no-bundle', { platform: `${process.platform}/${process.arch}` });
  verifyPrivateDir(runtimeRoot(dataDir), { create: true });
  const entry = CORE_MANIFEST.wheel;
  const wheelPath = verifiedWheelCachePath(dataDir, entry.sha256);
  const cacheDir = path.dirname(wheelPath);
  verifyPrivateDir(cacheDir, { create: true });
  const wheelEnv = { ...env, SIDEVOICE_CORE_SPEC: wheelPath };
  const cachedIdentity = runtimeIdentity(wheelEnv, entry.sha256);
  const cachedPaths = runtimePaths(dataDir, cachedIdentity.id);
  if (runtimeComplete(cachedPaths)) return { id: cachedIdentity.id, bin: cachedPaths.bin, venv: cachedPaths.venv, kind: 'uv' };

  const temporary = path.join(dataDir, `.wheel-download-${process.pid}-${randomUUID()}`);
  mkdirSync(temporary, { mode: 0o700 });
  try {
    let cached = false;
    try { cached = sha256(readFileSync(wheelPath)) === entry.sha256; } catch {}
    if (!cached) {
      rmSync(wheelPath, { force: true });
      progress(t('install.progress.wheel', {}, env));
      const wheel = await fetchVerifiedCoreWheel({ manifest: CORE_MANIFEST, coreVersion: CORE_VERSION, directory: temporary,
        channel, tufCachePath: path.join(dataDir, 'sigstore') });
      renameSync(wheel.path, wheelPath);
      const parent = openSync(cacheDir, 'r'); try { fsyncSync(parent); } finally { closeSync(parent); }
    }
    return await installRuntime({ dataDir, env: wheelEnv, log, progress, verifiedSha256: entry.sha256 });
  } finally { rmSync(temporary, { recursive: true, force: true }); }
}

/** One selected core source: explicit local developer override, platform bundle, or verified-wheel uv fallback. */
export async function installCoreRuntime({ dataDir, env = process.env, log = () => {}, progress = () => {}, channel = BUILD_PACKAGE.sidevoice?.channel || 'release' }) {
  if (env.SIDEVOICE_CORE_BIN) return { id: 'external', bin: env.SIDEVOICE_CORE_BIN, venv: null, kind: 'external' };
  const override = localDeveloperSpec(env);
  if (override) return { ...(await installRuntime({ dataDir, env: { ...env, SIDEVOICE_CORE_SPEC: override }, log, progress })), kind: 'uv' };
  if (!CORE_MANIFEST) throw refusal('manifest', 'this executable has no R4-a core manifest embedded');
  const target = coreTarget();
  if (target) return installVerifiedBundle({ dataDir, env, log, progress, channel, target });
  return installVerifiedWheel({ dataDir, env, log, progress, channel });
}

/** The core program this package would run with no installation selected: named, or its verified build — installed now if needed. */
export async function ensureInstalled(options) {
  return (await installCoreRuntime(options)).bin;
}

export function isBundleCore(bin) {
  return typeof bin === 'string' && bin.endsWith(path.join('python', 'bin', 'python3'));
}

/** `sidevoice-core --self-test`: imports everything serving needs, binds nothing, writes nothing. */
export function selfTest(bin, env = process.env, { bundle = false } = {}) {
  let output = '';
  const args = bundle ? ['-I', '-m', 'sidevoice_core.server', '--self-test'] : ['--self-test'];
  try { output = execFileSync(bin, args, { encoding: 'utf8', timeout: 120_000, env, stdio: ['ignore', 'pipe', 'pipe'] }); }
  catch (error) { output = String(error.stdout || ''); if (!output.trim()) throw keyed('install.self-test', { detail: String(error.stderr || error.message).trim().split('\n').at(-1) }); }
  let report = null; try { report = JSON.parse(output.trim().split('\n').at(-1)); } catch {}
  if (!report?.ok) throw keyed(report?.key || 'install.self-test', { detail: report?.message || output.trim().slice(0, 200) });
  return report;
}

/** The file the core dials the room with: this machine's pairing, followed by the core (it may not exist yet). */
export function roomCredentialPath(dataDir, env = process.env) {
  return env.SIDEVOICE_CREDENTIALS || path.join(dataDir, 'credentials.json');
}

/** What the running core wrote about itself, or null. */
export function readReady(dataDir) {
  try {
    const ready = JSON.parse(readFileSync(path.join(coreData(dataDir), 'core.json'), 'utf8'));
    return ready && ready.pid && ready.socket && ready.connector_id && ready.token ? ready : null;
  } catch { return null; }
}

/** A core of this data directory, by its command line: `--data-dir <C>`. A pid alone, or a process that cannot be
 *  examined, is not a core of ours — pids are reused, and a stale ready file can name somebody else's work. */
export const corePattern = dataDir => new RegExp(`--data-dir[= ]${coreData(dataDir).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}(\\s|$)`);
export function coreRunning(dataDir, pid) {
  return isProcess(pid, { command: corePattern(dataDir) });
}
/** Every core of this data directory still alive: the one its ready file names, and one still starting (no ready file
 *  yet), found by its command line. */
export function coreProcesses(dataDir) {
  const pids = new Set();
  const ready = readReady(dataDir);
  if (ready && coreRunning(dataDir, ready.pid)) pids.add(ready.pid);
  const starting = findProcess(corePattern(dataDir));
  if (starting) pids.add(starting);
  return [...pids];
}

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** Whether a core speaks what this connector speaks: `api` from its health, `link` from its ready file. */
export function compatible(core) {
  const within = (value, [low, high]) => Number.isInteger(value) && value >= low && value <= high;
  return within(core?.api, API_RANGE) && within(core?.protocol, LINK_RANGE);
}

/** The core serving now: its ready file, and its health on the socket answering 200 for that same launch within
 *  `timeout`. Then the two together, else null. Alive is not serving: a stale ready file outlives a SIGKILLed core,
 *  and a wedged core is alive. */
export async function serving(dataDir, timeout = 2000) {
  const ready = readReady(dataDir);
  if (!ready) return null;
  const health = await localHealth(ready.socket, timeout);
  if (health?.status !== 200 || (ready.launch_id && health.body?.launch_id !== ready.launch_id)) return null;
  return { ...ready, ...health.body, protocol: ready.protocol };
}

/** The arguments of one core: the job's (`idleExit` 0: it never leaves on its own; no launch id — the core makes one)
 *  or an on-demand launch's (its default idle exit, and a launch id to wait for). */
export function coreArgs({ dataDir, env = process.env, launchId = null, idleExit = null, roomCredential = null, bundle = false }) {
  const args = ['--data-dir', coreData(dataDir), '--port', String(env.SIDEVOICE_CORE_PORT ?? DEFAULT_PORT), '--socket', socketPathOf(dataDir)];
  if (launchId) args.push('--launch-id', launchId);
  if (idleExit !== null) args.push('--idle-exit', String(idleExit));
  if (roomCredential) args.push('--room-credential', roomCredential);
  return bundle ? ['-I', '-m', 'sidevoice_core.server', ...args] : args;
}

/** Start one detached core (no service manager): its stdout and stderr appended to `core.stderr.log` — the core writes
 *  its own log — and a handle whose `exit` settles when it is gone, a spawn error (no such program, no permission)
 *  included. */
export function spawnCore(bin, args, { dataDir, env = process.env }) {
  ensureCoreDirectory(coreData(dataDir));
  let child;
  const out = openSync(nodeFiles(dataDir).coreStderr, 'a', 0o600);
  try { child = spawn(bin, args, { detached: true, stdio: ['ignore', out, out], env: { ...env, SIDEVOICE_CORE_DATA_DIR: coreData(dataDir) } }); }
  finally { closeSync(out); }
  const handle = { pid: child.pid ?? null, child, done: null };
  handle.exit = new Promise(resolve => {
    child.once('exit', (code, signal) => resolve(handle.done = { code, signal }));
    child.once('error', error => resolve(handle.done = { error: { code: error.code, message: error.message, path: bin } }));
  });
  child.unref();
  return handle;
}

/** The last lines of `core.log`, for a failure a person reads. */
export function logTail(dataDir, lines = 10) {
  try { return readFileSync(logPath(dataDir), 'utf8').trimEnd().split('\n').slice(-lines); } catch { return []; }
}

/** What the core reported about its last failed start, or null: written by the core before ready, deleted by the core
 *  once it holds its lock — so one present while no core runs is the last start's. */
export function readFailure(dataDir) {
  try {
    const report = readJson(failurePath(dataDir));
    return report?.key ? report : null;
  } catch { return null; }
}

/** A failure as a person reads it: the core's own report, or a key of the connector's, with the log tail. */
export function describeFailure(dataDir, report) {
  const detail = report.key === 'import.missing-module' ? (String(report.message).match(/module named '?([\w.]+)/)?.[1] ?? report.message) : (report.detail ?? null);
  return { key: report.key, step: report.step || null, message: report.message ?? keyed(report.key, { detail: detail ?? '' }).message, detail, at: report.at || new Date().toISOString(), log_tail: logTail(dataDir) };
}

/** Why an on-demand launch did not come up: the core's own report for this launch; else the spawn error
 *  (`launch.missing-executable`, `launch.permission`); else `launch.exited` with its exit code. */
export function failureCause({ dataDir, launchId, exit, key = null }) {
  const report = key ? null : readFailure(dataDir);
  if (report && (!launchId || report.launch_id === launchId)) return describeFailure(dataDir, report);
  const describe = (cause, step, detail) => describeFailure(dataDir, { key: cause, step, detail });
  if (key) return describe(key, 'ready', null);
  if (exit?.error) {
    if (exit.error.code === 'ENOENT') return describe('launch.missing-executable', 'spawn', exit.error.path);
    if (exit.error.code === 'EACCES' || exit.error.code === 'EPERM') return describe('launch.permission', 'spawn', exit.error.path);
    return describe('launch.exited', 'spawn', exit.error.message);
  }
  return describe('launch.exited', 'run', exit?.code ?? exit?.signal ?? null);
}

/** Ask a core of this data directory to leave and wait until it has: SIGTERM, `STOP_GRACE_MS`, SIGKILL — each sent
 *  only while it is provably that core (`coreRunning`). */
export async function terminateCore(dataDir, pid, { grace = STOP_GRACE_MS, log = () => {} } = {}) {
  const gone = () => !coreRunning(dataDir, pid);
  if (!pid || gone()) return;
  signalVerified(pid, 'SIGTERM', { command: corePattern(dataDir) });
  const deadline = Date.now() + grace;
  while (!gone() && Date.now() < deadline) await wait(50);
  if (gone()) return;
  log(`the core (pid ${pid}) did not leave within ${Math.round(grace / 1000)} s; killing it`);
  signalVerified(pid, 'SIGKILL', { command: corePattern(dataDir) });
  const after = Date.now() + 5000;
  while (!gone() && Date.now() < after) await wait(50);
}

/** Wait for one launch to be ready, within `READY_TIMEOUT_MS`: its ready file and health, or why not. */
export async function awaitReady(handle, { dataDir, launchId, timeout = READY_TIMEOUT_MS }) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (handle.done) return { failure: failureCause({ dataDir, launchId, exit: handle.done }) };
    const ready = readReady(dataDir);
    if (ready?.launch_id === launchId) { const live = await serving(dataDir); if (live) return { ready: live }; }
    await wait(100);
  }
  return { failure: failureCause({ dataDir, launchId, key: 'ready.timeout' }) };
}

/** The core serving this data directory, without a service manager: the one already serving, or one this call starts,
 *  detached, from `bin` (the selected release's; else this package's build, installed now if need be). Another core
 *  still starting holds the directory (its `flock`): this launch is refused with `bind.core-running`, and that one is
 *  waited for instead. Nothing here unlinks a socket: only the core holding the directory replaces its own.
 *  `fresh`: only a core this call starts will do (an installer verifying a selection): one already serving, or another
 *  holding the directory, is `install.not-selected` — never taken for the one asked for. */
export async function ensureRunning({ dataDir, env = process.env, log = () => {}, progress = () => {}, roomCredential = roomCredentialPath(dataDir, env), bin = null, fresh = false }) {
  ensureCoreDirectory(coreData(dataDir));
  const running = await serving(dataDir);
  if (running && fresh) throw keyed('install.not-selected', { detail: `a core of another launch (pid ${running.pid}) still serves` });
  if (running) return running;
  const program = bin || await ensureInstalled({ dataDir, env, log, progress });
  const launchId = randomUUID();
  const handle = spawnCore(program, coreArgs({ dataDir, env, launchId, roomCredential, bundle: isBundleCore(program) }), { dataDir, env });
  log(`started sidevoice-core (pid ${handle.pid ?? '?'}, launch ${launchId}); waiting for it to be ready`);
  let { ready, failure } = await awaitReady(handle, { dataDir, launchId });
  if (!ready && failure.key === 'bind.core-running' && !fresh) {
    log('another core holds this data directory: waiting for it instead');
    const deadline = Date.now() + READY_TIMEOUT_MS;
    while (!ready && Date.now() < deadline) { ready = await serving(dataDir); if (!ready) await wait(200); }
    if (!ready) failure = failureCause({ dataDir, launchId: null, key: 'ready.timeout' });
  }
  if (ready) return ready;
  if (failure.key === 'ready.timeout' && handle.pid) await terminateCore(dataDir, handle.pid, { log });
  throw Object.assign(keyed(failure.key, { detail: failure.detail ?? '' }), { failure });
}
