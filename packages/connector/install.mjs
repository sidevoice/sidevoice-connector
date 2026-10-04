/** `sidevoice install` — put this version of Sidevoice on this machine and in front of the harnesses on it.
 *
 *  Under the install lock (§2.4, `release.mjs`): leftovers of a dead installer deleted; this package compared with the
 *  selection (`decide`: only a higher version, or a later nightly build, replaces it); staged as `R/releases/<id>` with
 *  its core runtime and both self-checks; a call in progress waited for (unless `--apply-now`); `current` switched; the
 *  job definitions written if their text changed and the jobs restarted on the new release (`--service`, or jobs
 *  already defined; with no manager, what runs on demand is stopped); the selection verified within 60 s — the core
 *  healthy at this release's version and speaking a compatible `api` and link, the connector answering at this
 *  release's version; on failure `current` flipped back to `previous`, restarted and verified again. A first install
 *  that fails is left installed and failing, said with the core's own key. Then every release and runtime neither link
 *  names is pruned. Run again, it verifies the selection and flips back if it does not run: the recovery.
 *
 *  Then the harnesses — each one found, or `--harness`, or none with `--no-agents` (the app's install: it connects agents
 *  only when the person picks them) — registered through `R/current`. Owned entries are changed under the install lock
 *  only when the selected executable format changes or rollback selects another format.
 *
 *  It pairs with nothing. Pairing is a person's act — the room shows a one-time code to whoever is in it, and a
 *  conversation asks for it the first time it joins — so the installer only reports whether this machine is
 *  paired, and with which room.
 *
 *  What it does not do is decide for the person: it never edits a machine-wide Codex configuration it
 *  does not own, it never relaxes Claude Code's inbound safeguard, and it never enables Linux lingering —
 *  those it prints, with the reason. Cursor's `mcp.json` is a map keyed by server name: only the `sidevoice`
 *  key is written, and only when it is absent or is one this package wrote. */
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { readdirSync, readFileSync, renameSync, rmSync, writeSync } from 'node:fs';
import { harnessesPresent } from './identity.mjs';
import { pairedRoom } from './pair.mjs';
import { CORE_VERSION, NO_UV, embeddedRustCoreIdentity, ensureRunning, findUv, readReady, takeInstallLock } from './core.mjs';
import { keyed, t } from './i18n.mjs';
import { normalizeInstallFailure } from './install-errors.mjs';
import { remove as removeSkill, skillsDir, status as skillStatus } from './skill.mjs';
import { candidate, coreProgram, decide, discardRuntimeIfUnselected, flipBack, markVerified, prune, releaseRoot, removeLeftovers, removeReleases, selectedDaemonCommand, selection, stableCommand, stage, switchTo } from './release.mjs';
import { matchesSelectedRustConnectorIdentity, RUST_CONNECTOR_KIND } from './rust-connector.mjs';
import { HARNESS_REGISTRATIONS, codexInstructions, cursorMcpFile, serverCommand, unregisterFromClaude, unregisterFromCodex, unregisterFromCursor } from './registrations.mjs';
import { askConnector, compatibleCore, installedService, jobDefinitions, linger, managerKind, recordInstallation, settledState, start as startService, startJobs, status, stop as stopService, stopOnDemand, uninstall as uninstallService, writeDefinitions } from './service.mjs';
import { dataDirOf, inhibitRuntimeLaunch, nodeFiles, nodeStopped, restoreSelectedLaunchGate } from './node-files.mjs';
import { crash, pause } from './testpoint.mjs';
import { captureAgentEnvironment, withAgentStateLock } from './agents.mjs';
import { readTrusted } from './secure-fs.mjs';

export { claudeRegistration, codexInstructions, copiesDir, cursorHasOurs, cursorMcpFile, registerWithCursor, serverCommand,
  unregisterFromCursor } from './registrations.mjs';
export { compareVersions, decide } from './release.mjs';

const VERIFY_MS = () => Number(process.env.SIDEVOICE_INSTALL_VERIFY_MS || 60_000);
const CALLS_POLL_MS = 2000;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** Runtime-kind changes cannot carry an unclassified speech row across the JS/Rust ownership boundary. */
function requireEmptyOutbox(dataDir) {
  const file = path.join(dataDir, 'outbox.json');
  let raw;
  try { raw = readTrusted(file); }
  catch { throw keyed('install.runtime-switch-outbox'); }
  if (raw === null) return;
  if (raw.length > 8 * 1024 * 1024) throw keyed('install.runtime-switch-outbox');
  let queue;
  try { queue = JSON.parse(raw); } catch { throw keyed('install.runtime-switch-outbox'); }
  if (!Array.isArray(queue) || queue.length) throw keyed('install.runtime-switch-outbox');
}

/** Quiesce the only writer before the final cross-runtime outbox decision. */
async function quiesceRuntime(env, { stoppedBefore = nodeStopped(dataDirOf(env), selection(env, 'current')?.id) } = {}) {
  if (installedService(env)) {
    let stopped;
    try { stopped = await stopService(env); }
    catch (error) { if (!stoppedBefore) await startService(env); throw error; }
    if (!stopped.ok) { if (!stoppedBefore) await startService(env); throw keyed('service.unload-failed', { detail: 'the previous connector did not exit' }); }
    return async () => { if (!stoppedBefore) await startService(env); };
  }
  const wasRunning = !!(await askConnector('status', {}, { env, timeout: 1000 }))?.version;
  const down = await stopOnDemand(env, { selectedRelease: selection(env, 'current')?.release });
  if (down.left.length) throw keyed('service.unload-failed', { detail: `pid ${down.left.join(', ')} still running` });
  return async () => {
    if (!wasRunning || stoppedBefore) return;
    await ensureRunning({ dataDir: dataDirOf(env), env, bin: coreProgram(env) });
    const [program, ...args] = selectedDaemonCommand(env);
    const child = spawn(program, [...args, 'connector'], { detached: true, stdio: 'ignore', env });
    child.on('error', () => {});
    child.unref();
    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline) {
      if ((await askConnector('status', {}, { env, timeout: 500 }))?.version) return;
      await wait(100);
    }
    throw keyed('connector.not-started');
  };
}

async function verifyUnmanagedRustConnector(env, release) {
  const selected = () => askConnector('identity', {}, { env, timeout: 1000 });
  if (matchesSelectedRustConnectorIdentity(await selected(), release, releaseRoot(env))) return true;
  const [program, ...prefix] = selectedDaemonCommand(env);
  const child = spawn(program, [...prefix, 'connector'], { detached: true, stdio: 'ignore', env });
  child.on('error', () => {});
  child.unref();
  const deadline = Date.now() + VERIFY_MS();
  while (Date.now() < deadline) {
    if (matchesSelectedRustConnectorIdentity(await selected(), release, releaseRoot(env))) return true;
    await wait(200);
  }
  return false;
}

/** Automatic recovery must never let the other runtime replay a failed release's rows. */
function quarantineOutbox(dataDir, from, to) {
  if ((from?.runtime_kind ?? 'javascript') === (to?.runtime_kind ?? 'javascript')) return null;
  try { requireEmptyOutbox(dataDir); return null; }
  catch (error) {
    if (error.key !== 'install.runtime-switch-outbox') throw error;
    const file = path.join(dataDir, 'outbox.json');
    const held = path.join(dataDir, `outbox.${Date.now()}.${process.pid}.quarantine.json`);
    renameSync(file, held);
    return held;
  }
}

export function flag(argv, name) {
  const index = argv.indexOf(name);
  return index >= 0 ? argv[index + 1] : undefined;
}

/** What Cursor can and cannot do with a room, said once at install so nobody expects more. */
export function cursorNotes() {
  return [
    'Cursor: ask a chat to join the voice room; it speaks its replies there. The first time, Cursor asks to approve the',
    'new MCP server (cursor-agent: or run  cursor-agent mcp enable sidevoice ). What you say in the room reaches Cursor',
    'only by experimental routes, because Cursor offers none of its own:',
    '  - Cursor CLI: only a chat started with  cursor-agent persist  (needs tmux); the room types into its terminal.',
    '  - Cursor editor: a small Sidevoice card appears under the join call; while it stays open in that chat, the room',
    '    sends to it. Several chats of a window can join, each with its own card.',
    'If you type while the room sends, the two mix.',
  ].join('\n');
}

/** Claude Code holds messages from other local processes when a session bypasses permission prompts. */
export function inboundWarning(env = process.env) {
  const settings = path.join(env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'), 'settings.json');
  let parsed = {};
  try { parsed = JSON.parse(readFileSync(settings, 'utf8')); } catch { return null; }
  if (parsed.crossSessionInbound) return null;
  if (parsed.permissions?.defaultMode !== 'bypassPermissions') return null;
  return [
    'This machine runs Claude Code sessions in bypassPermissions, and those hold what the room sends',
    'instead of delivering it — voice looks sent and never arrives. Either start a session with',
    `  --settings '{"crossSessionInbound":"accept"}'`,
    `or add "crossSessionInbound": "accept" to ${settings}. That second one lets any local process post`,
    'into every Claude session on this machine, which is the safeguard it removes: your call, not ours.',
  ].join('\n');
}

/* ----- verify, flip back (§2.4 steps 6–8) ----- */

/** Step 7: the selection runs — the core healthy at `release`'s core version, speaking a compatible `api` and link, and
 *  (with jobs) the connector answering at `release`'s version — within `VERIFY_MS`. A failure the core reported, or a
 *  job its manager will not run, ends the wait at once. With no manager, the core is started on demand from `current`
 *  (it leaves by itself when unused); `fresh`: it must be that very start — a core already serving, of whatever
 *  release, proves nothing about this one. `{ok}` or `{ok: false, failure}`. */
async function verify(env, release, kind, { fresh = false, previousLaunchId = null } = {}) {
  const dataDir = dataDirOf(env);
  if (kind === 'none') {
    try {
      const ready = await ensureRunning({ dataDir, env, bin: coreProgram(env), fresh });
      if (ready.version !== release.core) return { ok: false, failure: { key: 'install.not-selected', message: t('install.not-selected', { detail: `core ${ready.version}` }) } };
      if (!compatibleCore(ready, ready)) return { ok: false, failure: { key: 'install.incompatible', message: t('install.incompatible') } };
      if (release.runtime_kind === RUST_CONNECTOR_KIND) {
        return await verifyUnmanagedRustConnector(env, release)
          ? { ok: true } : { ok: false, failure: { key: 'install.not-selected', message: t('install.not-selected', { detail: 'the Rust Connector did not answer' }) } };
      }
      return { ok: true };
    } catch (error) { return { ok: false, failure: error.failure ?? { key: error.key || 'install.verify', message: error.message } }; }
  }
  const deadline = Date.now() + VERIFY_MS();
  let last = null;
  let staleLaunch = false;
  while (Date.now() < deadline) {
    last = await status(env);
    if (last.state === 'running' && last.core?.version === release.core) {
      if (previousLaunchId && last.core.launch_id === previousLaunchId) {
        staleLaunch = true;
        await wait(250);
        continue;
      }
      if (!compatibleCore(last.core, readReady(dataDir))) return { ok: false, failure: { key: 'install.incompatible', message: t('install.incompatible') } };
      if (release.runtime_kind === RUST_CONNECTOR_KIND) {
        const identity = await askConnector('identity', {}, { env, timeout: 1500 });
        if (matchesSelectedRustConnectorIdentity(identity, release, releaseRoot(env))) return { ok: true };
      } else {
        const answered = await askConnector('status', {}, { env, timeout: 1500 });
        if (answered?.version === release.connector) return { ok: true };
      }
    }
    if (settledState(last) && last.state !== 'running') return { ok: false, failure: last.failure };
    await wait(250);
  }
  return { ok: false, failure: staleLaunch
    ? { key: 'install.not-selected', message: t('install.not-selected', { detail: 'the previous Core launch still serves' }) }
    : last?.failure ?? { key: 'ready.timeout', message: t('ready.timeout') } };
}

/** Whether the jobs run the selection already (a re-run finds them on it, or on what ran before a crash). */
async function runsSelection(env, release) {
  if (selection(env, 'verified')?.id !== release.id) return false;
  const now = await status(env);
  if (now.state !== 'running' || now.core?.version !== release.core) return false;
  if (release.runtime_kind === RUST_CONNECTOR_KIND) {
    return matchesSelectedRustConnectorIdentity(await askConnector('identity', {}, { env, timeout: 1500 }), release, releaseRoot(env));
  }
  return (await askConnector('status', {}, { env, timeout: 1500 }))?.version === release.connector;
}

/** Step 6 for whatever is selected now: the jobs restarted on it (with a manager), or what runs on demand stopped. */
async function restartOn(env, kind) {
  recordInstallation(env);
  if (kind === 'none') { await stopOnDemand(env); return; }
  const changed = writeDefinitions(kind, env);
  recordInstallation(env, { definitions: jobDefinitions(kind, env) });
  await startJobs(env, { changed, restart: true });
}

/** Re-point only registrations that still belong to us after a format-changing rollback. */
function reconcileOwnedRegistrations(env) {
  const record = { command: stableCommand(env) };
  const failures = [];
  for (const [name, operations] of Object.entries(HARNESS_REGISTRATIONS)) {
    if (operations.state(env).state !== 'ours') continue;
    try { if (operations.set(env, record) === 'invalid') failures.push(name); }
    catch { failures.push(name); }
  }
  return failures;
}

/** Set the selected command while `apply` still holds the install lock, so an older install cannot write after a newer
 *  format switch. The caller formats these outcomes for its response after the transaction returns. */
function selectRegistrations(env, harnesses) {
  const record = { command: stableCommand(env) };
  const results = {};
  for (const [name, operations] of Object.entries(HARNESS_REGISTRATIONS)) {
    const before = operations.state(env).state;
    if (!harnesses.includes(name) && before !== 'ours') continue;
    if (name === 'claude' && before === 'absent' && !operations.reachable(env)) {
      results[name] = { before, outcome: 'manual' };
      continue;
    }
    try { results[name] = { before, outcome: operations.set(env, record) }; }
    catch (error) { results[name] = { before, error: String(error.message || error).split('\n')[0] }; }
  }
  return results;
}

/** Step 8: back to the last verified release (`flipBack`), restarted, verified again. Null when there is nothing to go
 *  back to. */
async function goBack(env, kind, progressEvent = () => {}, { explicit = false } = {}) {
  progressEvent({ step: 'rollback', done: null, total: null });
  const current = selection(env, 'current')?.release;
  const previous = ['verified', 'previous'].map(name => selection(env, name)?.release)
    .find(release => release && release.id !== current?.id);
  if (!previous) return null;
  let resume = null, quarantined = null;
  const runtimeKindChanged = (current?.runtime_kind ?? 'javascript') !== (previous.runtime_kind ?? 'javascript');
  const quiesce = runtimeKindChanged || (current?.runtime_kind === RUST_CONNECTOR_KIND && current.id !== previous.id);
  const stoppedBefore = nodeStopped(dataDirOf(env), current?.id);
  const uninhibit = quiesce ? inhibitRuntimeLaunch(dataDirOf(env)) : null;
  try {
    if (quiesce) resume = await quiesceRuntime(env, { stoppedBefore });
    await pause('rollback-after-quiesce');
    if (runtimeKindChanged) {
      if (explicit) requireEmptyOutbox(dataDirOf(env));
      else quarantined = quarantineOutbox(dataDirOf(env), current, previous);
    }
    const back = flipBack(env);
    if (!back) return null;
    uninhibit?.commit(back.id, back.runtime_kind);
    restoreSelectedLaunchGate(dataDirOf(env), back.id);
    await restartOn(env, kind);
    const verified = await verify(env, back, kind, { fresh: kind === 'none' });
    if (verified.ok) markVerified(env, back.id);
    return { release: back, ...verified, ...(quarantined ? { quarantined } : {}) };
  } catch (error) { uninhibit?.restore(); if (resume && selection(env, 'current')?.release?.id === current?.id) await resume(); throw error; }
  finally { uninhibit?.restore(); }
}

/** Wait for every call on this machine to end (an update that changes the core would end them). */
async function callsEnd(env, progress, { signal, progressEvent = () => {} } = {}) {
  let said = false;
  for (;;) {
    if (signal?.aborted) throw keyed('install.cancelled');
    const now = await status(env);
    if (!now.calls) return;
    if (!said) { progress(t('install.progress.calls', { calls: now.calls })); progressEvent({ step: 'wait-calls', done: null, total: null }); said = true; }
    await wait(CALLS_POLL_MS);
  }
}

/** Steps 1–9 under the install lock. `core: false` (`--no-core`) stages no core: nothing to run, nothing verified.
 *  Returns `{action: 'install'|'upgrade'|'noop'|'rollback'|'failed', release, from, failure?, back?, kind}`. */
export async function apply(env, { core = true, service = false, applyNow = false, progress = () => {}, log = () => {},
  afterSelection = null, signal, progressEvent = () => {}, beginCommit = () => {}, candidateRelease = null } = {}) {
  if (embeddedRustCoreIdentity() && !core) throw keyed('install.authenticity', { check: 'manifest' });
  const dataDir = dataDirOf(env);
  let stagedRuntimeId = null;
  const release = await takeInstallLock(dataDir, log, { signal, onWait: () => progressEvent({ step: 'wait-lock', done: null, total: null }) });
  let conversionUninhibit = null, conversionResume = null;
  try {
    if (signal?.aborted) throw keyed('install.cancelled');
    // Capture login-shell agent paths after taking the install lock, before any selected service can start.
    captureAgentEnvironment(env);
    removeLeftovers(env);
    const current = selection(env, 'current')?.release ?? null;
    // A release without a core (`--no-core`) is one of its own, and does not satisfy an install that needs the core:
    // that one stages a complete release instead.
    const proposed = candidateRelease ?? candidate(env);
    const next = { ...proposed, ...(core ? {} : { id: `${proposed.id}-nocore` }) };
    let action = decide(current, next);
    if (action === 'noop' && core && current && !current.core_build) action = 'upgrade';
    const runtimeKindChanged = (current?.runtime_kind ?? 'javascript') !== (next.runtime_kind ?? 'javascript');
    if (action !== 'noop' && runtimeKindChanged) requireEmptyOutbox(dataDir);
    let chosen = current;
    if (action !== 'noop') {
      chosen = await stage(env, next, { dataDir, core, log, progress, signal, progressEvent,
        onRuntime: runtime => { stagedRuntimeId = runtime.id; } });
      stagedRuntimeId = chosen.core_build ?? stagedRuntimeId;
      const selectedRuntimeChanged = !current || (current.runtime_kind ?? 'javascript') !== (chosen.runtime_kind ?? 'javascript')
        || (chosen.runtime_kind === 'rust-native-v1' && current.runtime_sha256 !== chosen.runtime_sha256);
      if (current && !applyNow && (current.core_build !== chosen.core_build || selectedRuntimeChanged)) await callsEnd(env, progress, { signal, progressEvent });
      if (signal?.aborted) throw keyed('install.cancelled');
      await pause('install-before-commit', { signal });
      const quiesce = runtimeKindChanged || (current?.runtime_kind === RUST_CONNECTOR_KIND && current.id !== chosen.id);
      const stoppedBefore = nodeStopped(dataDir, current?.id);
      const uninhibit = quiesce ? inhibitRuntimeLaunch(dataDir) : null;
      let resume = null;
      try {
        if (quiesce) resume = await quiesceRuntime(env, { stoppedBefore });
        await pause('install-after-quiesce', { signal });
        if (runtimeKindChanged) requireEmptyOutbox(dataDir);
        if (signal?.aborted) throw keyed('install.cancelled');
        progressEvent({ step: 'commit', done: null, total: null });
        beginCommit();
        switchTo(env, chosen.id);
        uninhibit?.commit(chosen.id, chosen.runtime_kind);
      } catch (error) { uninhibit?.restore(); if (resume) await resume(); throw error; }
      finally { uninhibit?.restore(); }
    }
    if (action === 'noop') {
      const conversionKind = core && service && !installedService(env) ? managerKind(env) : 'none';
      if (conversionKind !== 'none' && current?.runtime_kind === RUST_CONNECTOR_KIND) {
        const stoppedBefore = nodeStopped(dataDir, current.id);
        conversionUninhibit = inhibitRuntimeLaunch(dataDir);
        conversionResume = await quiesceRuntime(env, { stoppedBefore });
        await pause('install-after-quiesce', { signal });
      }
      progressEvent({ step: 'commit', done: null, total: null });
      beginCommit();
    }
    // Clearing the stop intent is part of the committed start. Cancellation while staging leaves it untouched.
    restoreSelectedLaunchGate(dataDir, chosen.id);
    await pause('install-after-commit');
    recordInstallation(env);
    const kind = core && (service || installedService(env)) ? managerKind(env) : 'none';
    let previousLaunchId = null;
    if (!core || !chosen.core_build) {
      const registrations = afterSelection?.();
      prune(env, dataDir);
      return { action, release: chosen, from: current, kind: 'none', registrations };
    }
    if (kind !== 'none') {
      const had = installedService(env);
      const changed = writeDefinitions(kind, env);
      recordInstallation(env, { definitions: jobDefinitions(kind, env) });
      crash('definitions-written');
      if (!had && !conversionResume) await stopOnDemand(env);
      conversionUninhibit?.commit(chosen.id, chosen.runtime_kind); conversionUninhibit = null;
      // The files may match after a definitions-written crash while the manager still has the prior release cached.
      // An unverified selection must reload both definitions before the restart can verify that selection.
      if (selection(env, 'verified')?.id !== chosen.id) {
        for (const job of ['core', 'connector']) if (!changed.includes(job)) changed.push(job);
      }
      const oldLaunchId = readReady(dataDir)?.launch_id ?? null;
      const restart = action !== 'noop' || !(await runsSelection(env, chosen));
      if (restart) previousLaunchId = oldLaunchId;
      if (restart) progressEvent({ step: 'service-start', done: null, total: null });
      await startJobs(env, { changed, restart });
    }
    // Verified on a noop too, with or without a manager: running install again is how a selection is recovered. With no
    // manager, a selection not yet verified (or just switched to) is verified by a core started from it: what runs on
    // demand is stopped first — a core of the release before would answer for it otherwise.
    const fresh = kind === 'none' && (action !== 'noop' || selection(env, 'verified')?.id !== chosen.id);
    if (fresh) await stopOnDemand(env);
    const verified = await verify(env, chosen, kind, { fresh, previousLaunchId });
    if (verified.ok) {
      markVerified(env, chosen.id);
      const registrations = afterSelection?.();
      prune(env, dataDir);
      return { action, release: chosen, from: current, kind, registrations };
    }
    const back = await goBack(env, kind, progressEvent);
    if (!back) return { action: 'failed', release: chosen, from: current, failure: verified.failure, kind };
    const registrationFailures = reconcileOwnedRegistrations(env);
    if (back.ok) prune(env, dataDir);
    return { action: 'rollback', release: back.release, from: current, failed: chosen, failure: verified.failure,
      back: back.ok, backFailure: back.failure ?? null, registrationFailures, kind,
      ...(back.quarantined ? { quarantined: back.quarantined } : {}) };
  } catch (error) {
    if (conversionUninhibit) { conversionUninhibit.restore(); conversionUninhibit = null; await conversionResume?.(); }
    if (signal?.aborted || error?.key === 'install.runtime-switch-outbox') {
      prune(env, dataDir);
      discardRuntimeIfUnselected(env, dataDir, stagedRuntimeId);
    }
    throw error;
  } finally { conversionUninhibit?.restore(); release(); }
}

/* ----- install ----- */

const USAGE = 'usage: sidevoice install [--harness claude|codex|cursor] [--no-agents] [--service] [--no-core] [--apply-now] [--json]';

export async function install(argv = [], env = process.env, { progress = () => {}, signal, progressEvent = () => {}, beginCommit = () => {} } = {}) {
  const stray = argv.find(item => !item.startsWith('-') && argv[argv.indexOf(item) - 1] !== '--harness');
  if (stray) throw keyed('install.usage', { usage: USAGE });
  const wanted = flag(argv, '--harness');
  // `--no-agents`: nothing registered with any harness — the person picks agents later (W3, `sidevoice agents`).
  const harnesses = argv.includes('--no-agents') ? [] : wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];

  // A core somebody else runs (`SIDEVOICE_URL`…) is not this installer's; `--no-core` leaves it for later.
  const externalCore = env.SIDEVOICE_URL && env.SIDEVOICE_CONNECTOR_ID && env.SIDEVOICE_CONNECTOR_TOKEN;
  const nativeCore = embeddedRustCoreIdentity();
  if (nativeCore && (argv.includes('--no-core') || externalCore || env.SIDEVOICE_CORE_BIN
      || env.SIDEVOICE_CORE_SPEC || env.SIDEVOICE_CORE_WHEEL_DIR)) {
    throw keyed('install.authenticity', { check: 'manifest' });
  }
  const core = nativeCore ? true : !argv.includes('--no-core') && !externalCore;
  const localCoreOverride = !!(env.SIDEVOICE_CORE_SPEC || env.SIDEVOICE_CORE_WHEEL_DIR);
  if (core && !env.SIDEVOICE_CORE_BIN && !findUv(env) && localCoreOverride) throw new Error(NO_UV);
  const result = await apply(env, { core, service: argv.includes('--service'), applyNow: argv.includes('--apply-now'), progress,
    signal, progressEvent, beginCommit,
    afterSelection: () => selectRegistrations(env, harnesses) });
  const record = result.release;
  done.push(t('install.version', { version: candidate(env).connector }));
  if (result.action === 'rollback') {
    const words = { to: result.failed.id, from: result.release.id, cause: result.failure?.key ?? '?', back: result.backFailure?.key ?? '?' };
    if (result.registrationFailures?.length) throw keyed('install.rollback-registration', {
      ...words, back: result.back ? 'running' : result.backFailure?.key ?? 'failed', harnesses: result.registrationFailures.join(', '),
    }, { result });
    throw keyed(result.back ? 'install.rollback' : 'install.rollback-failed', words, { failure: result.failure, result });
  }
  if (result.action === 'failed') throw keyed('install.verify', { cause: result.failure?.key ?? '?' }, { failure: result.failure, result });
  if (result.action === 'noop') done.push(t('install.noop', { id: record.id, channel: record.channel }));
  else done.push(t('install.selected', { id: record.id, previous: result.from?.id ?? t('install.nothing') }));
  if (!core) done.push(externalCore ? t('install.external-core', { url: env.SIDEVOICE_URL }) : t('install.no-core'));

  let serviceStatus = null;
  if (core) {
    serviceStatus = await status(env);
    done.push(t(result.kind === 'none' ? 'install.on-demand' : 'install.service', { state: serviceStatus.state, service: result.kind }));
    if (result.kind === 'systemd') { const lingering = linger(env); if (!lingering.enabled) next.push(`${lingering.reason}\n    ${lingering.command}`); serviceStatus = { ...serviceStatus, linger: lingering }; }
    const ready = readReady(dataDirOf(env));
    if (serviceStatus.reachable && ready) done.push(t('install.core-answering', { version: CORE_VERSION, url: ready.url }));
  }

  // Registered once, through `R/current`: the ones the person asked for now, and ours from before re-pointed there.
  const selectedRecord = { command: stableCommand(env) };
  const { command: shown, args } = serverCommand(env, selectedRecord);
  for (const [name, operations] of Object.entries(HARNESS_REGISTRATIONS)) {
    const selected = result.registrations?.[name];
    if (!selected) continue;
    const { before, outcome, error } = selected;
    if (outcome === 'manual') {
      if (name === 'claude') done.push(t('install.claude-unreachable', { manual: `claude mcp add --scope user sidevoice -- ${[shown, ...args].join(' ')}` }));
      continue;
    }
    if (error) {
      done.push(name === 'codex' ? t('install.codex-failed') : t(`install.${name}-failed`, { detail: error }));
      continue;
    }
    if (name === 'codex' && outcome === 'unknown') {
      done.push(t('agents.registration-unknown', { agent: t('harness.codex', {}, env) }));
      continue;
    }
    if (outcome === 'added' || outcome === 'repointed') done.push(t(`install.${name}-registered`, { file: cursorMcpFile(env) }));
    else if (outcome === 'foreign') done.push(name === 'claude'
      ? t('install.claude-foreign', { line: operations.state(env).line, manual: `claude mcp add --scope user sidevoice -- ${[shown, ...args].join(' ')}` })
      : name === 'codex' ? t('agents.foreign', { agent: t('harness.codex', {}, env) })
        : t('cursor.foreign', { file: cursorMcpFile(env), manual: '' }));
    else if (outcome === 'invalid') done.push(name === 'codex'
      ? t('agents.invalid', { agent: t('harness.codex', {}, env) })
      : t('cursor.invalid', { file: cursorMcpFile(env), why: operations.state(env).why, manual: '' }));
  }
  // The join shortcut is a prompt the server offers; a skill copy from an earlier version is taken away.
  if (harnesses.includes('claude') && skillStatus(skillsDir([], env)).state === 'installed') done.push(t('install.skill-removed', { target: removeSkill(skillsDir([], env)).target }));

  progressEvent({ step: 'pairing', done: null, total: null });
  const paired = pairedRoom(env);
  done.push(paired ? `This machine is paired with ${paired.origin} (connector ${paired.connector_id}).`
                   : 'This machine is not paired with any room yet.');
  if (!paired && core) next.push(t('install.route'));
  if (harnesses.includes('claude')) {
    next.push('In a conversation, ask to join the voice room (or run /mcp__sidevoice__voice-room).' +
              (paired ? '' : ' With no room paired it joins this machine only: the Sidevoice app on this computer reaches it once you ask the conversation to pair a device. For a room, give the conversation its address and the code it shows under "Emparejar máquina".'));
    next.push('Sessions already open need a restart before they see the server.');
    const warning = inboundWarning(env);
    if (warning) next.push(warning);
  }
  if (harnesses.includes('codex') && result.registrations?.codex?.outcome === 'manual') next.push(codexInstructions(env, selectedRecord));
  if (harnesses.includes('cursor')) next.push(cursorNotes());
  if (!harnesses.length && !argv.includes('--no-agents')) next.push('No harness found on this machine. Pass --harness claude, --harness codex or --harness cursor.');
  return { done, next, result, service: serviceStatus };
}

/** `sidevoice install [--json]`: with `--json`, one object for the app — progress goes to stderr then. */
export async function runInstall(argv = [], env = process.env) {
  const json = argv.includes('--json');
  const requestedProgress = argv.find(item => item.startsWith('--progress='));
  const structured = requestedProgress === '--progress=jsonl';
  if (requestedProgress && !structured || structured && !json) {
    const error = keyed('install.usage', { usage: USAGE });
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key, message: error.message } }));
    else console.error(error.message);
    return 1;
  }
  const controller = new AbortController();
  let commitStarted = false, progressOpen = structured;
  const onInterrupt = () => { if (!commitStarted) controller.abort(); };
  if (structured) process.on('SIGINT', onInterrupt);
  const emitProgress = event => {
    if (!progressOpen) return;
    const steps = new Set(['download', 'verify', 'stage', 'service-start', 'wait-calls', 'wait-lock', 'commit', 'pairing', 'rollback']);
    if (!steps.has(event?.step)) return;
    const safeCount = value => Number.isSafeInteger(value) && value >= 0 ? value : null;
    let done = safeCount(event.done), total = safeCount(event.total);
    if (done === null || total !== null && total < done) { done = null; total = null; }
    const record = JSON.stringify({ type: 'progress', step: event.step, done, total });
    try { writeSync(2, `${record}\n`); }
    catch { progressOpen = false; }
  };
  try {
    const { done, next, result, service } = await install(argv.filter(item => item !== '--json' && item !== '--progress=jsonl'), env, {
      signal: controller.signal,
      beginCommit: () => { commitStarted = true; },
      progressEvent: emitProgress,
      progress: line => { if (!structured) (json ? console.error(line) : console.log(line)); },
    });
    if (json) {
      console.log(JSON.stringify({ ok: true, action: result.action, installed: result.release.id, connector: result.release.connector, core: result.release.core,
        channel: result.release.channel, command: stableCommand(env), service: result.kind, state: service?.state ?? null,
        ...(service?.linger ? { linger: service.linger } : {}) }));
      return 0;
    }
    for (const line of done) console.log('· ' + line);
    if (next.length) {
      console.log('\nLeft for you:');
      for (const line of next) console.log('\n' + line);
    }
    return 0;
  } catch (error) {
    error = normalizeInstallFailure(error);
    if (json) {
      const key = error.key || 'install.failed';
      const params = key === 'install.authenticity' && /^[a-z0-9][a-z0-9-]{0,63}$/.test(error.check || '') ? { check: error.check } : undefined;
      console.log(JSON.stringify({ ok: false, error: { key, message: error.message, ...(params ? { params } : {}) }, ...(error.failure ? { failure: error.failure } : {}),
        ...(error.result?.quarantined ? { quarantined: error.result.quarantined } : {}) }));
    }
    else {
      console.error(error.message);
      if (error.result?.quarantined) console.error(t('install.outbox-quarantined', { path: error.result.quarantined }));
    }
    return 1;
  } finally {
    if (structured) process.removeListener('SIGINT', onInterrupt);
  }
}

/** `sidevoice rollback [--json]`: `current` back to `previous` (§2.4 step 8 alone), the jobs restarted on it and verified. */
export async function rollback(env = process.env) {
  const dataDir = dataDirOf(env);
  const release = await takeInstallLock(dataDir);
  try {
    const kind = installedService(env) ? managerKind(env) : 'none';
    const from = selection(env, 'current')?.release ?? null;
    const back = await goBack(env, kind, () => {}, { explicit: true });
    if (!back) throw keyed('install.no-previous');
    const registrationFailures = reconcileOwnedRegistrations(env);
    if (registrationFailures.length) throw keyed('install.rollback-registration', {
      to: 'rollback', from: back.release.id, cause: from?.id ?? '?', back: back.ok ? 'running' : back.failure?.key ?? 'failed',
      harnesses: registrationFailures.join(', '),
    }, { failure: back.failure });
    if (!back.ok) throw keyed('install.rollback-failed', { to: from?.id ?? '?', from: back.release.id, cause: 'rollback', back: back.failure?.key ?? '?' }, { failure: back.failure });
    return { ok: true, action: 'rollback', installed: back.release.id, from: from?.id ?? null, service: kind };
  } finally { release(); }
}
export async function runRollback(argv = [], env = process.env) {
  const json = argv.includes('--json');
  try {
    const result = await rollback(env);
    console.log(json ? JSON.stringify(result) : t('install.rolled-back', { id: result.installed, from: result.from ?? t('install.nothing') }));
    return 0;
  } catch (error) {
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key || 'install.failed', message: error.message }, ...(error.failure ? { failure: error.failure } : {}) }));
    else console.error(error.message);
    return 1;
  }
}

/* ----- uninstall (§2.5) ----- */

/** `sidevoice uninstall`: the reverse of install, in the order that leaves nothing pointing at what is gone — under the
 *  install lock, the stop written, both jobs unloaded (a refusal stops everything here: nothing deleted), what runs on
 *  demand stopped, the definitions deleted (`service.mjs`); then our harness registrations; then the releases (`R`) and
 *  the data directory — all of it but its permanent lock files (SEAMS §1) and the stop. The room keeps this machine's
 *  pairing until it is revoked under "Máquinas" on the room's page — said, with where. Codex removes only an entry
 *  confirmed as ours through its own CLI. */
export async function uninstall(argv = [], env = process.env) {
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : [...new Set([...Object.keys(HARNESS_REGISTRATIONS), ...harnessesPresent(env)])];
  const done = [], next = [];
  const dataDir = dataDirOf(env);
  const release = await takeInstallLock(dataDir);
  try {
    try {
      const service = await uninstallService(env, { keepStopped: true });
      done.push(t('uninstall.service-removed', { service: service.service }));
      if (service.note) done.push(service.note);
    } catch (error) {
      throw Object.assign(new Error(`${error.message} ${t('uninstall.stopped', { data: dataDir })}`), { key: error.key });
    }
    if (harnesses.includes('claude')) {
      unregisterFromClaude(done, next, env);
      if (skillStatus(skillsDir([], env)).state === 'installed') done.push(`Removed the voice-room skill copy at ${removeSkill(skillsDir([], env)).target}.`);
    }
    if (harnesses.includes('codex')) unregisterFromCodex(done, next, env);
    if (harnesses.includes('cursor')) unregisterFromCursor(done, next, env);
    const releasesRoot = releaseRoot(env);
    removeReleases(env);
    done.push(t('uninstall.releases-removed', { root: releasesRoot }));
    const paired = pairedRoom(env);
    // Kept: the permanent lock files and the stop, so that a connector already on its way — a façade's
    // launcher that started one just before — finds it and does not serve. Only an install, `service start` or the
    // connector job at a login clears it.
    const kept = new Set(['install.lock', 'connector.lock', 'agents.lock', 'node-stopped.json']);
    let entries = [];
    withAgentStateLock(env, () => {
      try { entries = readdirSync(dataDir); } catch {}
      for (const name of entries) if (!kept.has(name)) rmSync(path.join(dataDir, name), { recursive: true, force: true });
    });
    if (entries.length) {
      done.push(`Removed what was in ${dataDir} (credential, socket, outbox, logs, the core and its environment).`);
      if (paired) next.push(`The room at ${paired.origin} still lists this machine as paired (connector ${paired.connector_id}) until you revoke it under "Máquinas" on the room's page.`);
    }
  } finally { release(); }
  next.push('Sessions already open keep their MCP server until they end.');
  return { done, next };
}

/** `sidevoice uninstall [--json]`. */
export async function runUninstall(argv = [], env = process.env) {
  const json = argv.includes('--json');
  try {
    const { done, next } = await uninstall(argv.filter(item => item !== '--json'), env);
    if (json) { console.log(JSON.stringify({ ok: true, state: 'absent' })); return 0; }
    for (const line of done) console.log('· ' + line);
    if (next.length) { console.log('\nLeft for you:'); for (const line of next) console.log('\n' + line); }
    return 0;
  } catch (error) {
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key || 'uninstall.failed', message: error.message } }));
    else console.error(error.message);
    return 1;
  }
}
