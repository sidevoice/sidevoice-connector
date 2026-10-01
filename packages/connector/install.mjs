/** `sidevoice install` — put this version of Sidevoice on this machine and in front of the harnesses on it.
 *
 *  It is one transaction (`install-txn.mjs`, §4.3): the copy and the core staged, checked, committed, the node
 *  restarted on them and rolled back if it does not come up — and a no-op when what is installed is the same or
 *  newer. Then the node service (`service.mjs`) — by default; `--no-service` leaves the core to start on demand —
 *  and the harnesses: each one found, or `--harness`, or none at all with `--no-agents` (the app's install: it
 *  connects agents only when the person picks them). It is safe to run twice: run it again after an upgrade and
 *  every harness of ours points at the new version.
 *
 *  It pairs with nothing. Pairing is a person's act — the room shows a one-time code to whoever is in it, and a
 *  conversation asks for it the first time it joins — so the installer only reports whether this machine is
 *  paired, and with which room.
 *
 *  What it does not do is decide for the person: it never edits a machine-wide Codex configuration it
 *  does not own, it never relaxes Claude Code's inbound safeguard, and it never enables Linux lingering —
 *  those it prints, with the reason. Cursor's `mcp.json` is a map keyed by server name: only the `sidevoice`
 *  key is written, and only when it is absent or is one this package wrote. */
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { harnessesPresent } from './identity.mjs';
import { pairedRoom } from './pair.mjs';
import { CORE_VERSION, NO_UV, findUv, readReady } from './core.mjs';
import { keyed, t } from './i18n.mjs';
import { remove as removeSkill, skillsDir, status as skillStatus } from './skill.mjs';
import { transact } from './install-txn.mjs';
import { HARNESS_REGISTRATIONS, candidate, claudeState, codexInstructions, copiesDir, cursorHasOurs, cursorMcpFile, cursorState, fromSource,
  registerWithClaude, unregisterFromClaude, unregisterFromCursor } from './registrations.mjs';
import { askConnector, installedService, linger, uninstall as uninstallService } from './service.mjs';
import { dataDirOf } from './node-files.mjs';
import { readLock } from './lockfile.mjs';

export { claudeRegistration, codexInstructions, copiesDir, cursorHasOurs, cursorMcpFile, registerWithCursor, serverCommand,
  unregisterFromCursor } from './registrations.mjs';
export { compareVersions, decide, pruneInstallations, recover } from './install-txn.mjs';

/** The connector that holds this machine's socket, if any, and which version it is: a façade uses whatever
 *  connector is running, so one left over from before an upgrade serves every new session with old code. */
export function runningConnector(env = process.env) {
  const dataDir = env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
  const socketPath = env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
  const lock = readLock(socketPath + '.lock');
  if (!lock || lock.unreadable) return null;
  const pid = lock.pid;
  return new Promise(resolve => {
    const socket = net.createConnection(socketPath);
    const done = value => { clearTimeout(timer); socket.destroy(); resolve(value); };
    const timer = setTimeout(() => done(null), 1500);
    let buffer = '';
    socket.on('error', () => done(null));
    socket.on('connect', () => socket.write(JSON.stringify({ id: 1, method: 'status', params: {} }) + '\n'));
    socket.on('data', chunk => {
      buffer += chunk; const index = buffer.indexOf('\n'); if (index < 0) return;
      try { const reply = JSON.parse(buffer.slice(0, index)); done({ pid, version: reply.result?.version || null, bindings: reply.result?.bindings?.length ?? null }); }
      catch { done({ pid, version: null, bindings: null }); }
    });
  });
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

const USAGE = 'usage: sidevoice install [--harness claude|codex|cursor] [--no-agents] [--no-service] [--no-core] [--apply-now] [--json]';

export async function install(argv = [], env = process.env, { progress = () => {} } = {}) {
  const stray = argv.find(item => !item.startsWith('-') && argv[argv.indexOf(item) - 1] !== '--harness');
  if (stray) throw keyed('install.usage', { usage: USAGE });
  const wanted = flag(argv, '--harness');
  // `--no-agents`: nothing registered with any harness — the person picks agents later (W3, `sidevoice agents`).
  const harnesses = argv.includes('--no-agents') ? [] : wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];

  // A core somebody else runs (`SIDEVOICE_URL`…) is not this installer's; `--no-core` leaves it for later.
  const externalCore = env.SIDEVOICE_URL && env.SIDEVOICE_CONNECTOR_ID && env.SIDEVOICE_CONNECTOR_TOKEN;
  const core = !argv.includes('--no-core') && !externalCore;
  if (core && !env.SIDEVOICE_CORE_BIN && !findUv(env)) throw new Error(NO_UV);
  // The node service by default (F3): the manager's where there is one, a detached supervisor where not.
  const service = core && !argv.includes('--no-service');

  // One transaction: the copy, the core, the service and every registration — the ones the person asked for now
  // included — committed together, and the result running before anything is said to be installed.
  const consent = harnesses.filter(name => name in HARNESS_REGISTRATIONS);
  const result = await transact(env, { core, applyNow: argv.includes('--apply-now'), service, consent, progress });
  const record = result.record;
  done.push(t('install.version', { version: candidate(env).connector }));
  if (result.action === 'rollback') {
    throw keyed('install.rollback', { to: candidate(env).id, from: result.from?.id ?? t('install.nothing'), cause: result.failure?.key ?? '?' }, { failure: result.failure, result });
  }
  done.push(...result.notes);
  if (result.action === 'noop') done.push(t('install.noop', { id: record.id, channel: record.channel }));
  else if (record.copy) done.push(t(result.from?.copy && result.from.copy !== record.copy ? 'install.copied-keeping' : 'install.copied', { copy: record.copy, previous: result.from?.id }));
  if (!core) done.push(externalCore ? t('install.external-core', { url: env.SIDEVOICE_URL }) : t('install.no-core'));

  let serviceStatus = null;
  if (service) {
    serviceStatus = await askConnector('node.status', {}, { env });
    const kind = installedService(env)?.kind ?? 'none';
    done.push(t(kind === 'none' ? 'install.service-detached' : 'install.service', { state: serviceStatus?.state ?? '?', service: kind }));
    if (kind === 'systemd') { const lingering = linger(env); if (!lingering.enabled) next.push(`${lingering.reason}\n    ${lingering.command}`); serviceStatus = { ...serviceStatus, linger: lingering }; }
  }
  const ready = core ? readReady(dataDirOf(env)) : null;
  if (ready) done.push(t('install.core-answering', { version: CORE_VERSION, url: ready.url }));

  if (harnesses.includes('claude')) {
    registerWithClaude(done, env, record);
    if (claudeState(env).state === 'ours') done.push(t('install.claude-registered'));
    // The join shortcut is a prompt the server offers; a skill copy from an earlier version is taken away.
    if (skillStatus(skillsDir([], env)).state === 'installed') done.push(t('install.skill-removed', { target: removeSkill(skillsDir([], env)).target }));
  }
  if (harnesses.includes('cursor') && cursorState(env).state === 'ours') done.push(t('install.cursor-registered', { file: cursorMcpFile(env) }));

  const paired = pairedRoom(env);
  done.push(paired ? `This machine is paired with ${paired.origin} (connector ${paired.connector_id}).`
                   : 'This machine is not paired with any room yet.');
  if (!paired && core) next.push(t('install.route'));
  const running = await runningConnector(env);
  if (running && running.version !== record.connector && !service) {
    next.push(`A connector from ${running.version ? 'version ' + running.version : 'an older version'} is still running (pid ${running.pid}) and every conversation on this machine uses it. ` +
              `It exits by itself 15 s after the last conversation leaves it; to switch now: kill ${running.pid}, then join again from each conversation.`);
  }

  if (harnesses.includes('claude')) {
    next.push('In a conversation, ask to join the voice room (or run /mcp__sidevoice__voice-room).' +
              (paired ? '' : ' With no room paired it joins this machine only: the Sidevoice app on this computer reaches it once you ask the conversation to pair a device. For a room, give the conversation its address and the code it shows under "Emparejar máquina".'));
    next.push('Sessions already open need a restart before they see the server.');
    const warning = inboundWarning(env);
    if (warning) next.push(warning);
  }
  if (harnesses.includes('codex')) next.push(codexInstructions(env, record));
  if (harnesses.includes('cursor')) next.push(cursorNotes());
  if (!harnesses.length && !argv.includes('--no-agents')) next.push('No harness found on this machine. Pass --harness claude, --harness codex or --harness cursor.');
  return { done, next, result, service: serviceStatus };
}

/** `sidevoice uninstall`: the reverse of install, for this machine, in the order that leaves nothing pointing at
 *  what is gone (§4.2 teardown): the node service first — stopped, its supervisor and core gone, its definition
 *  deleted — then our harness registrations, then the copies, then the data. If the service manager will not
 *  unload the service, nothing it still points at is deleted, and it says so. The room keeps this machine's
 *  pairing until it is revoked under "Máquinas" on the room's page — said, with where. Codex's machine-wide
 *  file is, as always, printed and not touched. */
export async function uninstall(argv = [], env = process.env) {
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];
  try {
    const service = await uninstallService(env, { keepStopped: true });
    done.push(t('uninstall.service-removed', { service: service.service }));
    if (service.note) done.push(service.note);
  } catch (error) {
    throw Object.assign(new Error(`${error.message} ${t('uninstall.stopped', { data: dataDirOf(env) })}`), { key: error.key });
  }
  if (harnesses.includes('claude')) {
    unregisterFromClaude(done, next, env);
    if (skillStatus(skillsDir([], env)).state === 'installed') done.push(`Removed the voice-room skill copy at ${removeSkill(skillsDir([], env)).target}.`);
  }
  if (harnesses.includes('cursor')) unregisterFromCursor(done, next, env);
  if (!fromSource(env) && existsSync(copiesDir(env))) {
    rmSync(copiesDir(env), { recursive: true, force: true }); done.push(`Removed the installed copies under ${copiesDir(env)}.`);
    if (!harnesses.includes('cursor') && cursorHasOurs(env)) next.push(`Cursor still lists the sidevoice MCP server in ${cursorMcpFile(env)}, and it now points at nothing: run  sidevoice uninstall --harness cursor , or install again.`);
  }
  const dataDir = dataDirOf(env);
  const paired = pairedRoom(env);
  if (existsSync(dataDir)) {
    rmSync(dataDir, { recursive: true, force: true });
    done.push(`Removed ${dataDir} (credential, socket, outbox, log, the core and its environment).`);
    if (paired) next.push(`The room at ${paired.origin} still lists this machine as paired (connector ${paired.connector_id}) until you revoke it under "Máquinas" on the room's page.`);
  }
  if (harnesses.includes('codex')) next.push(`Remove the [mcp_servers.sidevoice] table from ${env.CODEX_HOME || path.join(os.homedir(), '.codex')}/config.toml — it is machine-wide and this package does not rewrite it.`);
  next.push('Sessions already open keep their MCP server until they end.');
  return { done, next };
}

/** `sidevoice uninstall`. */
export async function runUninstall(argv = [], env = process.env) {
  try {
    const { done, next } = await uninstall(argv, env);
    for (const line of done) console.log('· ' + line);
    if (next.length) { console.log('\nLeft for you:'); for (const line of next) console.log('\n' + line); }
    return 0;
  } catch (error) { console.error(error.message); return 1; }
}

/** `sidevoice install [--json]`: with `--json`, one object for the app — progress goes to stderr then. */
export async function runInstall(argv = [], env = process.env) {
  const json = argv.includes('--json');
  try {
    const { done, next, result, service } = await install(argv.filter(item => item !== '--json'), env, { progress: line => (json ? console.error(line) : console.log(line)) });
    if (json) {
      console.log(JSON.stringify({ ok: true, action: result.action, installed: result.record.id, connector: result.record.connector, core: result.record.core,
        channel: result.record.channel, command: result.record.command, service: service?.service ?? 'none', state: service?.state ?? null,
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
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key || 'install.failed', message: error.message }, ...(error.failure ? { failure: error.failure } : {}) }));
    else console.error(error.message);
    return 1;
  }
}
