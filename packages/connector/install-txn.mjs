/** The install transaction (§4.3): one installation selected at a time, moved to another under one lock, and put
 *  back together after a crash whatever was done before it.
 *
 *  Everything an installation is takes part: its copy, its core runtime, the service definition, every harness
 *  registration of ours — those it re-points and those the person just consented to — and `install.json`. Under
 *  `install.lock`:
 *  0. recover: a journal left by a transaction that died is finished or undone (below);
 *  1. the selection is read again, here, under the lock, and the candidate compared with it: only a higher
 *     version, or the same version on `nightly` with a higher `build_seq`, replaces it — a release's artifacts are
 *     immutable, and the app and `npx` never downgrade, so concurrent installers converge on the higher one;
 *  2. the copy is staged (`<copy>.staging`) and the core installed into a runtime of its own (`core.mjs`: one
 *     immutable directory per build, never one something runs from), then that runtime's `--self-test` must pass;
 *  3. a call in progress is waited for, unless the person said to apply now;
 *  4. the plan — both installations, and for each registration and the service whether it existed before and what
 *     is wanted — is journaled (`install-txn.json`) before any side effect; then every artifact is made to point at
 *     the new installation (`reconcile`), `install.json` last, and read back;
 *  5. the selection must run: the node restarted on it and answering `running` with a compatible `api` and link
 *     within 60 s — a first install too. Not running → every artifact back to the previous installation (to none
 *     at all after a first install), which must run in turn; the journal goes only once the selection runs;
 *  6. the previous installation is deleted after five minutes running (`pruneInstallations`, the supervisor).
 *
 *  **Recovery by reconciliation.** The journal records no steps. Recovery selects `to` if `install.json` already
 *  names it (or the side a rollback forced), else `from`, and makes every artifact so for it from the plan — a
 *  registration that existed before is put back even if a crash left none, one added for `to` is removed for
 *  `from`, a foreign one is never touched — reads them back, and then requires the selection to run, as step 5.
 *  Errors propagate and keep the journal. The supervisor recovers at its start (and while a journal waits): if the
 *  selection is not the program it is, it hands over to that program (`connector.mjs`). */
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, readdirSync, renameSync, rmSync } from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { API_RANGE, CORE_VERSION, LINK_RANGE, installRuntime, launchedFrom, readReady, runtimePaths, runtimeRoot, takeInstallLock } from './core.mjs';
import { selfIdentity } from './proc.mjs';
import { keyed, t } from './i18n.mjs';
import { connectorClient } from './ipc.mjs';
import { dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';
import { HARNESS_REGISTRATIONS, candidate, copiesDir, registration, stageCopy } from './registrations.mjs';
import { askConnector, definitionProgram, installationPaths, installationSettings, installedService, launchEnvironment, managerKind, reload, stopNode, uninstall as removeService, writeDefinition } from './service.mjs';
import { crash, pause } from './testpoint.mjs';

const VERIFY_MS = Number(process.env.SIDEVOICE_INSTALL_VERIFY_MS || 60_000);
const CALLS_POLL_MS = 2000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

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

export const sameCommand = (a, b) => Array.isArray(a) && Array.isArray(b) && a.length === b.length && a.every((item, index) => item === b[index]);
const sameRecord = (a, b) => !!a && !!b && a.id === b.id && sameCommand(a.command, b.command);

/** What a transaction will make true, and what was true before it — so either side can be made true again. */
async function planFor(env, { from, to, consent = [], wantService = false, notes = [] }) {
  const harnesses = {};
  for (const [name, operations] of Object.entries(HARNESS_REGISTRATIONS)) {
    const before = operations.state(env).state;
    // Asked for, but its tool is not there to add an entry with: said, with the command to run, not attempted.
    const reachable = !consent.includes(name) || before !== 'absent' || operations.reachable(env);
    if (!reachable) notes.push(t(`install.${name}-unreachable`, { manual: `claude mcp add --scope user sidevoice -- ${[registration(to).command, ...registration(to).args].join(' ')}` }));
    harnesses[name] = { before, want: before === 'ours' || (consent.includes(name) && before === 'absent' && reachable) };
  }
  const existing = installedService(env);
  // With no manager, the node service is a detached supervisor: there is one if one answers.
  const detached = !existing && managerKind(env) === 'none' && !!(await askConnector('node.status', {}, { env, timeout: 1500 }))?.supervisor;
  const before = !!existing || detached;
  return { from, to, harnesses, service: { kind: existing?.kind ?? managerKind(env), before, want: wantService || before }, paths: installationPaths(env) };
}

/** For one side of a journal: the installation, what each registration must be, and whether a service is defined. */
function desired(journal, side) {
  const record = side === 'to' ? journal.to : journal.from;
  const harnesses = {};
  for (const [name, plan] of Object.entries(journal.harnesses)) {
    harnesses[name] = !record ? 'remove'
      : side === 'to' ? (plan.want ? 'set' : 'keep')
      : plan.before === 'ours' ? 'set' : 'remove';
  }
  return { record, harnesses, service: !!record && (side === 'to' ? journal.service.want : journal.service.before) };
}

/** Make every artifact so for one side, then read each back. Every step is "make it so": done again, it changes
 *  nothing. `crashing` arms the test crash points between side effects. */
export async function reconcile(env, journal, side, { crashing = false, notes = [] } = {}) {
  env = { ...env, ...journal.paths };   // where the transaction's installer put things, whoever reconciles
  const files = nodeFiles(dataDirOf(env));
  const point = name => { if (crashing) crash(`txn-${name}`); };
  const { record, harnesses, service } = desired(journal, side);
  // Toward nothing, or toward an installation without a service: whatever runs now — a detached supervisor, a
  // connector, their core — is stopped, and seen gone (each by its verified identity), before anything is cleared.
  if (!record || (!service && journal.service.kind === 'none')) {
    // A defined service goes through its manager first (stopping it stops its supervisor and core, and nothing
    // restarts them); what then still runs is stopped by its verified identity.
    if (!service && installedService(env)) await removeService(env, { keepStopped: false });
    const status = await askConnector('node.status', {}, { env, timeout: 1500 });
    if (!record || status?.supervisor) { const down = await stopNode(env); if (down.left.length) throw keyed('service.unload-failed', { detail: `pid ${down.left.join(', ')} still running` }); }
  }
  if (record?.copy && !existsSync(record.copy)) {
    if (!existsSync(record.copy + '.staging')) throw keyed('install.copy-missing', { path: record.copy });
    renameSync(record.copy + '.staging', record.copy);
  }
  point('copy');
  if (record?.runtime_id && record.runtime_id !== 'external' && !existsSync(runtimePaths(dataDirOf(env), record.runtime_id).marker)) throw keyed('install.copy-missing', { path: runtimePaths(dataDirOf(env), record.runtime_id).home });
  // With no service manager the node service is a detached supervisor: nothing to define, only to run.
  if (service && journal.service.kind !== 'none') {
    const kind = installedService(env)?.kind ?? journal.service.kind;
    writeDefinition(kind, record, env);
    if (kind === 'systemd') {
      for (const args of [['--user', 'daemon-reload'], ['--user', 'enable', 'sidevoice-node.service']]) execFileSync(env.SIDEVOICE_SYSTEMCTL || 'systemctl', args, { timeout: 10_000, env, stdio: 'ignore' });
    }
  } else if (!service && installedService(env)) {
    await removeService(env, { keepStopped: false });
  }
  point('definition');
  for (const [name, action] of Object.entries(harnesses)) {
    const operations = HARNESS_REGISTRATIONS[name];
    await pause(`txn-register-${name}`);
    const outcome = action === 'set' ? operations.set(env, record) : action === 'remove' ? operations.remove(env) : null;
    if (outcome === 'repointed' && crashing) notes.push(t('install.repointed', { harness: t(`harness.${name}`) }));
    if (outcome === 'foreign' || outcome === 'invalid') notes.push(t(`install.${name}-${outcome === 'foreign' && name === 'claude' ? 'foreign-note' : outcome}`));
    point(`register:${name}`);
  }
  if (record) writePrivate(files.install, record); else rmSync(files.install, { force: true });
  point('install.json');
  for (const name of safeList(copiesDir(env))) if (name.endsWith('.staging')) rmSync(path.join(copiesDir(env), name), { recursive: true, force: true });
  verifyArtifacts(env, journal, side);
  return notes;
}

/** Read every artifact back: what reconcile wrote is what is there. */
function verifyArtifacts(env, journal, side) {
  const { record, harnesses, service } = desired(journal, side);
  const wrong = what => { throw keyed('install.reconcile-failed', { what }); };
  const installed = readJson(nodeFiles(dataDirOf(env)).install);
  if (record ? !sameRecord(installed, record) : installed) wrong('install.json');
  if (record?.copy && !existsSync(record.copy)) wrong(record.copy);
  const definition = installedService(env);
  if (journal.service.kind === 'none') return verifyRegistrations(env, record, harnesses);
  if (service && (!definition || !sameCommand(definitionProgram(definition), [...record.command, 'connector', '--supervise']))) wrong('the service definition');
  if (!service && definition) wrong('the service definition');
  verifyRegistrations(env, record, harnesses);
}
function verifyRegistrations(env, record, harnesses) {
  const wrong = what => { throw keyed('install.reconcile-failed', { what }); };
  for (const [name, action] of Object.entries(harnesses)) {
    const state = HARNESS_REGISTRATIONS[name].state(env);
    if (action === 'set' && state.state === 'ours') {
      const { command, args } = registration(record);
      const line = state.line ?? [state.entry.command, ...state.entry.args].join(' ');
      if (line !== [command, ...args].join(' ')) wrong(`${name}'s registration`);
    }
    if (action === 'set' && state.state === 'absent') wrong(`${name}'s registration`);
    if (action === 'remove' && state.state === 'ours') wrong(`${name}'s registration`);
  }
}

/** The one verification gate (installer and supervisor alike): the connector answering runs this installation's
 *  command and is running; its core was launched from this installation's runtime; and it speaks a compatible `api`
 *  and link. `{ok}`, or `{ok: false, why}` — `why` a cause key. */
export function selectionRuns(status, record, dataDir) {
  if (status?.state !== 'running') return { ok: false, why: status?.state === 'failed' ? (status.failure?.key || 'install.verify') : 'ready.timeout' };
  if (!sameCommand(status.command, record.command)) return { ok: false, why: 'install.not-selected' };
  if (record.core_bin && record.runtime_id !== 'external' && launchedFrom(dataDir, status.core?.launch_id) !== record.core_bin) return { ok: false, why: 'install.not-selected' };
  const api = status.core?.api, link = readReady(dataDir)?.protocol;
  const within = (value, [low, high]) => Number.isInteger(value) && value >= low && value <= high;
  if (!within(api, API_RANGE) || !within(link, LINK_RANGE)) return { ok: false, why: 'install.incompatible' };
  return { ok: true };
}
export const VERIFY_DEADLINE_MS = VERIFY_MS;

/** Make the node run this installation and judge it with `selectionRuns`, within `VERIFY_MS`. With a service (or a
 *  detached supervisor) the service is reloaded from its definition — the supervisor is replaced. With none, the
 *  connector answering is replaced by this installation's (`connector --replace` takes it over by handover), or one is
 *  started through the launcher with this installation's command; then the core is ensured. */
export async function runsCompatibly(env, record, { service, log = () => {}, permit = null }) {
  const dataDir = dataDirOf(env);
  let last = null;
  // The node this transaction starts to verify is the only one that may serve while it holds the install lock: a
  // supervisor booting meanwhile without this permit waits for the transaction (`connector.mjs`). A file, not the
  // environment: a service manager starts the node with its definition's.
  if (permit) writePrivate(nodeFiles(dataDir).permit, { journal: permit, pid: process.pid, start: selfIdentity().start ?? null });
  try {
    if (service) {
      // Another installation is starting: what the one before spent of the budget is not this one's.
      writePrivate(nodeFiles(dataDir).restart, { at: new Date().toISOString(), why: `selected ${record.id}` });
      await reload(env);
    } else {
      const answering = await askConnector('node.status', {}, { env, timeout: 1500 });
      if (answering?.state && !sameCommand(answering.command, record.command)) {
        log(`the connector running (${answering.command?.join(' ')}) is not the selected installation's: replacing it`);
        const child = spawn(record.command[0], [...record.command.slice(1), 'connector', '--replace'], { detached: true, stdio: 'ignore', env: launchEnvironment(record, env) });
        child.on('error', () => {}); child.unref();
        const until = Date.now() + VERIFY_MS;
        while (Date.now() < until && !sameCommand((await askConnector('node.status', {}, { env, timeout: 1500 }))?.command, record.command)) await wait(100);
      }
      const client = connectorClient(launchEnvironment(record, env), { self: record.command });
      try { last = await client.rpc('node.ensure', {}); } finally { client.end(); }
      if (selectionRuns(last, record, dataDir).ok) return { ok: true };
    }
    const deadline = Date.now() + VERIFY_MS;
    while (Date.now() < deadline) {
      last = await askConnector('node.status', {}, { env, timeout: 1500 });
      if (selectionRuns(last, record, dataDir).ok) return { ok: true };
      if (last?.state === 'failed') break;
      await wait(250);
    }
  } catch (error) { return { ok: false, failure: { key: error.key || 'install.verify', message: error.message } }; }
  finally { if (permit) rmSync(nodeFiles(dataDir).permit, { force: true }); }
  const why = selectionRuns(last, record, dataDir).why;
  log(`the installation ${record.id} did not run (${why})`);
  return { ok: false, failure: last?.state === 'failed' && last.failure ? last.failure : { key: why } };
}

/** Which side recovery selects. */
function selectedSide(env, journal) {
  if (journal.selection) return journal.selection;
  return sameRecord(readJson(nodeFiles(dataDirOf(env)).install), journal.to) ? 'to' : 'from';
}

/** Recovery (holding the lock). `mode`: `installer` — reconcile, then the selection must run (rolling back as step
 *  5 does), then the journal goes; `supervisor` — reconcile and say which installation is selected, the journal
 *  staying until the supervisor sees it run (`settle`); `artifacts` — no core to run (`--no-core`). */
export async function recover(env, { mode = 'installer', log = () => {} } = {}) {
  const files = nodeFiles(dataDirOf(env));
  const journal = readJson(files.journal);
  if (!journal) return null;
  env = { ...env, ...journal.paths };
  let side = selectedSide(env, journal);
  // A supervisor cannot reconcile toward an installation without itself — nothing at all, or one with no service:
  // stopping what runs would stop the reconciler. That recovery is finished outside it (`service recover`).
  if (mode === 'supervisor' && (!(side === 'to' ? journal.to : journal.from) || !desired(journal, side).service)) {
    return { journal, side, record: side === 'to' ? journal.to : journal.from, outside: true };
  }
  log(`an install transaction was interrupted (${journal.from?.id ?? 'nothing'} → ${journal.to?.id}); reconciling to ${(side === 'to' ? journal.to : journal.from)?.id ?? 'nothing'}`);
  await reconcile(env, journal, side);
  const record = side === 'to' ? journal.to : journal.from;
  if (mode === 'supervisor') return { journal, side, record };
  if (!record) await nothingRuns(env);
  if (mode === 'artifacts' || !record) { rmSync(files.journal, { force: true }); return { side, record }; }
  const permit = journal.id ?? journal.at;
  let ran = await runsCompatibly(env, record, { service: desired(journal, side).service, log, permit });
  if (!ran.ok && side === 'to') {
    side = 'from';
    writePrivate(files.journal, { ...journal, selection: 'from', failure: ran.failure });
    await reconcile(env, journal, 'from');
    ran = journal.from ? await runsCompatibly(env, journal.from, { service: desired(journal, 'from').service, log, permit }) : (await nothingRuns(env), { ok: true });
  }
  if (ran.ok) rmSync(files.journal, { force: true });
  return { side, record: side === 'to' ? journal.to : journal.from, ran };
}

/** Nothing of this node runs — no connector answering, no connector or core alive by its records — seen under the
 *  install lock, after a reconcile to nothing and before the journal goes: what started meanwhile is stopped now, and
 *  what cannot be stopped keeps the journal. */
async function nothingRuns(env) {
  const down = await stopNode(env);
  const answering = await askConnector('node.status', {}, { env, timeout: 1500 });
  if (down.left.length || answering?.state) throw keyed('service.unload-failed', { detail: down.left.length ? `pid ${down.left.join(', ')} still running` : 'a connector still answers' });
}

/** The supervisor's side of a recovered journal: the selection runs — the journal goes; it cannot — roll back to
 *  `from` (the caller then hands over to it). Called holding the lock. Returns `done`, `rollback` or `keep`. */
export async function settle(env, { running, failed }) {
  const files = nodeFiles(dataDirOf(env));
  const journal = readJson(files.journal);
  if (!journal) return { outcome: 'done' };
  const side = selectedSide(env, journal);
  if (running) { rmSync(files.journal, { force: true }); return { outcome: 'done' }; }
  if (failed && side === 'to') {
    writePrivate(files.journal, { ...journal, selection: 'from' });
    // Back to nothing, or to an installation with no service: not this supervisor's to reconcile (see `recover`).
    if (!journal.from || !desired(journal, 'from').service) return { outcome: 'outside', record: journal.from };
    await reconcile(env, journal, 'from');
    return { outcome: 'rollback', record: journal.from };
  }
  return { outcome: 'keep' };
}

/** The number of calls open on the running node, or 0 when none answers. */
async function openCalls(env) {
  const status = await askConnector('node.status', {}, { env, timeout: 1500 });
  return status?.calls ?? 0;
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

/** The whole transaction. `consent`: harnesses the person asked to connect now. `service`: the node service wanted.
 *  `keep`: the selected installation stays (only consent or service change — `service install`). `core: false`
 *  leaves the core for later (`--no-core`): nothing to install or run, so artifacts are all that is checked.
 *  Returns `{action: 'install'|'upgrade'|'noop'|'rollback', record, from, failure?, notes}`. */
export async function transact(env, { core = true, applyNow = false, by = env.SIDEVOICE_INSTALLED_BY || 'cli', service = false, consent = [], keep = false, progress = () => {}, log = () => {} } = {}) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const release = await takeInstallLock(dataDir, log);
  try {
    await recover(env, { mode: core ? 'installer' : 'artifacts', log });
    const current = readJson(files.install);   // the selection, read again under the lock
    if (keep && !current) throw keyed('service.no-installation');
    const fresh = { ...candidate(env), by, at: new Date().toISOString(), settings: installationSettings(env), paths: installationPaths(env) };
    const action = keep ? 'noop' : decide(current, fresh);
    let next = action === 'noop' ? current : fresh;
    if (action !== 'noop') {
      stageCopy(next);
      if (core) {
        progress(t('install.progress.core', { version: CORE_VERSION }));
        const runtime = await installRuntime({ dataDir, env, log: line => progress('  ' + line),
          progress: line => { if (line.trim() && !/^\s*[+-] /.test(line)) progress('    uv: ' + line.trim()); } });
        next = { ...next, runtime_id: runtime.id, core_bin: runtime.bin };
        selfTest(runtime.bin, env);
      } else if (current?.runtime_id) next = { ...next, runtime_id: current.runtime_id, core_bin: current.core_bin };
    }
    const planNotes = [];
    const plan = await planFor(env, { from: current, to: next, consent, wantService: service, notes: planNotes });
    // The record says which service it was installed with (informational: the definition on disk is the truth).
    next = plan.to = { ...next, service: plan.service.want ? plan.service.kind : 'none' };
    const newConsent = Object.values(plan.harnesses).some(harness => harness.want && harness.before === 'absent');
    const newService = plan.service.want && !plan.service.before;
    if (action === 'noop' && !newConsent && !newService) return { action, record: current, from: current, notes: planNotes };
    if (current && !applyNow && action !== 'noop') {
      let said = false;
      for (let calls = await openCalls(env); calls > 0; calls = await openCalls(env)) {
        if (!said) { progress(t('install.progress.calls', { calls })); said = true; }
        await wait(CALLS_POLL_MS);
      }
    }
    const journal = { ...plan, id: randomUUID(), at: new Date().toISOString() };
    writePrivate(files.journal, journal);
    crash('txn-journal');
    const notes = await reconcile(env, journal, 'to', { crashing: true, notes: planNotes });
    const needsRun = core && (action !== 'noop' || newService);
    const ran = needsRun ? await runsCompatibly(env, next, { service: plan.service.want, log, permit: journal.id }) : { ok: true };
    if (ran.ok) { rmSync(files.journal, { force: true }); return { action, record: next, from: current, notes }; }
    // Back to what was there — nothing at all after a first install — and that must run in turn.
    writePrivate(files.journal, { ...journal, selection: 'from', failure: ran.failure });
    await reconcile(env, journal, 'from');
    const back = current && core ? await runsCompatibly(env, current, { service: plan.service.before, log, permit: journal.id }) : { ok: true };
    if (!current) await nothingRuns(env);
    if (back.ok) rmSync(files.journal, { force: true });
    if (!current && next.copy) rmSync(next.copy, { recursive: true, force: true });
    return { action: 'rollback', record: current, from: current, failure: ran.failure, back: back.ok, backFailure: back.failure ?? null, notes };
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
      if (name !== record.runtime_id) { rmSync(path.join(runtimeRoot(dataDir), name), { recursive: true, force: true }); removed.push(path.join(runtimeRoot(dataDir), name)); }
    }
    if (removed.length) log(`removed the previous installation(s): ${removed.join(', ')}`);
    return removed;
  } finally { release(); }
}
const safeList = dir => { try { return readdirSync(dir); } catch { return []; } };
