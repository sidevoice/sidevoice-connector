/** What is installed, and which of it runs (§2.4, SEAMS §1): immutable release directories and two links.
 *
 *    R/releases/<id>/   immutable once renamed into place: `dist/` (this package), `core` (→ the core runtime it was
 *                       verified with), `release.json` {id, connector, core, core_build, channel, build_seq}
 *    R/current  → releases/<id>   the selection
 *    R/previous → releases/<id>   the rollback target
 *
 *  `R` is `$XDG_DATA_HOME/sidevoice`. Everything that runs Sidevoice — both job definitions, every harness
 *  registration, `install.json`'s `command` — names a path through `R/current`, so it is written once and never
 *  re-pointed: switching is replacing one link, `symlink(tmp)` + `rename(tmp, link)` (never `ln -sfn`, which unlinks
 *  first on macOS), then `fsync(R)`. A crash anywhere leaves `current` naming a complete release, old or new. A
 *  process started through `current` keeps running its own release after a switch (Node runs a module by its real
 *  path; the core's console script names its real venv), and the next start runs the new one.
 *
 *  `install` holds the install lock throughout (`lockfile.mjs`). There is no journal: whatever a dead installer left
 *  is recognised by name and deleted (`*.tmp*`), and re-running `install` verifies the selection and flips back if it
 *  does not run — the recovery. */
import path from 'node:path';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import { closeSync, cpSync, existsSync, fsyncSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, realpathSync, renameSync, rmSync, symlinkSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { CORE_VERSION, installRuntime, runtimeRoot, selfTest } from './core.mjs';
import { keyed, t } from './i18n.mjs';
import { recordedInstallation } from './node-files.mjs';
import { readTrustedJson, verifyPrivateDir, writePrivateFile } from './secure-fs.mjs';
import { crash } from './testpoint.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
/** Where the package's own root is: next to these modules in the checkout, and one level up once they have been
 *  bundled into `dist/`. */
const packageRoot = () => (existsSync(path.join(here, '..', 'package.json')) ? path.dirname(here) : here);
let manifestCache = null;
const manifest = () => (manifestCache ??= JSON.parse(readFileSync(path.join(here, 'package.json'), 'utf8')));

/** `R`: where the installation recorded it was made (`install.json`'s `releases`), else `$XDG_DATA_HOME/sidevoice`. */
export function releaseRoot(env = process.env) {
  const recorded = recordedInstallation(env)?.releases;
  if (typeof recorded === 'string' && path.isAbsolute(recorded) && path.basename(recorded) === 'sidevoice') return recorded;
  return path.join(env.XDG_DATA_HOME || path.join(env.HOME || os.homedir(), '.local', 'share'), 'sidevoice');
}
export function releaseLayout(env = process.env) {
  const root = releaseRoot(env);
  return { root, releases: path.join(root, 'releases'), current: path.join(root, 'current'), previous: path.join(root, 'previous') };
}
/** The program each job and harness runs, through `current` (R1: node + `cli.mjs`; R4 swaps it for the executable). */
export function stableCommand(env = process.env) {
  return [process.execPath, path.join(releaseLayout(env).current, 'dist', 'cli.mjs')];
}
export function coreProgram(env = process.env) {
  return path.join(releaseLayout(env).current, 'core', 'bin', 'sidevoice-core');
}

/** A checkout installs itself: its release links to it instead of copying it. */
export function fromSource(env = process.env) {
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '0') return false;
  return env.SIDEVOICE_INSTALL_FROM_SOURCE === '1' || existsSync(path.join(packageRoot(), '..', '..', '.git'));
}

/** The release this package would make: its version, channel and build (`sidevoice` in the manifest, stamped by CI;
 *  a checkout's is `source`). A nightly carries the version of the last release, so its id names its build. */
export function candidate(env = process.env) {
  const { version, sidevoice = {} } = manifest();
  const source = fromSource(env);
  const channel = source ? 'source' : sidevoice.channel || 'release';
  const build_seq = Number(sidevoice.build_seq) || 0;
  const id = channel === 'nightly' ? `${version}-nightly.${build_seq}` : source ? `${version}-source` : version;
  return { id, connector: version, core: CORE_VERSION, channel, build_seq, source: source ? packageRoot() : null };
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
  if (next.channel === 'source' || current.channel === 'source') return current.id === next.id && current.source === next.source ? 'noop' : 'upgrade';
  const order = compareVersions(next.connector, current.connector);
  if (order > 0) return 'upgrade';
  if (order < 0) return 'noop';
  return next.channel === 'nightly' && next.build_seq > (Number(current.build_seq) || 0) ? 'upgrade' : 'noop';
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

/** Step 3: `releases/<id>` made whole — this package (copied; a checkout linked), the core runtime installed or reused
 *  and linked as `core`, the core's `--self-test` and the connector's `--version` passing on what was staged,
 *  `release.json` written, everything flushed — and only then renamed into place. A complete one is reused. */
export async function stage(env, next, { dataDir, core = true, log = () => {}, progress = () => {} }) {
  const { root, releases } = releaseLayout(env);
  verifyPrivateDir(root, { create: true });
  verifyPrivateDir(releases, { create: true });
  const final = path.join(releases, next.id);
  const existing = readTrustedJson(path.join(final, 'release.json'));
  if (existing?.id === next.id) return existing;
  const temporary = `${final}.tmp-${randomBytes(4).toString('hex')}`;
  mkdirSync(temporary, { mode: 0o700 });
  if (next.source) symlinkSync(next.source, path.join(temporary, 'dist'));
  else for (const file of manifest().files.concat('package.json')) {
    const from = path.join(packageRoot(), file);
    if (existsSync(from)) cpSync(from, path.join(temporary, file), { recursive: true });
  }
  crash('stage-copied');
  // `--no-core`: a release with no core of its own (the connector installs one the first time a conversation needs it).
  let runtime = { id: null };
  if (core) {
    progress(t('install.progress.core', { version: CORE_VERSION }));
    runtime = await installRuntime({ dataDir, env, log: line => progress('  ' + line),
      progress: line => { if (line.trim() && !/^\s*[+-] /.test(line)) progress('    uv: ' + line.trim()); } });
    if (runtime.venv) symlinkSync(runtime.venv, path.join(temporary, 'core'));
    else { mkdirSync(path.join(temporary, 'core', 'bin'), { recursive: true }); symlinkSync(path.resolve(runtime.bin), path.join(temporary, 'core', 'bin', 'sidevoice-core')); }
    selfTest(path.join(temporary, 'core', 'bin', 'sidevoice-core'), env);
  }
  const reported = spawnSync(process.execPath, [path.join(temporary, 'dist', 'cli.mjs'), '--version'], { encoding: 'utf8', timeout: 30_000 });
  if (reported.stdout.trim() !== next.connector) throw keyed('install.self-test', { detail: `the staged connector says ${reported.stdout.trim() || reported.stderr.trim() || '?'}, not ${next.connector}` });
  const release = { id: next.id, connector: next.connector, core: next.core, core_build: runtime.id, channel: next.channel, build_seq: next.build_seq, ...(next.source ? { source: next.source } : {}) };
  writePrivateFile(path.join(temporary, 'release.json'), JSON.stringify(release, null, 2) + '\n');
  syncTree(temporary);
  if (existsSync(final)) rmSync(final, { recursive: true, force: true });   // not a complete release (no release.json)
  renameSync(temporary, final);
  syncDir(releases);
  return release;
}

/** Step 5: `previous` ← `current`, then `current` ← `releases/<id>` — the commit point. */
export function switchTo(env, id) {
  const now = selection(env, 'current');
  if (now) { point(env, 'previous', now.id); crash('switch-previous'); }
  point(env, 'current', id);
  crash('switch-current');
}

/** Step 8 alone: `current` back to what `previous` names. Null when there is nothing to go back to. */
export function flipBack(env) {
  const previous = selection(env, 'previous'), current = selection(env, 'current');
  if (!previous || previous.id === current?.id) return null;
  point(env, 'current', previous.id);
  return previous.release;
}

/** Step 9: every release and core runtime neither `current` nor `previous` names, deleted. */
export function prune(env, dataDir) {
  const { releases } = releaseLayout(env);
  const kept = ['current', 'previous'].map(name => selection(env, name)).filter(Boolean);
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

/** `R` itself — releases, both links, anything else Sidevoice put there — taken out by `uninstall`. */
export function removeReleases(env) {
  rmSync(releaseRoot(env), { recursive: true, force: true });
}
