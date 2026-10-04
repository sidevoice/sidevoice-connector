/** What is installed, and which of it runs (§2.4, SEAMS §1): immutable release directories and two links.
 *
 *    R/releases/<id>/   immutable once renamed into place: `dist/` (the distributor SEA), `core` (the selected Core),
 *                       `release.json` with separate distributor format, daemon runtime, Core and pair identities
 *    R/current  → releases/<id>   the selection
 *    R/verified → releases/<id>   the last release seen running (written only after a verification)
 *    R/previous → releases/<id>   what was verified before the selection: the rollback target
 *
 *  `R` is `$XDG_DATA_HOME/sidevoice`. Everything that runs Sidevoice — both job definitions, every harness
 *  registration, `install.json`'s `command` — names a path through `R/current`; a format switch repoints owned
 *  registrations while holding the install lock. Switching is replacing one link, `symlink(tmp)` + `rename(tmp, link)`
 *  (never `ln -sfn`, which unlinks first on macOS), then `fsync(R)`. A crash anywhere leaves `current` naming a complete release, old or new. A
 *  process started through `current` keeps running its own release after a switch (Node runs a module by its real
 *  path; the core's console script names its real venv), and the next start runs the new one.
 *
 *  `install` holds the install lock throughout (`lockfile.mjs`). There is no journal: whatever a dead installer left
 *  is recognised by name and deleted (`*.tmp*`), and re-running `install` verifies the selection and flips back if it
 *  does not run — the recovery. */
import path from 'node:path';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import { closeSync, cpSync, copyFileSync, chmodSync, existsSync, fsyncSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, realpathSync, renameSync, rmSync, symlinkSync } from 'node:fs';
import { createHash, randomBytes } from 'node:crypto';
import { CORE_VERSION, embeddedRustCoreIdentity, installCoreRuntime, isBundleCore, runtimeRoot, selfTest } from './core.mjs';
import { RUST_CORE_ENTRYPOINT, RUST_CORE_KIND, RUST_CORE_TARGETS, rustCoreTarget } from './rust-core.mjs';
import { keyed, t } from './i18n.mjs';
import { recordedInstallation } from './node-files.mjs';
import { readTrustedJson, verifyPrivateDir, writePrivateFile } from './secure-fs.mjs';
import { crash } from './testpoint.mjs';
import { BUILD_PACKAGE, BUILD_PACKAGE_DIR } from './build-info.mjs';
import { runningAsSea } from './sea-runtime.mjs';

const here = BUILD_PACKAGE_DIR;
const pairIdentity = (runtimeKind, runtimeBuild, coreBuild) => `pair-v1:${runtimeKind}:${runtimeBuild}:core:${coreBuild}`;
/** Where the package's own root is: next to these modules in the checkout, and one level up once they have been
 *  bundled into `dist/`. */
const packageRoot = () => (existsSync(path.join(here, '..', 'package.json')) ? path.dirname(here) : here);
/** `R`: where the installation recorded it was made (`install.json`'s `releases`), else `$XDG_DATA_HOME/sidevoice`. */
export function releaseRoot(env = process.env) {
  const recorded = recordedInstallation(env)?.releases;
  if (typeof recorded === 'string' && path.isAbsolute(recorded) && path.basename(recorded) === 'sidevoice') return recorded;
  return path.join(env.XDG_DATA_HOME || path.join(env.HOME || os.homedir(), '.local', 'share'), 'sidevoice');
}
export function releaseLayout(env = process.env) {
  const root = releaseRoot(env);
  return { root, releases: path.join(root, 'releases'), current: path.join(root, 'current'), previous: path.join(root, 'previous'), verified: path.join(root, 'verified') };
}
/** The program each job and harness runs for the selected release. Older metadata without a format is R1's ESM. */
export function stableCommand(env = process.env) {
  const selected = selection(env, 'current')?.release;
  const format = selected?.format ?? (!selected && runningAsSea() ? 'sea' : 'esm');
  if (format === 'sea') return [path.join(releaseLayout(env).current, 'dist', 'sidevoice')];
  const installed = recordedInstallation(env);
  const legacyNode = Array.isArray(installed?.command) && installed.command.length === 2
    && path.isAbsolute(installed.command[0]) && path.basename(installed.command[1]) === 'cli.mjs' ? installed.command[0] : null;
  const node = installed?.nodeExecutable || legacyNode || (!runningAsSea() ? process.execPath : null);
  if (!node) throw keyed('install.node-runtime-missing');
  return [node, path.join(releaseLayout(env).current, 'dist', 'cli.mjs')];
}
export function coreProgram(env = process.env) {
  const selected = selection(env, 'current')?.release;
  if (selected?.core_kind === RUST_CORE_KIND) {
    const target = selected.core_target;
    const coreId = `${RUST_CORE_KIND}-${target}-${selected.core_source_sha}-${selected.core_archive_sha256}`;
    const complete = RUST_CORE_TARGETS.includes(target) && target === rustCoreTarget()
      && /^[0-9a-f]{40}$/.test(selected.core_source_sha || '')
      && /^[0-9a-f]{64}$/.test(selected.core_cargo_lock_sha256 || '')
      && /^[0-9a-f]{64}$/.test(selected.core_manifest_sha256 || '')
      && /^[0-9a-f]{64}$/.test(selected.core_archive_sha256 || '')
      && Number.isSafeInteger(selected.core_archive_size) && selected.core_archive_size > 0
      && selected.core_build === coreId && selected.core_entrypoint === RUST_CORE_ENTRYPOINT
      && selected.core === CORE_VERSION && selected.runtime_kind === 'javascript' && selected.format === 'sea'
      && /^[0-9a-f]{64}$/.test(selected.runtime_sha256 || '')
      && (selected.runtime_build_sha === null || /^[0-9a-f]{40}$/.test(selected.runtime_build_sha || ''))
      && selected.pair_id === pairIdentity('javascript', selected.runtime_sha256, coreId);
    const binary = path.join(releaseLayout(env).current, 'core', RUST_CORE_ENTRYPOINT);
    let executable = false;
    try {
      const stat = lstatSync(binary);
      executable = stat.isFile() && !stat.isSymbolicLink() && !!(stat.mode & 0o111);
    } catch {}
    if (!complete || !executable) throw keyed('install.not-selected', { detail: 'the selected native Core release identity or entrypoint is invalid' });
    return binary;
  }
  if (selected?.core_kind && !['python-bundle', 'python-wheel', 'external'].includes(selected.core_kind)) {
    throw keyed('install.not-selected', { detail: `unknown selected Core kind ${selected.core_kind}` });
  }
  // Keep an explicitly selected legacy release runnable through rollback after the native package is launched.
  if (selected) {
    const root = path.join(releaseLayout(env).current, 'core');
    const bundled = path.join(root, 'python', 'bin', 'python3');
    if (selected.core_kind === 'python-bundle' || (!selected.core_kind && existsSync(bundled))) return bundled;
    return path.join(root, 'bin', 'sidevoice-core');
  }
  // With no selection, an embedded native SEA never falls through to Python.
  const embedded = embeddedRustCoreIdentity();
  const root = path.join(releaseLayout(env).current, 'core');
  if (embedded) return path.join(root, 'bin', RUST_CORE_ENTRYPOINT.split('/').at(-1));
  const bundled = path.join(root, 'python', 'bin', 'python3');
  return existsSync(bundled) ? bundled : path.join(root, 'bin', 'sidevoice-core');
}

/** A checkout installs itself: its release links to it instead of copying it. */
export function fromSource(env = process.env) {
  if (runningAsSea()) return false;
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '0') return false;
  return env.SIDEVOICE_INSTALL_FROM_SOURCE === '1' || existsSync(path.join(packageRoot(), '..', '..', '.git'));
}

/** The release this package would make: its version, channel and build (`sidevoice` in the manifest, stamped by CI;
 *  a checkout's is `source`). A nightly carries the version of the last release, so its id names its build. */
export function candidate(env = process.env) {
  const { version, sidevoice = {} } = BUILD_PACKAGE;
  const source = fromSource(env);
  const format = runningAsSea() ? 'sea' : 'esm';
  const channel = source ? 'source' : sidevoice.channel || 'release';
  const build_seq = Number(sidevoice.build_seq) || 0;
  const baseId = channel === 'nightly' ? `${version}-nightly.${build_seq}` : source ? `${version}-source` : version;
  const nativeCore = embeddedRustCoreIdentity();
  const runtimeKind = 'javascript';
  const runtimeBuildSha = sidevoice.connector_sha || null;
  const runtimeSha256 = runningAsSea() ? createHash('sha256').update(readFileSync(process.execPath)).digest('hex') : null;
  const runtimeBuild = runtimeSha256 || runtimeBuildSha || `${channel}:${version}:${build_seq}`;
  const pair_id = pairIdentity(runtimeKind, runtimeBuild, nativeCore?.id ?? `python-${CORE_VERSION}`);
  const id = format === 'sea'
    ? nativeCore
      ? `${baseId}-jsd-${runtimeSha256.slice(0, 12)}-core-${nativeCore.target}-${nativeCore.archiveSha256.slice(0, 12)}-sea`
      : `${baseId}-sea`
    : baseId;
  return { id, connector: version, core: nativeCore?.version ?? CORE_VERSION,
    core_kind: nativeCore?.kind ?? null, core_build: nativeCore?.id ?? null,
    pair_id, runtime_kind: runtimeKind, runtime_build_sha: runtimeBuildSha, runtime_sha256: runtimeSha256,
    channel, build_seq, format, source: source ? packageRoot() : null };
}

const parts = version => String(version || '0').split(/[.+-]/).slice(0, 3).map(part => Number(part) || 0);
export function compareVersions(a, b) {
  const [x, y] = [parts(a), parts(b)];
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i] ? 1 : -1;
  return 0;
}

/** Whether `next` replaces `current`: `install`, `upgrade`, or `noop` (same or lower, or the same version and channel
 *  with no higher build). A checkout is its own channel: installing from one selects it. */
export function decide(current, next) {
  if (!current) return 'install';
  if (next.channel === 'source' || current.channel === 'source') return current.source === next.source && current.connector === next.connector ? 'noop' : 'upgrade';
  const order = compareVersions(next.connector, current.connector);
  if (order > 0) return 'upgrade';
  if (order < 0) return 'noop';
  if (next.channel === 'nightly' && current.channel === 'nightly'
      && Number(next.build_seq) < Number(current.build_seq)) return 'noop';
  const currentRuntime = current.runtime_kind ?? 'javascript';
  if (next.runtime_kind && next.runtime_kind !== currentRuntime) return 'upgrade';
  if (next.core_build && current.core_build !== next.core_build) return 'upgrade';
  if (next.channel === 'nightly' && next.build_seq > (Number(current.build_seq) || 0)) return 'upgrade';
  // A format change is a tie-breaker only within the same release ordering. It cannot move between channels or
  // replace a newer nightly with an older one.
  const sameBuild = current.channel === next.channel && (next.channel !== 'nightly' || Number(next.build_seq) === Number(current.build_seq));
  if (sameBuild && (current.format ?? 'esm') !== (next.format ?? 'esm')) return next.format === 'sea' ? 'upgrade' : 'noop';
  return 'noop';
}

/** What a link selects: `{id, dir, release}`, or null when there is no link or what it names is not a release. */
export function selection(env, name = 'current') {
  const link = releaseLayout(env)[name];
  let dir;
  try { lstatSync(link); dir = realpathSync(link); } catch { return null; }
  const release = readTrustedJson(path.join(dir, 'release.json'));
  return release?.id ? { id: release.id, dir, release } : null;
}

const syncDir = dir => { const fd = openSync(dir, 'r'); try { fsyncSync(fd); } finally { closeSync(fd); } };
/** Every file and directory under `dir` flushed, so a release renamed into place is whole after a power loss too. */
function syncTree(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) syncTree(full);
    else if (entry.isFile()) { const fd = openSync(full, 'r'); try { fsyncSync(fd); } finally { closeSync(fd); } }
  }
  syncDir(dir);
}

/** Point `name` (`current` or `previous`) at `releases/<id>`: a new link beside it, renamed over it. */
export function point(env, name, id) {
  const { root } = releaseLayout(env);
  const temporary = path.join(root, `.${name}.${process.pid}.${randomBytes(4).toString('hex')}.tmp`);
  symlinkSync(path.join('releases', id), temporary);
  renameSync(temporary, path.join(root, name));
  syncDir(root);
}

/** Step 1: what a dead installer left — a release still being staged, a link not yet renamed — is deleted by name. */
export function removeLeftovers(env) {
  const { root, releases } = releaseLayout(env);
  for (const name of safeList(releases)) if (name.includes('.tmp-')) rmSync(path.join(releases, name), { recursive: true, force: true });
  for (const name of safeList(root)) if (name.endsWith('.tmp')) rmSync(path.join(root, name), { recursive: true, force: true });
}
const safeList = dir => { try { return readdirSync(dir); } catch { return []; } };

/** Add the legacy Core alias inside a staged release. A native archive already supplies its fixed tree. */
export function linkCoreRuntimeIntoRelease(releaseDirectory, runtime) {
  if (runtime.kind === RUST_CORE_KIND) return;
  if (runtime.venv) symlinkSync(runtime.venv, path.join(releaseDirectory, 'core'));
  else if (runtime.kind === 'bundle') symlinkSync(runtime.root, path.join(releaseDirectory, 'core'));
  else {
    mkdirSync(path.join(releaseDirectory, 'core', 'bin'), { recursive: true });
    symlinkSync(path.resolve(runtime.bin), path.join(releaseDirectory, 'core', 'bin', 'sidevoice-core'));
  }
}

/** Step 3: `releases/<id>` made whole — this package (copied; a checkout linked), the core runtime verified and staged
 *  inside its release or linked from its immutable runtime, the core's `--self-test` and connector's `--version` passing,
 *  `release.json` written, everything flushed — and only then renamed into place. A complete one is reused. */
export async function stage(env, next, { dataDir, core = true, log = () => {}, progress = () => {}, signal,
  progressEvent = () => {}, onRuntime = () => {} }) {
  const nativeCore = embeddedRustCoreIdentity();
  if (nativeCore && !core) throw keyed('install.authenticity', { check: 'manifest' });
  const { root, releases } = releaseLayout(env);
  verifyPrivateDir(root, { create: true });
  verifyPrivateDir(releases, { create: true });
  const final = path.join(releases, next.id);
  const existing = readTrustedJson(path.join(final, 'release.json'));
  if (existing?.id === next.id) {
    if (existing.pair_id === next.pair_id
        || (!existing.pair_id && !next.core_build && (existing.runtime_kind ?? 'javascript') === (next.runtime_kind ?? 'javascript'))) return existing;
    throw keyed('install.not-selected', { detail: 'the release path already contains a different runtime pair identity' });
  }
  const temporary = `${final}.tmp-${randomBytes(4).toString('hex')}`;
  mkdirSync(temporary, { mode: 0o700 });
  progressEvent({ step: 'stage', done: null, total: null });
  try {
  if (signal?.aborted) throw keyed('install.cancelled');
  if (next.source) symlinkSync(next.source, path.join(temporary, 'dist'));
  else if (runningAsSea()) {
    mkdirSync(path.join(temporary, 'dist'), { mode: 0o700 });
    copyFileSync(process.execPath, path.join(temporary, 'dist', 'sidevoice'));
    chmodSync(path.join(temporary, 'dist', 'sidevoice'), 0o755);
  } else for (const file of BUILD_PACKAGE.files.concat('package.json')) {
    const from = path.join(packageRoot(), file);
    if (existsSync(from)) cpSync(from, path.join(temporary, file), { recursive: true });
  }
  crash('stage-copied');
  // `--no-core`: a release with no core of its own (the connector installs one the first time a conversation needs it).
  let runtime = { id: null };
  if (core) {
    progress(t('install.progress.core', { version: CORE_VERSION }));
    runtime = await installCoreRuntime({ dataDir, env, channel: next.channel, signal, releaseCoreDirectory: path.join(temporary, 'core'), progressEvent, log: line => progress('  ' + line),
      progress: line => { if (line.trim() && !/^\s*[+-] /.test(line)) progress('    uv: ' + line.trim()); } });
    onRuntime(runtime);
    linkCoreRuntimeIntoRelease(temporary, runtime);
    const stagedCore = runtime.kind === RUST_CORE_KIND ? runtime.bin
      : runtime.kind === 'bundle' ? path.join(temporary, 'core', 'python', 'bin', 'python3')
        : path.join(temporary, 'core', 'bin', 'sidevoice-core');
    await selfTest(stagedCore, env, { bundle: runtime.kind === 'bundle',
      ...(runtime.kind === RUST_CORE_KIND ? { nativeRoot: runtime.root } : {}), signal });
  }
  const reported = runningAsSea()
    ? spawnSync(path.join(temporary, 'dist', 'sidevoice'), ['--version'], { encoding: 'utf8', timeout: 30_000 })
    : spawnSync(process.execPath, [path.join(temporary, 'dist', 'cli.mjs'), '--version'], { encoding: 'utf8', timeout: 30_000 });
  if (reported.stdout.trim() !== next.connector) throw keyed('install.self-test', { detail: `the staged connector says ${reported.stdout.trim() || reported.stderr.trim() || '?'}, not ${next.connector}` });
  if (signal?.aborted) throw keyed('install.cancelled');
  const stagedRuntimeSha256 = runningAsSea()
    ? createHash('sha256').update(readFileSync(path.join(temporary, 'dist', 'sidevoice'))).digest('hex') : null;
  if (next.runtime_sha256 && stagedRuntimeSha256 !== next.runtime_sha256) {
    throw keyed('install.self-test', { detail: 'the staged Connector runtime differs from its candidate digest' });
  }
  const coreKind = runtime.kind === RUST_CORE_KIND ? runtime.kind
    : runtime.kind === 'bundle' ? 'python-bundle' : runtime.kind === 'external' ? 'external' : 'python-wheel';
  const release = { id: next.id, connector: next.connector, core: next.core, core_build: runtime.id,
    channel: next.channel, build_seq: next.build_seq, format: next.format ?? (runningAsSea() ? 'sea' : 'esm'),
    pair_id: next.pair_id, runtime_kind: next.runtime_kind ?? 'javascript',
    runtime_build_sha: next.runtime_build_sha ?? null,
    runtime_sha256: stagedRuntimeSha256 ?? next.runtime_sha256 ?? null,
    ...(runtime.id ? { core_kind: coreKind } : {}),
    ...(runtime.kind === RUST_CORE_KIND ? { core_kind: runtime.kind, core_source_sha: runtime.sourceSha,
      core_cargo_lock_sha256: runtime.cargoLockSha256, core_manifest_sha256: runtime.manifestSha256,
      core_target: runtime.target, core_archive_sha256: runtime.archiveSha256,
      core_archive_size: runtime.archiveSize, core_entrypoint: runtime.entrypoint } : {}),
    ...(next.source ? { source: next.source } : {}) };
  writePrivateFile(path.join(temporary, 'release.json'), JSON.stringify(release, null, 2) + '\n');
  syncTree(temporary);
  if (existsSync(final)) rmSync(final, { recursive: true, force: true });   // not a complete release (no release.json)
  renameSync(temporary, final);
  syncDir(releases);
  return release;
  } finally { rmSync(temporary, { recursive: true, force: true }); }
}

/** Step 5: `previous` ← the last verified release (never a selection nobody saw run — one an installer that died left
 *  selected), then `current` ← `releases/<id>` — the commit point. */
export function switchTo(env, id) {
  const verified = selection(env, 'verified');
  if (verified) { point(env, 'previous', verified.id); crash('switch-previous'); }
  point(env, 'current', id);
  crash('switch-current');
}

/** After a verification: `verified` ← `releases/<id>`. */
export function markVerified(env, id) {
  if (selection(env, 'verified')?.id !== id) point(env, 'verified', id);
}

/** Step 8 alone: `current` back to the last verified release — or, when that is the selection itself (a person going
 *  back), to `previous`. Null when there is nothing to go back to. */
export function flipBack(env) {
  const current = selection(env, 'current');
  const back = ['verified', 'previous'].map(name => selection(env, name)).find(selected => selected && selected.id !== current?.id);
  if (!back) return null;
  point(env, 'current', back.id);
  return back.release;
}

/** Step 9: every release and core runtime none of `current`, `verified` and `previous` names, deleted. */
export function prune(env, dataDir) {
  const { releases } = releaseLayout(env);
  const kept = ['current', 'verified', 'previous'].map(name => selection(env, name)).filter(Boolean);
  const keptDirs = new Set(kept.map(selected => path.basename(selected.dir)));
  const keptRuntimes = new Set(kept.map(selected => selected.release.core_build).filter(Boolean));
  const removed = [];
  for (const name of safeList(releases)) {
    if (keptDirs.has(name)) continue;
    rmSync(path.join(releases, name), { recursive: true, force: true }); removed.push(path.join(releases, name));
  }
  if (kept.length) for (const name of safeList(runtimeRoot(dataDir))) {
    if (keptRuntimes.has(name)) continue;
    rmSync(path.join(runtimeRoot(dataDir), name), { recursive: true, force: true }); removed.push(path.join(runtimeRoot(dataDir), name));
  }
  return removed;
}

/** A pre-commit candidate runtime may be complete but unreferenced. Remove only that candidate, never one a selected
 *  release can still run from. */
export function discardRuntimeIfUnselected(env, dataDir, id) {
  if (typeof id !== 'string' || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,191}$/.test(id) || id === 'external') return false;
  const selected = ['current', 'verified', 'previous'].some(name => selection(env, name)?.release.core_build === id);
  if (selected) return false;
  rmSync(path.join(runtimeRoot(dataDir), id), { recursive: true, force: true });
  return true;
}

/** `R` itself — releases, both links, anything else Sidevoice put there — taken out by `uninstall`. */
export function removeReleases(env) {
  rmSync(releaseRoot(env), { recursive: true, force: true });
}
