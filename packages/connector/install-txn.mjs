/** The install transaction (§4.3): one installation selected at a time, moved to another under one lock, and
 *  put back together after a crash whatever was done before it.
 *
 *  Under `install.lock` (the lock the core's install already took, `core.mjs`):
 *  0. recover: a journal (`install-txn.json`) left by a transaction that died is finished or undone;
 *  1. the candidate is compared with `install.json`: only a higher version, or the same version on `nightly`
 *     with a higher `build_seq`, replaces it — a release's artifacts are immutable, so two builds of the same
 *     version never replace each other in a loop; a lower one is never installed (the app never downgrades,
 *     and neither does `npx`: concurrent installers converge on the higher one);
 *  2. the copy is staged (`<copy>.staging`) and the core installed beside the one running (`core.mjs` keeps
 *     one runtime per version), then `sidevoice-core --self-test` must pass;
 *  3. a call in progress is waited for, unless the person said to apply now;
 *  4. commit: the journal `{from, to}` is written before any side effect, then every artifact is made to point
 *     at `to` (`reconcile`), `install.json` last — the commit point;
 *  5. the node is restarted; not `running` with a compatible `api` and link within 60 s → every artifact back to
 *     `from`, restarted, reported as `rollback`;
 *  6. the previous installation is deleted only after five minutes running (`pruneInstallations`, the supervisor).
 *
 *  **Recovery by reconciliation.** The journal records no steps. Recovery selects `to` if `install.json` already
 *  names it, else `from`, and makes every artifact so for the selected one: its copy present, the service
 *  definition written from its command, every registration of ours pointed at it (foreign ones untouched),
 *  `install.json` naming it, no `*.staging` left. Each of those is "make it so", so a step that half ran, or ran
 *  without being recorded, is simply done again. */
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, renameSync, rmSync } from 'node:fs';
import path from 'node:path';
import { API_RANGE, CORE_VERSION, LINK_RANGE, ensureInstalled, ensureRunning, examineRunning, readReady, runtimeRoot, takeInstallLock } from './core.mjs';
import { keyed } from './i18n.mjs';
import { dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';
import { candidate, copiesDir, repointClaude, repointCursor, stageCopy } from './registrations.mjs';
import { askConnector, installedService, reload, writeDefinition } from './service.mjs';

const VERIFY_MS = Number(process.env.SIDEVOICE_INSTALL_VERIFY_MS || 60_000);
const CALLS_POLL_MS = 2000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** A test kills the installer right after one side effect, to recover from there (`SIDEVOICE_TXN_CRASH_AFTER`). */
function sideEffect(env, name) {
  if (env.SIDEVOICE_TXN_CRASH_AFTER === name) process.kill(process.pid, 'SIGKILL');
}

const parts = version => String(version || '0').split(/[.+-]/).slice(0, 3).map(part => Number(part) || 0);
export function compareVersions(a, b) {
  const [x, y] = [parts(a), parts(b)];
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i] ? 1 : -1;
  return 0;
}

/** Whether `next` replaces `current`: `install`, `upgrade`, or `noop` (same or lower, or the same version and
 *  channel with no higher build). A checkout is its own channel: installing from one points everything at it. */
export function decide(current, next) {
  if (!current) return 'install';
  if (next.channel === 'source' || current.channel === 'source') return current.command?.join('\0') === next.command.join('\0') ? 'noop' : 'upgrade';
  const order = compareVersions(next.connector, current.connector);
  if (order > 0) return 'upgrade';
  if (order < 0) return 'noop';
  return next.channel === 'nightly' && next.build_seq > (Number(current.build_seq) || 0) ? 'upgrade' : 'noop';
}

/** Make every artifact point at `record`; `record` null makes them point at nothing (a first install undone). */
export function reconcile(env, record, { crash = false, done = [] } = {}) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const step = name => { if (crash) sideEffect(env, name); };
  if (record?.copy && !existsSync(record.copy)) {
    if (!existsSync(record.copy + '.staging')) throw keyed('install.copy-missing', { path: record.copy });
    renameSync(record.copy + '.staging', record.copy);
  }
  step('copy');
  const service = installedService(env);
  if (service && record) {
    writeDefinition(service.kind, record, env);
    if (service.kind === 'systemd') { try { execFileSync(env.SIDEVOICE_SYSTEMCTL || 'systemctl', ['--user', 'daemon-reload'], { timeout: 10_000, env, stdio: 'ignore' }); } catch {} }
  }
  step('definition');
  if (record) {
    try { const was = repointClaude(env, record); if (was) done.push(`Re-pointed Claude Code's MCP server to this version (was: ${was}).`); } catch {}
    step('register:claude');
    try { repointCursor(env, record, done); } catch {}
    step('register:cursor');
    writePrivate(files.install, record);
  } else rmSync(files.install, { force: true });
  step('install.json');
  removeStaging(env);
}

function removeStaging(env) {
  try { for (const name of readdirSync(copiesDir(env))) if (name.endsWith('.staging')) rmSync(path.join(copiesDir(env), name), { recursive: true, force: true }); } catch {}
}

/** Recovery: finish or undo the transaction a journal says was under way. `verify` continues a recovered `to` at
 *  step 5 (the installer's own recovery); the supervisor, which is itself what step 5 restarts, only reconciles. */
export async function recover(env, { verify = false, log = () => {} } = {}) {
  const files = nodeFiles(dataDirOf(env));
  const journal = readJson(files.journal);
  if (!journal) return null;
  const current = readJson(files.install);
  const selectedTo = !!journal.to && current?.id === journal.to.id && current?.command?.join('\0') === journal.to.command.join('\0');
  const chosen = selectedTo ? journal.to : journal.from;
  log(`an install transaction was interrupted (${journal.from?.id ?? 'nothing'} → ${journal.to?.id}); reconciling to ${chosen?.id ?? 'nothing'}`);
  reconcile(env, chosen);
  let outcome = { recovered: chosen?.id ?? null };
  if (selectedTo && verify) outcome = { ...outcome, ...(await verifyOrRollBack(env, journal.from, journal.to, { log })) };
  rmSync(files.journal, { force: true });
  return outcome;
}

/** Whether a core is running and speaks this connector's `api` and link. */
function compatible(status, dataDir) {
  const ready = readReady(dataDir);
  const api = status?.core?.api;
  return status?.state === 'running' && Number.isInteger(api) && api >= API_RANGE[0] && api <= API_RANGE[1]
    && Number.isInteger(ready?.protocol) && ready.protocol >= LINK_RANGE[0] && ready.protocol <= LINK_RANGE[1];
}

/** Step 5: the node restarted on the new installation and seen running compatibly within 60 s, else every
 *  artifact back to `from` and the node restarted on it. */
async function verifyOrRollBack(env, from, to, { log = () => {}, skip = false } = {}) {
  if (skip) return { action: 'committed' };
  const dataDir = dataDirOf(env);
  let failure = null;
  try {
    await restartNode(env);
    const deadline = Date.now() + VERIFY_MS;
    let status = null;
    while (Date.now() < deadline) {
      status = await nodeState(env);
      if (compatible(status, dataDir)) return { action: 'committed' };
      if (status?.state === 'failed') break;
      await wait(500);
    }
    failure = status?.failure || { key: status?.state === 'running' ? 'install.incompatible' : 'ready.timeout' };
  } catch (error) { failure = { key: error.key || 'install.verify', message: error.message }; }
  log(`the new installation did not come up (${failure.key}); rolling back to ${from?.id ?? 'nothing'}`);
  reconcile(env, from);
  if (from) { try { await restartNode(env); } catch {} }
  return { action: 'rollback', failure };
}

/** Restart the node so it runs the selected installation: through the service manager when there is a
 *  service (the supervisor itself is replaced, not only its core), else a core started without a service. */
async function restartNode(env) {
  const running = await askConnector('node.status', {}, { env, timeout: 1500 });
  if (installedService(env) || running?.supervisor) return reload(env);
  // No service: the next plain connector links with whatever core runs; this one is started now, on the new
  // runtime (`ensureRunning` asks a core of another version to leave), and is what step 5 judges.
  const ready = await ensureRunning({ dataDir: dataDirOf(env), env });
  return { ok: true, state: 'running', core: ready };
}

/** The node as the connector sees it, or — no connector, no service — the core itself, by its health. */
async function nodeState(env) {
  const status = await askConnector('node.status', {}, { env, timeout: 1500 });
  if (status?.state) return status;
  const found = await examineRunning(dataDirOf(env));
  return found.adopt ? { state: 'running', core: { pid: found.adopt.pid, api: found.adopt.api, version: found.adopt.version }, calls: found.adopt.calls ?? 0 } : null;
}

/** The number of calls open on the running core, or 0 when none answers. */
async function openCalls(env) {
  return (await nodeState(env))?.calls ?? 0;
}

/** `sidevoice-core --self-test`: imports everything serving needs, binds nothing, writes nothing. */
export function selfTest(bin, env) {
  let output = '';
  try { output = execFileSync(bin, ['--self-test'], { encoding: 'utf8', timeout: 120_000, env, stdio: ['ignore', 'pipe', 'pipe'] }); }
  catch (error) { output = String(error.stdout || ''); if (!output.trim()) throw keyed('install.self-test', { detail: String(error.stderr || error.message).trim().split('\n').at(-1) }); }
  let report = null; try { report = JSON.parse(output.trim().split('\n').at(-1)); } catch {}
  if (!report?.ok) throw keyed(report?.key || 'install.self-test', { detail: report?.message || output.trim().slice(0, 200) });
  return report;
}

/** The whole transaction. `core: false` leaves the core for later (nothing to self-test or restart). Returns
 *  `{action: 'install'|'upgrade'|'noop'|'rollback', record, from, failure?}`. */
export async function transact(env, { core = true, applyNow = false, by = env.SIDEVOICE_INSTALLED_BY || 'cli', service = null, progress = () => {}, log = () => {} } = {}) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const release = await takeInstallLock(dataDir, log);
  try {
    await recover(env, { verify: core, log });
    const current = readJson(files.install);
    const next = { ...candidate(env), by, at: new Date().toISOString(), service: service ?? current?.service ?? 'none' };
    const action = decide(current, next);
    if (action === 'noop') return { action, record: current, from: current };
    // 2. staged, and the core installed beside the one running, then asked whether it imports.
    stageCopy(next);
    if (core) {
      progress(`Installing this machine's Sidevoice core ${CORE_VERSION} (the first time: Python and a few hundred MB, some minutes)…`);
      const bin = await ensureInstalled({ dataDir, env, log: line => progress('  ' + line),
        progress: line => { if (line.trim() && !/^\s*[+-] /.test(line)) progress('    uv: ' + line.trim()); } });
      try { selfTest(bin, env); } catch (error) { removeStaging(env); throw error; }
    }
    // 3. not in the middle of someone's call.
    if (current && !applyNow) {
      let said = false;
      for (let calls = await openCalls(env); calls > 0; calls = await openCalls(env)) {
        if (!said) { progress(`Waiting for ${calls} call(s) on this machine to end before applying the update (--apply-now applies it at once)…`); said = true; }
        await wait(CALLS_POLL_MS);
      }
    }
    // 4. the journal before any side effect, then every artifact, install.json last.
    writePrivate(files.journal, { from: current, to: next, at: new Date().toISOString() });
    sideEffect(env, 'journal');
    const done = [];
    reconcile(env, next, { crash: true, done });
    // 5. running on it, or back.
    const outcome = await verifyOrRollBack(env, current, next, { log, skip: !core || !current });
    rmSync(files.journal, { force: true });
    return { ...outcome, action: outcome.action === 'rollback' ? 'rollback' : action, record: outcome.action === 'rollback' ? current : next, from: current, done: outcome.action === 'rollback' ? [] : done };
  } finally { release(); }
}

/** Step 6: once the selected installation has run for five minutes, the others go — copies and core runtimes —
 *  unless a transaction is under way (its journal keeps both). Asked by the supervisor; never waits for the lock. */
export async function pruneInstallations(env, { log = () => {} } = {}) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const release = await takeInstallLock(dataDir, log, { wait: false });
  if (!release) return [];
  const removed = [];
  try {
    const record = readJson(files.install);
    if (!record || existsSync(files.journal)) return [];
    if (record.copy) {
      for (const name of safeList(copiesDir(env))) {
        const full = path.join(copiesDir(env), name);
        if (full !== record.copy) { rmSync(full, { recursive: true, force: true }); removed.push(full); }
      }
    }
    for (const name of safeList(runtimeRoot(dataDir))) {
      if (name !== record.core) { rmSync(path.join(runtimeRoot(dataDir), name), { recursive: true, force: true }); removed.push(path.join(runtimeRoot(dataDir), name)); }
    }
    if (removed.length) log(`removed the previous installation(s): ${removed.join(', ')}`);
    return removed;
  } finally { release(); }
}
const safeList = dir => { try { return readdirSync(dir); } catch { return []; } };
