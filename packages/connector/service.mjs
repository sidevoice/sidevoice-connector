/** `sidevoice service install | uninstall | start | stop | restart | status [--json]` — the node service (§4.2):
 *  the login service that runs `connector --supervise`, which runs this machine's core. One owner for the app
 *  and for `npx` alike.
 *
 *  | manager | definition | start / stop / restart / uninstall |
 *  |---|---|---|
 *  | launchd (macOS) | `~/Library/LaunchAgents/dev.sidevoice.node.plist`: RunAtLoad, KeepAlive, ThrottleInterval 10, output → `node-service.log` | bootstrap + kickstart / bootout / kickstart -k / bootout + delete |
 *  | systemd (Linux, a user manager) | `~/.config/systemd/user/sidevoice-node.service`: Restart=on-failure, RestartSec=2, StartLimitIntervalSec=600, StartLimitBurst=5 | start / stop / restart (reset-failed first after a start limit) / disable --now, delete, daemon-reload |
 *  | none (containers, pods: no user manager) | — | a detached supervisor; nothing brings it back after a reboot |
 *
 *  The program is the selected installation's `command` from `install.json` (absolute paths) followed by
 *  `connector --supervise`: R1 `node …/cli.mjs`, R4 the single executable — only `command` changes.
 *
 *  A stop is a person's: `node-stopped.json` is written before the manager is asked, and while it is there the
 *  launcher starts nothing (`launcher.mjs`); only `service start` — or the supervisor starting at the next
 *  login — clears it. Linux keeps a user's services only while that user has a session; running them without
 *  one is `loginctl enable-linger`, a system setting the machine's owner enables (O4): it is printed, with the
 *  reason, and never run (`RUN_LINGER`). */
import { execFileSync, spawn } from 'node:child_process';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { accessSync, constants, existsSync, readFileSync, rmSync, unlinkSync } from 'node:fs';
import { coreAlive, readReady, socketPathOf, takeInstallLock } from './core.mjs';
import { localHealth } from './core-socket.mjs';
import { keyed, t } from './i18n.mjs';
import { connectorSocketOf, dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';

export const LABEL = 'dev.sidevoice.node';
export const UNIT = 'sidevoice-node.service';
/** O4: Sidevoice prints the linger command and never runs it. The one place to change that decision. */
export const RUN_LINGER = false;
const TEARDOWN_MS = Number(process.env.SIDEVOICE_TEARDOWN_MS || 15_000);
const START_WAIT_MS = Number(process.env.SIDEVOICE_SERVICE_START_WAIT_MS || 10_000);

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const uid = () => process.getuid?.() ?? 0;

/** Which manager this machine offers this user. `SIDEVOICE_SERVICE_MANAGER` names one (a test's stand-in; or
 *  `none` to keep the detached supervisor). Linux has one only when a user manager answers. */
export function managerKind(env = process.env) {
  if (env.SIDEVOICE_SERVICE_MANAGER) return env.SIDEVOICE_SERVICE_MANAGER;
  if (process.platform === 'darwin') return 'launchd';
  if (process.platform === 'linux') return manage(env, 'systemd', ['--user', 'show-environment']).ok ? 'systemd' : 'none';
  return 'none';
}

export function definitionPath(kind, env = process.env) {
  const home = env.HOME || os.homedir();
  if (kind === 'launchd') return path.join(home, 'Library', 'LaunchAgents', `${LABEL}.plist`);
  if (kind === 'systemd') return path.join(env.XDG_CONFIG_HOME || path.join(home, '.config'), 'systemd', 'user', UNIT);
  return null;
}

/** The service registered with this machine's manager, if one is: `{kind, file}` or null. Found by its
 *  definition alone — asking a manager is slow, and this is asked at every launch. */
export function installedService(env = process.env) {
  const kind = env.SIDEVOICE_SERVICE_MANAGER || (process.platform === 'darwin' ? 'launchd' : process.platform === 'linux' ? 'systemd' : 'none');
  const file = definitionPath(kind, env);
  return file && existsSync(file) ? { kind, file } : null;
}

/** Run the manager's own command; never throws. Every call has an absolute deadline. */
function manage(env, kind, args) {
  const bin = kind === 'launchd' ? env.SIDEVOICE_LAUNCHCTL || '/bin/launchctl'
    : kind === 'systemd' ? env.SIDEVOICE_SYSTEMCTL || 'systemctl' : env.SIDEVOICE_LOGINCTL || 'loginctl';
  try {
    const output = execFileSync(bin, args, { encoding: 'utf8', timeout: 10_000, env, stdio: ['ignore', 'pipe', 'pipe'] });
    return { ok: true, output, code: 0 };
  } catch (error) {
    return { ok: false, output: `${error.stdout || ''}${error.stderr || ''}` || error.message, code: error.status ?? error.code ?? null };
  }
}

/** What the supervisor is started with besides its arguments: the `SIDEVOICE_*` settings this install was made
 *  with (a data dir, a core named by hand…) — never a credential of a core somebody else runs — and which
 *  manager runs it, which `node.status` reports. */
export function serviceEnvironment(kind, env = process.env) {
  const kept = Object.entries(env).filter(([name]) => name.startsWith('SIDEVOICE_')
    && !['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_SERVICE'].includes(name));
  return Object.fromEntries([...kept.sort(([a], [b]) => a.localeCompare(b)), ['SIDEVOICE_SERVICE', kind]]);
}

const xml = text => String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** The LaunchAgent. KeepAlive brings the supervisor back whenever it exits (only `bootout` stops it), at most
 *  every 10 s; its output goes to the log it also writes itself. */
export function plistText({ program, log, environment = {} }) {
  const strings = items => items.map(item => `    <string>${xml(item)}</string>`).join('\n');
  const variables = Object.entries(environment).map(([name, value]) => `    <key>${xml(name)}</key>\n    <string>${xml(value)}</string>`).join('\n');
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
${strings(program)}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
${variables}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>${xml(log)}</string>
  <key>StandardErrorPath</key>
  <string>${xml(log)}</string>
</dict>
</plist>
`;
}

/** One systemd word: quoted, its quotes and backslashes escaped, and `%`/`$` kept from being specifiers. */
const systemdWord = text => `"${String(text).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/%/g, '%%').replace(/\$/g, '$$$$')}"`;

/** The user unit. Restarted on failure 2 s later, at most 5 starts in 10 minutes (then `start-limit`, which
 *  `service restart` clears); started with the user's manager (`default.target`). */
export function unitText({ program, log, environment = {} }) {
  const variables = Object.entries(environment).map(([name, value]) => `Environment=${systemdWord(`${name}=${value}`)}`).join('\n');
  const file = String(log).replace(/%/g, '%%');
  return `[Unit]
Description=Sidevoice node service (the connector supervising this machine's core)
StartLimitIntervalSec=600
StartLimitBurst=5

[Service]
Type=simple
ExecStart=${program.map(systemdWord).join(' ')}
${variables}
Restart=on-failure
RestartSec=2
KillMode=control-group
TimeoutStopSec=25
StandardOutput=append:${file}
StandardError=append:${file}

[Install]
WantedBy=default.target
`;
}

/** The program the service runs: the selected installation's command, then `connector --supervise`. */
export function serviceProgram(record) {
  if (!Array.isArray(record?.command) || !record.command.length) throw keyed('service.no-installation');
  return [...record.command, 'connector', '--supervise'];
}

/** Write the definition for this installation (also what an install transaction does to re-point it). */
export function writeDefinition(kind, record, env = process.env) {
  const file = definitionPath(kind, env);
  if (!file) return null;
  const dataDir = dataDirOf(env);
  const spec = { program: serviceProgram(record), log: nodeFiles(dataDir).serviceLog, environment: serviceEnvironment(kind, env) };
  writePrivate(file, kind === 'launchd' ? plistText(spec) : unitText(spec));
  return file;
}

const domain = () => `gui/${uid()}`;
const target = () => `${domain()}/${LABEL}`;

function loaded(kind, env) {
  if (kind === 'launchd') return manage(env, kind, ['print', target()]).ok;
  if (kind === 'systemd') return !/LoadState=not-found/.test(manage(env, kind, ['--user', 'show', '-p', 'LoadState', UNIT]).output);
  return false;
}

/** What the manager says about the service: `{loaded, active, result, exit, reason}`. */
export function managerState(kind, env = process.env) {
  if (kind === 'launchd') {
    const printed = manage(env, kind, ['print', target()]);
    if (!printed.ok) return { loaded: false, active: false, reason: 'not-loaded' };
    const field = name => (printed.output.match(new RegExp(`^\\s*${name} = (.*)$`, 'm')) || [])[1]?.trim() ?? null;
    const exit = field('last exit code');
    return { loaded: true, active: field('state') === 'running', exit, pid: Number(field('pid')) || null, reason: null };
  }
  if (kind === 'systemd') {
    const shown = manage(env, kind, ['--user', 'show', '-p', 'ActiveState,Result,ExecMainStatus,LoadState', UNIT]).output;
    const field = name => (shown.match(new RegExp(`^${name}=(.*)$`, 'm')) || [])[1]?.trim() ?? null;
    const load = field('LoadState');
    return { loaded: !!load && load !== 'not-found', active: ['active', 'activating', 'reloading'].includes(field('ActiveState')),
      result: field('Result'), exit: field('ExecMainStatus'), reason: load === 'not-found' ? 'not-loaded' : field('Result') === 'start-limit-hit' ? 'start-limit' : null };
  }
  return { loaded: false, active: false, reason: null };
}

/** Why a registered service has no supervisor answering, as one of `executable-missing | permission-denied |
 *  start-limit | not-loaded`: the manager's word where it has one, else the program itself examined. */
export function serviceFailure(kind, env = process.env) {
  const state = managerState(kind, env);
  if (state.reason) return state.reason;
  const record = readJson(nodeFiles(dataDirOf(env)).install);
  for (const file of (record?.command || []).filter(item => path.isAbsolute(item))) {
    if (!existsSync(file)) return 'executable-missing';
    try { accessSync(file, file === record.command[0] ? constants.X_OK : constants.R_OK); } catch { return 'permission-denied'; }
  }
  return 'not-loaded';
}

/** One request to the connector on its socket, or null when none answers within `timeout`. */
export function askConnector(method, params = {}, { env = process.env, timeout = 1500 } = {}) {
  return new Promise(resolve => {
    const socket = net.createConnection(connectorSocketOf(env));
    let buffer = '';
    const done = value => { clearTimeout(timer); socket.destroy(); resolve(value); };
    const timer = setTimeout(() => done(null), timeout);
    socket.on('error', () => done(null));
    socket.on('connect', () => socket.write(JSON.stringify({ id: 1, method, params }) + '\n'));
    socket.on('data', chunk => {
      buffer += chunk; const index = buffer.indexOf('\n'); if (index < 0) return;
      try { const reply = JSON.parse(buffer.slice(0, index)); done(reply.ok ? reply.result : { error: reply.error }); } catch { done(null); }
    });
  });
}

/** `service status --json` (SEAMS §3): the supervisor's own `node.status` when one answers, else derived from
 *  the files and the manager without starting anything.
 *
 *  A plain connector answering (`supervisor: false`) speaks only for the core it started on demand: what the
 *  node service is — `not-installed`, `stopped-by-person`… — comes from the files and the manager as when
 *  nothing answers, with that core's `core` and `calls`. A supervisor's `stopped` is only ever transient (before
 *  its first start, and while it shuts down — its socket stops taking connections first). */
export async function status(env = process.env) {
  const answered = await askConnector('node.status', {}, { env });
  if (answered && !answered.error && answered.state && answered.supervisor) return { ...answered, ok: true };
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const kind = managerKind(env);
  const service = installedService(env);
  const installed = existsSync(files.install);
  const ready = readReady(dataDir);
  const health = ready && !answered?.core ? await localHealth(ready.socket, 1500) : null;
  const core = answered?.core ?? (health?.status === 200 ? { pid: health.body.pid, version: health.body.version, api: health.body.api, launch_id: health.body.launch_id } : null);
  const calls = answered?.core ? answered.calls ?? 0 : health?.status === 200 ? health.body.calls ?? 0 : 0;
  const base = { ok: true, service: kind, installed, failure: null, calls, core };
  if (!installed && !service) return { ...base, state: 'absent' };
  if (existsSync(files.stopped)) return { ...base, state: 'stopped-by-person' };
  if (!service) return { ...base, state: 'not-installed' };
  return { ...base, state: 'service-failed', failure: { key: serviceFailure(kind, env) } };
}

/** The pids that must be gone for the node to be down: the connector holding the socket, and the core. */
function nodePids(env) {
  const dataDir = dataDirOf(env);
  let connector = null; try { connector = Number(readFileSync(connectorSocketOf(env) + '.lock', 'utf8')) || null; } catch {}
  const core = readReady(dataDir)?.pid || null;
  return { connector, core };
}
const alive = pid => { if (!pid) return false; try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; } };

/** Wait for the supervisor and its core to be gone, up to `TEARDOWN_MS`; then kill what is left. Their sockets
 *  go with them. */
async function awaitDown(env, pids = nodePids(env)) {
  const deadline = Date.now() + TEARDOWN_MS;
  const left = () => [pids.connector, pids.core].filter(pid => pid && (pid === pids.core ? coreAlive(pid) : alive(pid)));
  while (left().length && Date.now() < deadline) await wait(100);
  const killed = left();
  for (const pid of killed) { try { process.kill(pid, 'SIGKILL'); } catch {} }
  const after = Date.now() + 5000;
  while (left().length && Date.now() < after) await wait(50);
  for (const file of [connectorSocketOf(env), connectorSocketOf(env) + '.lock', socketPathOf(dataDirOf(env))]) {
    if (!left().length) { try { unlinkSync(file); } catch {} }
  }
  return { killed, left: left() };
}

/** Wait for a connector to answer on the socket, up to `START_WAIT_MS`. */
async function awaitUp(env, timeout = START_WAIT_MS) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const answered = await askConnector('node.status', {}, { env, timeout: 1000 });
    if (answered?.state) return answered;
    await wait(200);
  }
  return null;
}

/** Ask the manager to start the registered service (or, with none, a detached supervisor). Starts nothing
 *  already running. Used by `service start` and by the launcher, which never spawns where a service exists. */
export function managerStart(env = process.env, { load = true } = {}) {
  const service = installedService(env);
  const kind = service?.kind ?? managerKind(env);
  if (kind === 'launchd' && service) {
    if (!loaded(kind, env)) {
      if (!load) return { ok: false, key: 'service.not-loaded' };
      const booted = manage(env, kind, ['bootstrap', domain(), service.file]);
      if (!booted.ok && !loaded(kind, env)) return { ok: false, key: 'service.not-loaded', detail: booted.output.trim() };
    }
    const kicked = manage(env, kind, ['kickstart', target()]);
    return kicked.ok ? { ok: true } : { ok: false, key: 'service.not-loaded', detail: kicked.output.trim() };
  }
  if (kind === 'systemd' && service) {
    let started = manage(env, kind, ['--user', 'start', UNIT]);
    if (!started.ok && managerState(kind, env).reason === 'start-limit') {
      manage(env, kind, ['--user', 'reset-failed', UNIT]);
      started = manage(env, kind, ['--user', 'start', UNIT]);
    }
    if (started.ok) return { ok: true };
    const reason = serviceFailure(kind, env);
    return { ok: false, key: reason === 'not-loaded' ? 'service.not-loaded' : `service.${reason}`, detail: started.output.trim() };
  }
  return { ok: false, key: 'service.not-loaded' };
}

/** Start a detached supervisor: the node service where there is no manager to run it. */
function startDetached(env) {
  const record = readJson(nodeFiles(dataDirOf(env)).install);
  const program = serviceProgram(record);
  const child = spawn(program[0], program.slice(1), { detached: true, stdio: 'ignore', env: { ...env, SIDEVOICE_SERVICE: 'none' } });
  child.on('error', () => {});
  child.unref();
}

/** `service install`: the definition written from the selected installation, the manager told, the service
 *  started; with no manager, a detached supervisor. Linux says what linger would add. */
export async function install(env = process.env) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const kind = managerKind(env);
  const release = await takeInstallLock(dataDir);
  try {
    const record = readJson(files.install);
    if (!record?.command) throw keyed('service.no-installation');
    if (kind !== 'none') {
      const file = writeDefinition(kind, record, env);
      if (kind === 'systemd') { manage(env, kind, ['--user', 'daemon-reload']); manage(env, kind, ['--user', 'enable', UNIT]); }
      // A job loaded from an earlier definition is replaced by this one.
      if (kind === 'launchd' && loaded(kind, env)) manage(env, kind, ['bootout', target()]);
      void file;
    }
    if (record.service !== kind) writePrivate(files.install, { ...record, service: kind });
  } finally { release(); }
  try { rmSync(files.stopped, { force: true }); } catch {}
  const started = await start(env, { clear: false });
  return { ...started, ...(kind === 'systemd' ? { linger: linger(env) } : {}) };
}

/** Linux: whether this user's services run without a session, and how to make them (O4). */
export function linger(env = process.env) {
  const user = env.USER || os.userInfo().username;
  const enabled = /Linger=yes/.test(manage(env, 'loginctl', ['show-user', user, '-p', 'Linger']).output);
  const command = `loginctl enable-linger ${user}`;
  if (!enabled && RUN_LINGER) manage(env, 'loginctl', ['enable-linger', user]);
  return { enabled, command, reason: t('service.linger-reason') };
}

/** `service start`: a stop no longer holds; the manager starts the service, or a detached supervisor starts. */
export async function start(env = process.env, { clear = true } = {}) {
  const files = nodeFiles(dataDirOf(env));
  if (clear) { try { rmSync(files.stopped, { force: true }); } catch {} }
  const kind = installedService(env)?.kind ?? 'none';
  const running = await askConnector('node.status', {}, { env });
  if (running?.state && running.supervisor && running.service === kind) return { ok: true, state: running.state, service: kind };
  if (kind === 'none') {
    if (!existsSync(files.install)) throw keyed('service.no-installation');
    // A plain connector already serving is taken over by the supervisor (handover); none is started twice.
    startDetached(env);
  } else {
    const started = managerStart(env);
    if (!started.ok) throw keyed(started.key, { detail: started.detail || '' });
  }
  const up = await awaitUp(env);
  if (!up) throw keyed('service.not-loaded', { detail: '' });
  return { ok: true, state: up.state, service: kind };
}

/** `service stop`: the person's stop — written first, so nothing starts the node again meanwhile — then the
 *  manager stops it (launchd: bootout, so KeepAlive does not bring it back), and both processes are awaited. */
export async function stop(env = process.env) {
  const files = nodeFiles(dataDirOf(env));
  writePrivate(files.stopped, { at: new Date().toISOString() });
  const kind = installedService(env)?.kind ?? 'none';
  const pids = nodePids(env);
  if (kind === 'launchd') manage(env, kind, ['bootout', target()]);
  else if (kind === 'systemd') manage(env, kind, ['--user', 'stop', UNIT]);
  else if (pids.connector) { try { process.kill(pids.connector, 'SIGTERM'); } catch {} }
  // A plain connector's core is detached and outlives it: it is asked to leave as well.
  if (kind === 'none' && pids.core) { try { process.kill(pids.core, 'SIGTERM'); } catch {} }
  const down = await awaitDown(env, pids);
  return { ok: !down.left.length, state: 'stopped-by-person', service: kind, ...(down.killed.length ? { note: t('service.killed', { pids: down.killed.join(', ') }) } : {}) };
}

/** `service restart` — the person's «Reintentar». A supervisor that answers restarts the core itself
 *  (`node.restart`, which closes the budget window); one that does not is restarted by its manager, with a
 *  note that this restart is the person's, so the new supervisor closes the window too. */
export async function restart(env = process.env) {
  const files = nodeFiles(dataDirOf(env));
  const kind = installedService(env)?.kind ?? 'none';
  if (existsSync(files.stopped)) return start(env);
  const answered = await askConnector('node.restart', {}, { env, timeout: 90_000 });
  if (answered?.state && answered.supervisor && answered.service === kind) return { ok: true, state: answered.state, service: kind };
  writePrivate(files.restart, { at: new Date().toISOString() });
  if (kind === 'launchd') {
    const kicked = manage(env, kind, ['kickstart', '-k', target()]);
    if (!kicked.ok) return start(env);
  } else if (kind === 'systemd') {
    if (managerState(kind, env).reason === 'start-limit') manage(env, kind, ['--user', 'reset-failed', UNIT]);
    const restarted = manage(env, kind, ['--user', 'restart', UNIT]);
    if (!restarted.ok) throw keyed(`service.${serviceFailure(kind, env)}`, { detail: restarted.output.trim() });
  } else return start(env);
  const up = await awaitUp(env);
  if (!up) throw keyed('service.not-loaded', { detail: '' });
  return { ok: true, state: up.state, service: kind };
}

/** The service started again from its definition as it is now — what an install transaction does once it has
 *  rewritten it: launchd reads a plist only when it is bootstrapped, so the job is booted out and in again;
 *  systemd reloads and restarts; a detached supervisor is replaced. The supervisor and its core are new. */
export async function reload(env = process.env) {
  const kind = installedService(env)?.kind ?? 'none';
  const pids = nodePids(env);
  if (kind === 'launchd') {
    if (loaded(kind, env)) manage(env, kind, ['bootout', target()]);
    await awaitDown(env, pids);
    const booted = manage(env, kind, ['bootstrap', domain(), definitionPath(kind, env)]);
    if (!booted.ok) throw keyed('service.not-loaded', { detail: booted.output.trim() });
  } else if (kind === 'systemd') {
    manage(env, kind, ['--user', 'daemon-reload']);
    if (managerState(kind, env).reason === 'start-limit') manage(env, kind, ['--user', 'reset-failed', UNIT]);
    const restarted = manage(env, kind, ['--user', 'restart', UNIT]);
    if (!restarted.ok) throw keyed(`service.${serviceFailure(kind, env)}`, { detail: restarted.output.trim() });
  } else {
    if (pids.connector) { try { process.kill(pids.connector, 'SIGTERM'); } catch {} }
    await awaitDown(env, { connector: pids.connector, core: null });
    startDetached(env);
  }
  const up = await awaitUp(env);
  if (!up) throw keyed('service.not-loaded', { detail: '' });
  return { ok: true, state: up.state, service: kind };
}

/** `service uninstall`: stopped, both processes gone (killed after 15 s), the definition deleted, the manager
 *  reloaded. Idempotent. An unload the manager refuses stops here, with nothing deleted. */
export async function uninstall(env = process.env, { keepStopped = false } = {}) {
  const files = nodeFiles(dataDirOf(env));
  const kind = managerKind(env);
  const file = definitionPath(kind, env);
  const pids = nodePids(env);
  // Nothing may start the node while it is being taken apart.
  writePrivate(files.stopped, { at: new Date().toISOString() });
  if (kind === 'launchd' && loaded(kind, env)) {
    const out = manage(env, kind, ['bootout', target()]);
    if (!out.ok && loaded(kind, env)) throw keyed('service.unload-failed', { detail: out.output.trim() });
  } else if (kind === 'systemd' && file && existsSync(file)) {
    const out = manage(env, kind, ['--user', 'disable', '--now', UNIT]);
    if (!out.ok && managerState(kind, env).active) throw keyed('service.unload-failed', { detail: out.output.trim() });
  } else if (pids.connector) { try { process.kill(pids.connector, 'SIGTERM'); } catch {} }
  if (pids.core) { try { process.kill(pids.core, 'SIGTERM'); } catch {} }
  const down = await awaitDown(env, pids);
  if (down.left.length) throw keyed('service.unload-failed', { detail: `pid ${down.left.join(', ')} still running` });
  if (file) { try { rmSync(file, { force: true }); } catch {} }
  if (kind === 'systemd') { manage(env, kind, ['--user', 'daemon-reload']); manage(env, kind, ['--user', 'reset-failed', UNIT]); }
  if (!keepStopped) { try { rmSync(files.stopped, { force: true }); } catch {} }
  const record = readJson(files.install);
  if (record && record.service !== 'none') { try { writePrivate(files.install, { ...record, service: 'none' }); } catch {} }
  return { ok: true, state: existsSync(files.install) ? 'not-installed' : 'absent', service: kind,
    ...(down.killed.length ? { note: t('service.killed', { pids: down.killed.join(', ') }) } : {}) };
}

/** `sidevoice service <install|uninstall|start|stop|restart|status> [--json]`. */
export async function run(argv = [], env = process.env) {
  const [action] = argv.filter(item => !item.startsWith('-'));
  const json = argv.includes('--json');
  const actions = { install, uninstall, start, stop, restart, status };
  if (!actions[action]) { console.error(t('service.usage')); return 2; }
  let result;
  try { result = await actions[action](env); }
  catch (error) { result = { ok: false, error: { key: error.key || 'service.failed', message: error.message } }; }
  if (json) console.log(JSON.stringify(result));
  else if (!result.ok) console.error(result.error?.message || t('service.failed'));
  else {
    console.log(t(`service.state.${result.state}`, { service: result.service }) + (result.failure?.key ? ` (${t('service.reason.' + result.failure.key)})` : ''));
    if (result.note) console.log(result.note);
    if (result.linger && !result.linger.enabled) console.log(`\n${result.linger.reason}\n    ${result.linger.command}`);
  }
  return result.ok ? 0 : 1;
}
