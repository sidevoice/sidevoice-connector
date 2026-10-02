/** Detection and consented actions for the supported coding agents on this host. */
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { agentHarnesses } from './harnesses.mjs';
import { agentEvidence, captureLoginPath, connectorMcpCommand, resolveAgentBinary } from './agent-support.mjs';
import { tryLockSync } from './lockfile.mjs';
import { dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';
import { keyed, t } from './i18n.mjs';

const IDS = Object.freeze(['claude', 'codex', 'cursor']);
const EMPTY_STORE = () => ({ version: 1, scanned_at: null, login_path: null, binaries: {}, seen: {}, dismissed: {} });

function fileOf(env) { return nodeFiles(dataDirOf(env)).agents; }
function lockOf(env) { return nodeFiles(dataDirOf(env)).agentsLock; }

function loadStore(env) {
  let store;
  try { store = readJson(fileOf(env)); } catch (error) { throw error; }
  if (!store || store.version !== 1 || typeof store !== 'object') return EMPTY_STORE();
  return { ...EMPTY_STORE(), ...store, binaries: store.binaries || {}, seen: store.seen || {}, dismissed: store.dismissed || {} };
}

function signatureOf(agent) {
  const identity = { id: agent.id, version: agent.version || null, evidence: agent.evidence };
  return createHash('sha256').update(JSON.stringify(identity)).digest('hex');
}

function inspectOne(id, env, store, { includeVersion = true } = {}) {
  const module = agentHarnesses[id];
  const binary = resolveAgentBinary(id, env, { loginPath: store.login_path, binaries: store.binaries });
  const evidence = agentEvidence(id, env, binary);
  const raw = module.detect(env, { binary, includeVersion });
  if (!includeVersion && !raw.version) raw.version = store.seen[id]?.agent?.version || null;
  return { ...raw, evidence, present: evidence.length > 0, version: raw.version || null, binary };
}

export function withAgentStateLock(env, action) {
  const lockFile = lockOf(env);
  const pauseBuffer = new Int32Array(new SharedArrayBuffer(4));
  const deadline = Date.now() + 10_000;
  let lock;
  do {
    lock = tryLockSync(lockFile, { env });
    if (lock.held) break;
    Atomics.wait(pauseBuffer, 0, 0, 2);
  } while (Date.now() < deadline);
  if (!lock?.held) throw keyed('lock.unavailable', { detail: 'timed out waiting for the agent state lock' });
  try { return action(); } finally { lock.release(); }
}

function withStoreTransaction(env, update, shouldSave = () => true) {
  return withAgentStateLock(env, () => {
    const store = loadStore(env);
    const value = update(store);
    const saved = shouldSave(store, value);
    if (saved) writePrivate(fileOf(env), store);
    return { store, value, saved };
  });
}

function scan(env, store, { watch = null, refresh = true, includeVersion = true, requireInstalled = false } = {}) {
  if (watch && !IDS.includes(watch)) throw keyed('agents.unknown', { id: watch });
  if (!refresh) return store;

  // CLI detection can take seconds. Do it outside the state lock, then merge the result under a short
  // cross-process transaction so a concurrent dismissal is preserved when the detected identity is unchanged.
  const base = store;
  const captured = watch && base.login_path ? null : captureLoginPath(env);
  const inspectedLoginPath = captured || base.login_path;
  const scanInputs = { ...base, login_path: inspectedLoginPath };
  const inspected = new Map();
  for (const id of watch ? [watch] : IDS) inspected.set(id, inspectOne(id, env, scanInputs, { includeVersion }));
  return withStoreTransaction(env, latest => {
    const inputsUnchanged = latest.login_path === base.login_path;
    if (inputsUnchanged && captured) latest.login_path = captured;
    for (const [id, agent] of inspected) {
      // A concurrent install-time capture may have changed the binary search inputs while this scan was running.
      // Leave that newer result alone and let the next scan use it.
      if (!inputsUnchanged || latest.binaries[id] !== base.binaries[id]) continue;
      if (agent.binary) latest.binaries[id] = agent.binary;
      else delete latest.binaries[id];
      const signature = agent.present ? signatureOf(agent) : null;
      const previous = latest.seen[id];
      const sameAppearance = previous?.present === true && previous.signature === signature;
      const generation = agent.present ? (sameAppearance ? previous.generation : randomUUID()) : previous?.generation || null;
      latest.seen[id] = {
        present: agent.present,
        signature,
        generation,
        detected_at: agent.present && !sameAppearance ? new Date().toISOString() : previous?.detected_at || null,
        agent: { ...agent, binary: undefined, actionable: false, dismissed: false },
      };
    }
    latest.scanned_at = new Date().toISOString();
  }, () => !requireInstalled || !!readJson(nodeFiles(dataDirOf(env)).install)).store;
}

function rowsFrom(store) {
  return IDS.flatMap(id => {
    const seen = store.seen[id];
    if (!seen?.present || !seen.agent) return [];
    const dismissed = !!seen.generation && store.dismissed[id] === seen.generation;
    const agent = { ...seen.agent, dismissed };
    agent.actionable = agent.present && agent.registration === 'not-connected' && !dismissed;
    return [agent];
  });
}

function listFrom(store, env) {
  const launch = connectorMcpCommand(env);
  return {
    agents: rowsFrom(store),
    scanned_at: store.scanned_at,
    custom: {
      command: [launch.command, ...launch.args].map(value => `'${String(value).replace(/'/g, `'\\''`)}'`).join(' '),
      snippet: JSON.stringify({ mcpServers: { sidevoice: { command: launch.command, args: launch.args } } }, null, 2),
      version: launch.version,
    },
  };
}

/** A full scan on the CLI or `rescan=1`; a manual panel may refresh one harness only. */
export function listAgents(env = process.env, { rescan = true, watch = null } = {}) {
  const loaded = loadStore(env);
  const store = scan(env, loaded, { refresh: rescan || !!watch || !loaded.scanned_at, watch });
  return listFrom(store, env);
}

/** The connector's detached startup/periodic scanner: skip before inspection when uninstalled and re-check
 *  installation under `agents.lock` before commit, coordinated with uninstall's state purge. */
export function scanInstalledAgents(env = process.env) {
  const files = nodeFiles(dataDirOf(env));
  if (!readJson(files.install)) return false;
  scan(env, loadStore(env), { refresh: true, requireInstalled: true });
  return true;
}

function errorFor(key, params = {}, env = process.env) {
  const error = keyed(key, params, { params });
  error.message = t(key, params, env);
  return error;
}

function fail(key, params = {}) { throw errorFor(key, params); }

function stateFor(store, id) {
  const seen = store.seen[id];
  if (!seen?.present || !seen.agent) fail('agents.not-present', { id, agent: t(`harness.${id}`) });
  return seen;
}

function actionFailure(outcome, id, env) {
  const params = { id, agent: t(`harness.${id}`, {}, env) };
  if (outcome === 'foreign') return errorFor('agents.foreign', params, env);
  if (outcome === 'invalid') return errorFor('agents.invalid', params, env);
  if (outcome === 'manual') return errorFor('agents.manual-required', params, env);
  if (outcome === 'unknown') return errorFor('agents.registration-unknown', params, env);
  return errorFor('agents.action-failed', params, env);
}

function reportError(onError, error) {
  try { onError?.(error); } catch {}
}

/** Perform one explicit action, then return the refreshed host state. */
export function agentAction(action, id, env = process.env, onError = null) {
  if (!['connect', 'disconnect', 'dismiss'].includes(action)) fail('agents.usage');
  if (!IDS.includes(id)) fail('agents.unknown', { id: id || '' });
  let store = scan(env, loadStore(env), { watch: id, refresh: true });
  let current = stateFor(store, id);
  const agent = current.agent;
  if (action === 'dismiss') {
    const committed = withStoreTransaction(env, latest => {
      const fresh = stateFor(latest, id);
      if (fresh.agent.registration === 'not-connected') latest.dismissed[id] = fresh.generation;
    });
    return listFrom(committed.store, env);
  }
  if (action === 'connect') {
    if (agent.registration === 'connected') return listFrom(store, env);
    if (agent.registration === 'foreign') throw actionFailure('foreign', id, env);
    if (agent.registration !== 'not-connected') throw actionFailure('unknown', id, env);
    let outcome;
    try { outcome = agentHarnesses[id].connect(env, undefined, { binary: store.binaries[id] || null }); }
    catch (error) { reportError(onError, error); throw errorFor('agents.action-failed', { id, agent: t(`harness.${id}`, {}, env) }, env); }
    if (!['added', 'repointed', 'unchanged'].includes(outcome)) throw actionFailure(outcome, id, env);
    store = scan(env, loadStore(env), { watch: id, refresh: true, includeVersion: false });
    current = stateFor(store, id);
    if (current.agent.registration !== 'connected') throw errorFor('agents.registration-not-confirmed', { agent: t(`harness.${id}`, {}, env) }, env);
    const committed = withStoreTransaction(env, latest => {
      const fresh = stateFor(latest, id);
      if (fresh.agent.registration === 'connected') delete latest.dismissed[id];
    });
    return listFrom(committed.store, env);
  }
  if (agent.registration === 'not-connected') return listFrom(store, env);
  if (agent.registration === 'foreign') throw actionFailure('foreign', id, env);
  if (agent.registration !== 'connected') throw actionFailure('unknown', id, env);
  let outcome;
  try { outcome = agentHarnesses[id].disconnect(env, { binary: store.binaries[id] || null }); }
  catch (error) { reportError(onError, error); throw errorFor('agents.action-failed', { id, agent: t(`harness.${id}`, {}, env) }, env); }
  if (!['removed', 'absent'].includes(outcome)) throw actionFailure(outcome, id, env);
  store = scan(env, loadStore(env), { watch: id, refresh: true, includeVersion: false });
  current = stateFor(store, id);
  return listFrom(store, env);
}

/** Core→connector link protocol. The core owns HTTP auth and relay policy; it may name only a registered id. */
export function handleAgentRequest(route, frame = {}, env = process.env, onError = null) {
  try {
    if (route === 'agents.list') {
      return listAgents(env, { rescan: frame.rescan === true, watch: typeof frame.watch === 'string' ? frame.watch : null });
    }
    const match = /^agents\.(connect|disconnect|dismiss)$/.exec(route);
    if (match) return agentAction(match[1], frame.id, env, onError);
    return { error: { key: 'agents.unknown-request', params: { route }, message: t('agents.unknown-request', { route }, env) } };
  } catch (error) {
    reportError(onError, error);
    const key = typeof error.key === 'string' ? error.key : 'agents.action-failed';
    const params = error.params && typeof error.params === 'object' ? error.params : {};
    return { error: { key, params, message: t(key, params, env) } };
  }
}

function writeResult(result, json, env) {
  if (json) { console.log(JSON.stringify(result)); return; }
  const byState = { connected: 'agents.state.connected', 'not-connected': 'agents.state.not-connected', foreign: 'agents.state.foreign', unknown: 'agents.state.unknown', invalid: 'agents.state.unknown' };
  if ('agents' in result) {
    if (!result.agents.length) console.log(t('agents.list.empty', {}, env));
    else for (const agent of result.agents) console.log(t('agents.list.row', { agent: agent.label, state: t(byState[agent.registration] || byState.unknown, {}, env), version: agent.version || t('agents.version.unknown', {}, env) }, env));
    return;
  }
  console.log(JSON.stringify(result));
}

/** `sidevoice agents [--json]` and `sidevoice agents connect|disconnect|dismiss <id> [--json]`. */
export function run(argv = [], env = process.env) {
  const json = argv.includes('--json');
  const args = argv.filter(value => value !== '--json');
  try {
    if (!args.length) return writeResult(listAgents(env, { rescan: true }), json, env);
    const [action, id, ...rest] = args;
    if (rest.length || !id || !['connect', 'disconnect', 'dismiss'].includes(action)) throw errorFor('agents.usage');
    const result = agentAction(action, id, env);
    if (json) return writeResult(result, true, env);
    const agent = result.agents.find(row => row.id === id);
    const key = action === 'dismiss' ? 'agents.action.dismiss' : `agents.action.${action}`;
    console.log(t(key, { agent: agent?.label || t(`harness.${id}`, {}, env) }, env));
    return;
  }
  catch (error) {
    const key = typeof error.key === 'string' ? error.key : 'agents.action-failed';
    const params = error.params && typeof error.params === 'object' ? error.params : {};
    const message = t(key, params, env);
    if (!json) { console.error(message); return 1; }
    console.log(JSON.stringify({ ok: false, error: { key, params, message } }));
    return 1;
  }
}

/** Resolve and record the login shell's PATH at install time, before a login service starts with its own minimal env. */
export function captureAgentEnvironment(env = process.env) {
  const base = loadStore(env);
  const loginPath = captureLoginPath(env);
  const binaries = {};
  for (const id of IDS) {
    const binary = resolveAgentBinary(id, env, { loginPath: loginPath || base.login_path, binaries: base.binaries });
    binaries[id] = binary || null;
  }
  return withStoreTransaction(env, latest => {
    if (loginPath && latest.login_path === base.login_path) latest.login_path = loginPath;
    for (const id of IDS) {
      if (latest.binaries[id] !== base.binaries[id]) continue;
      if (binaries[id]) latest.binaries[id] = binaries[id];
      else delete latest.binaries[id];
    }
  }).store;
}

export const AGENT_IDS = IDS;
