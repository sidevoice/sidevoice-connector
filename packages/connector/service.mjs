/** `sidevoice service install | uninstall | start | stop | restart | status [--json]` — Sidevoice at login (§2.1–§2.3,
 *  §2.8–§2.9): two jobs of this user's service manager, and nothing of ours supervising either.
 *
 *  | job | launchd (`~/Library/LaunchAgents/<label>.plist`) | systemd (`~/.config/systemd/user/<unit>`) |
 *  |---|---|---|
 *  | core `dev.sidevoice.core` / `sidevoice-core.service` | RunAtLoad; KeepAlive {SuccessfulExit false, Crashed true}; ThrottleInterval 10; output → `core.stderr.log` | Restart=on-failure, RestartSec=10, at most 5 starts in 10 min |
 *  | connector `dev.sidevoice.connector` / `sidevoice-connector.service` | RunAtLoad; KeepAlive; ThrottleInterval 10; output → `connector.log` | Restart=always, RestartSec=2, the same start limit |
 *
 *  The core runs `R/current/core/bin/sidevoice-core --data-dir C … --idle-exit 0`; the connector runs `install.json`'s
 *  `command` + `connector --service` (R1 `node R/current/dist/cli.mjs`; R4 only `command` changes). Both name paths
 *  through `R/current` (`release.mjs`), so a definition is rewritten only when its text would change. Neither job
 *  starts, signals or adopts the other: the connector links to the core when it answers, and the core's exit status
 *  tells the manager whether to restart it (SEAMS §2: 0 after a failed start — not again; 75 while another core holds
 *  its directory — later; a crash after ready — again). One instance of each: the manager's, plus the core's `flock`
 *  and the connector's lock (`lockfile.mjs`).
 *
 *  A stop is a person's: `node-stopped.json` is written before the manager is asked, and while it is there the
 *  launcher starts nothing (`launcher.mjs`); `service start` clears it, and so does the connector job starting at the
 *  next login. Every command that changes something holds the install lock. Linux keeps a user's services only while
 *  that user has a session; running them without one is `loginctl enable-linger`, a system setting the machine's
 *  owner enables (O4): printed, with the reason, and never run (`RUN_LINGER`).
 *
 *  What the node is doing is never stored: `deriveStatus()` reads it fresh from the manager, the core's failure
 *  report and the core's health — the same answer for `service status --json` and for any connector's `node.status`. */
import { execFileSync } from 'node:child_process';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { accessSync, constants, existsSync, readFileSync, rmSync } from 'node:fs';
import { API_RANGE, LINK_RANGE, coreArgs, coreProcesses, coreRunning, describeFailure, failurePath, logTail, readFailure, readReady,
  roomCredentialPath, socketPathOf, takeInstallLock, terminateCore } from './core.mjs';
import { localHealth } from './core-socket.mjs';
import { readLock, tryLock } from './lockfile.mjs';
import { isProcess, processAge, signalVerified } from './proc.mjs';
import { coreProgram, selection } from './release.mjs';
import { keyed, t } from './i18n.mjs';
import { connectorLockOf, connectorSocketOf, dataDirOf, nodeFiles, readJson, writePrivate } from './node-files.mjs';

export const JOBS = {
  core: { label: 'dev.sidevoice.core', unit: 'sidevoice-core.service' },
  connector: { label: 'dev.sidevoice.connector', unit: 'sidevoice-connector.service' },
};
/** systemd's start limit, the same for both jobs; launchd has none. */
export const START_LIMIT = 5;
/** O4: Sidevoice prints the linger command and never runs it. The one place to change that decision. */
export const RUN_LINGER = false;
/** A core job running this long without being ready is not starting any more (§2.8 row 6). */
const STARTING_S = 60;
const TEARDOWN_MS = () => Number(process.env.SIDEVOICE_TEARDOWN_MS || 15_000);
const SETTLE_MS = () => Number(process.env.SIDEVOICE_SERVICE_START_WAIT_MS || 10_000);

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const uid = () => process.getuid?.() ?? 0;
const ORDER = ['core', 'connector'];

/** Which manager this machine offers this user. `SIDEVOICE_SERVICE_MANAGER` names one (a test's stand-in, or `none`).
 *  Linux has one only when a user manager answers. */
export function managerKind(env = process.env) {
  if (env.SIDEVOICE_SERVICE_MANAGER) return env.SIDEVOICE_SERVICE_MANAGER;
  if (process.platform === 'darwin') return 'launchd';
  if (process.platform === 'linux') return manage(env, 'systemd', ['--user', 'show-environment']).ok ? 'systemd' : 'none';
  return 'none';
}
const platformKind = env => env.SIDEVOICE_SERVICE_MANAGER || (process.platform === 'darwin' ? 'launchd' : process.platform === 'linux' ? 'systemd' : 'none');

export function definitionPath(kind, job, env = process.env) {
  const home = env.HOME || os.homedir();
  if (kind === 'launchd') return path.join(home, 'Library', 'LaunchAgents', `${JOBS[job].label}.plist`);
  if (kind === 'systemd') return path.join(env.XDG_CONFIG_HOME || path.join(home, '.config'), 'systemd', 'user', JOBS[job].unit);
  return null;
}

/** The jobs defined on this machine, found by their definitions alone — asking a manager is slow, and this is asked at
 *  every launch: `{kind, core, connector}` (each a file or null), or null when neither is. */
export function installedService(env = process.env) {
  const kind = platformKind(env);
  const files = Object.fromEntries(ORDER.map(job => { const file = definitionPath(kind, job, env); return [job, file && existsSync(file) ? file : null]; }));
  return files.core || files.connector ? { kind, ...files } : null;
}

/** Run the manager's own command; never throws. Every call has an absolute deadline: 30 s, because launchd holds a
 *  `kickstart` of a job started less than its ThrottleInterval (10 s) ago until that interval has passed (measured on
 *  macos-14, an upgrade restarting the core right after an earlier one), and systemd's `stop` waits TimeoutStopSec (20). */
function manage(env, kind, args) {
  const bin = kind === 'launchd' ? env.SIDEVOICE_LAUNCHCTL || '/bin/launchctl'
    : kind === 'systemd' ? env.SIDEVOICE_SYSTEMCTL || 'systemctl' : env.SIDEVOICE_LOGINCTL || 'loginctl';
  try {
    const output = execFileSync(bin, args, { encoding: 'utf8', timeout: 30_000, env, stdio: ['ignore', 'pipe', 'pipe'] });
    return { ok: true, output, code: 0 };
  } catch (error) {
    return { ok: false, output: `${error.stdout || ''}${error.stderr || ''}` || error.message, code: error.status ?? error.code ?? null };
  }
}

/* ----- the definitions ----- */

/** The `SIDEVOICE_*` settings an installation is made with — never a credential of a core somebody else runs, nor a
 *  test's hooks, nor what only says where to install from. */
const NOT_SETTINGS = ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN', 'SIDEVOICE_SERVICE', 'SIDEVOICE_TEST_HOOKS',
  'SIDEVOICE_CORE_BIN', 'SIDEVOICE_CORE_SPEC', 'SIDEVOICE_CORE_WHEEL_DIR', 'SIDEVOICE_UV', 'SIDEVOICE_INSTALL_FROM_SOURCE', 'SIDEVOICE_INSTALLED_BY'];
export function installationSettings(env = process.env) {
  const kept = Object.entries(env).filter(([name]) => name.startsWith('SIDEVOICE_') && !NOT_SETTINGS.includes(name));
  return Object.fromEntries(kept.sort(([a], [b]) => a.localeCompare(b)));
}
/** Where the installation lives, resolved and absolute: what a job's process is started with, whatever the manager's
 *  environment says. */
export function installationPaths(env = process.env) {
  const home = env.HOME || os.homedir();
  return {
    SIDEVOICE_DATA_DIR: path.resolve(dataDirOf(env)),
    XDG_DATA_HOME: path.resolve(env.XDG_DATA_HOME || path.join(home, '.local', 'share')),
    XDG_CONFIG_HOME: path.resolve(env.XDG_CONFIG_HOME || path.join(home, '.config')),
  };
}
export function serviceEnvironment(kind, env = process.env) {
  return { ...installationSettings(env), ...installationPaths(env), SIDEVOICE_SERVICE: kind };
}

/** A value that goes into a definition: text with no control character. A newline in a path or a setting would
 *  start a directive of its own in a unit (an injected `ExecStartPre=`), and has no place in a plist either. */
function safeValue(text, what) {
  const value = String(text);
  if (/[\u0000-\u001f\u007f]/.test(value)) throw keyed('service.unsafe-value', { what });
  return value;
}
const xml = (text, what) => safeValue(text, what).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const unxml = text => text.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');

/** A LaunchAgent. `keepAlive`: `true` (the connector: back whenever it exits) or `crashed` (the core: back after a
 *  crash or a non-zero exit, never after exit 0). At most one start every 10 s. */
export function plistText({ label, program, log, environment = {}, keepAlive = true }) {
  const strings = items => items.map(item => `    <string>${xml(item, 'ProgramArguments')}</string>`).join('\n');
  const variables = Object.entries(environment).map(([name, value]) => `    <key>${xml(name, name)}</key>\n    <string>${xml(value, name)}</string>`).join('\n');
  const alive = keepAlive === 'crashed'
    ? '<dict>\n    <key>SuccessfulExit</key>\n    <false/>\n    <key>Crashed</key>\n    <true/>\n  </dict>'
    : '<true/>';
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${xml(label, 'Label')}</string>
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
  ${alive}
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>${xml(log, 'StandardOutPath')}</string>
  <key>StandardErrorPath</key>
  <string>${xml(log, 'StandardErrorPath')}</string>
</dict>
</plist>
`;
}

/* systemd, one serializer per directive (systemd.service(5), systemd.exec(5), systemd.unit(5)):
 * - ExecStart= words are quoted; inside, `\\` and `"` are escaped, `%` is a specifier (`%%`) and `$` is variable
 *   expansion (`$$`);
 * - Environment= is one quoted `NAME=value`: `\\` and `"` escaped, `%` a specifier — and `$` nothing special, so it
 *   is written as it is (doubling it there would change the value).
 * No value may hold a control character. Output goes to the journal. */
const execWord = (text, what) => `"${safeValue(text, what).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/%/g, '%%').replace(/\$/g, '$$$$')}"`;
const environmentAssignment = (name, value) => {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) throw keyed('service.unsafe-value', { what: name });
  return `"${name}=${safeValue(value, name).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/%/g, '%%')}"`;
};
const unexecWord = word => word.replace(/\\(.)/g, '$1').replace(/%%/g, '%').replace(/\$\$/g, '$');

/** A user unit: restarted `restart` (`on-failure`: the core; `always`: the connector) `restartSec` later, at most 5
 *  starts in 10 minutes (then `start-limit-hit`, which `service restart` clears); started with the user's manager. */
export function unitText({ description, program, environment = {}, restart = 'on-failure', restartSec = 10 }) {
  const variables = Object.entries(environment).map(([name, value]) => `Environment=${environmentAssignment(name, value)}`).join('\n');
  return `[Unit]
Description=${safeValue(description, 'Description')}
StartLimitIntervalSec=600
StartLimitBurst=${START_LIMIT}

[Service]
Type=simple
ExecStart=${program.map(word => execWord(word, 'ExecStart')).join(' ')}
${variables}
Restart=${restart}
RestartSec=${restartSec}
KillMode=control-group
TimeoutStopSec=20

[Install]
WantedBy=default.target
`;
}

/** The program a definition on disk runs, read back as its serializer wrote it. */
export function definitionProgram(kind, file) {
  let text; try { text = readFileSync(file, 'utf8'); } catch { return null; }
  if (kind === 'launchd') {
    const block = text.match(/<key>ProgramArguments<\/key>\s*<array>([\s\S]*?)<\/array>/)?.[1];
    return block ? [...block.matchAll(/<string>([^<]*)<\/string>/g)].map(match => unxml(match[1])) : null;
  }
  const line = text.match(/^ExecStart=(.*)$/m)?.[1];
  return line ? [...line.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map(match => unexecWord(match[1])) : null;
}

/** What each job runs: the core through `R/current`, the connector as `install.json`'s `command` says. */
export function jobPrograms(env = process.env) {
  const dataDir = dataDirOf(env);
  const record = readJson(nodeFiles(dataDir).install);
  if (!Array.isArray(record?.command) || !record.command.length) throw keyed('service.no-installation');
  return {
    core: [coreProgram(env), ...coreArgs({ dataDir, env, idleExit: 0, roomCredential: roomCredentialPath(dataDir, env) })],
    connector: [...record.command, 'connector', '--service'],
  };
}

/** The text of both definitions for this manager. */
export function definitionTexts(kind, env = process.env) {
  const programs = jobPrograms(env), environment = serviceEnvironment(kind, env), files = nodeFiles(dataDirOf(env));
  if (kind === 'launchd') return {
    core: plistText({ label: JOBS.core.label, program: programs.core, log: files.coreStderr, environment, keepAlive: 'crashed' }),
    connector: plistText({ label: JOBS.connector.label, program: programs.connector, log: files.connectorLog, environment, keepAlive: true }),
  };
  return {
    core: unitText({ description: 'Sidevoice core (this machine\'s conversations and voice)', program: programs.core, environment, restart: 'on-failure', restartSec: 10 }),
    connector: unitText({ description: 'Sidevoice connector (the harnesses\' link to the core)', program: programs.connector, environment, restart: 'always', restartSec: 2 }),
  };
}

/** Write each definition whose text would change; returns the jobs whose definition changed. */
export function writeDefinitions(kind, env = process.env) {
  const texts = definitionTexts(kind, env), changed = [];
  for (const job of ORDER) {
    const file = definitionPath(kind, job, env);
    let now = null; try { now = readFileSync(file, 'utf8'); } catch {}
    if (now === texts[job]) continue;
    writePrivate(file, texts[job]);
    changed.push(job);
  }
  return changed;
}

/* ----- the manager ----- */

const domain = () => `gui/${uid()}`;
const target = job => `${domain()}/${JOBS[job].label}`;
function loaded(kind, job, env) {
  if (kind === 'launchd') return manage(env, kind, ['print', target(job)]).ok;
  if (kind === 'systemd') return !/LoadState=not-found/.test(manage(env, kind, ['--user', 'show', '-p', 'LoadState', JOBS[job].unit]).output);
  return false;
}
/** `launchctl bootout` returns before launchd has let the job go, and a `bootstrap` in that interval fails
 *  ("5: Input/output error", measured on macos-14): the job is waited out, and the bootstrap tried again. */
async function bootOut(env, job) {
  const out = manage(env, 'launchd', ['bootout', target(job)]);
  for (let i = 0; i < 100 && loaded('launchd', job, env); i++) await wait(100);
  return loaded('launchd', job, env) ? { ok: false, output: out.output.trim() || 'still loaded' } : { ok: true };
}
async function bootIn(env, job) {
  const file = definitionPath('launchd', job, env);
  let booted = manage(env, 'launchd', ['bootstrap', domain(), file]);
  for (let i = 0; i < 10 && !booted.ok && !loaded('launchd', job, env); i++) { await wait(500); booted = manage(env, 'launchd', ['bootstrap', domain(), file]); }
  return booted.ok || loaded('launchd', job, env) ? { ok: true } : booted;
}

/** `launchctl print` read: `{loaded, running, pid, exit, signal, runs, restarting, reason}`. A job that is loaded,
 *  not running, and last ended badly is one launchd starts again (its `KeepAlive`), throttled. */
export function parseLaunchd(printed) {
  if (!printed.ok) return { loaded: false, running: false, pid: null, exit: null, runs: null, restarting: false, reason: 'not-loaded' };
  const field = name => (printed.output.match(new RegExp(`^\\s*${name} = (.*)$`, 'm')) || [])[1]?.trim() ?? null;
  const pid = Number(field('pid')) || null;
  const running = field('state') === 'running' && !!pid;
  const exit = /^-?\d+$/.test(field('last exit code') ?? '') ? Number(field('last exit code')) : null;
  const signal = field('last terminating signal');
  return { loaded: true, running, pid: running ? pid : null, exit, signal, runs: Number(field('runs')) || null,
    restarting: !running && ((exit !== null && exit !== 0) || !!signal), reason: null };
}
/** `systemctl --user show` read, the same shape. */
export function parseSystemd(shown) {
  const field = name => (shown.match(new RegExp(`^${name}=(.*)$`, 'm')) || [])[1]?.trim() ?? null;
  const load = field('LoadState');
  const pid = Number(field('ExecMainPID')) || null;
  const running = ['active', 'activating', 'reloading'].includes(field('ActiveState')) && field('SubState') !== 'auto-restart' && !!pid;
  return { loaded: !!load && load !== 'not-found', running, pid: running ? pid : null,
    exit: field('ExecMainStatus') === null ? null : Number(field('ExecMainStatus')), signal: null,
    runs: field('NRestarts') === null ? null : Number(field('NRestarts')), restarting: field('SubState') === 'auto-restart',
    reason: !load || load === 'not-found' ? 'not-loaded' : field('Result') === 'start-limit-hit' ? 'start-limit' : null };
}
export function managerState(kind, job, env = process.env) {
  if (kind === 'launchd') return parseLaunchd(manage(env, kind, ['print', target(job)]));
  if (kind === 'systemd') {
    const shown = manage(env, kind, ['--user', 'show', '-p', 'LoadState,ActiveState,SubState,Result,ExecMainStatus,ExecMainPID,NRestarts', JOBS[job].unit]);
    return parseSystemd(shown.ok ? shown.output : '');
  }
  return { loaded: false, running: false, pid: null, exit: null, runs: null, restarting: false, reason: null };
}

/** Why a defined job's program cannot run, if it cannot: `executable-missing` or `permission-denied`. */
function programProblem(program) {
  const file = program?.[0];
  if (!file) return 'executable-missing';
  if (!existsSync(file)) return 'executable-missing';
  try { accessSync(file, constants.X_OK); } catch { return 'permission-denied'; }
  for (const item of program.slice(1, 2)) if (path.isAbsolute(item) && /\.m?js$/.test(item) && !existsSync(item)) return 'executable-missing';
  return null;
}

/* ----- what the node is doing (§2.8) ----- */

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

/** Everything `deriveStatus` needs, read fresh: the definitions on disk, the manager's view of each job, the stop
 *  marker, the core's health on its socket, its ready file and its failure report. `connectorRunning`: the connector
 *  answering this (itself), else asked. */
export async function observe(env = process.env, { connectorRunning = null } = {}) {
  const dataDir = dataDirOf(env), files = nodeFiles(dataDir);
  const service = installedService(env);
  const kind = service?.kind ?? managerKind(env);
  const defined = { core: !!service?.core, connector: !!service?.connector };
  const jobs = Object.fromEntries(ORDER.map(job => [job, defined[job] ? managerState(kind, job, env) : null]));
  const ready = readReady(dataDir);
  const health = await localHealth(ready?.socket || socketPathOf(dataDir), 1500);
  const report = readFailure(dataDir);
  return {
    service: kind, installed: !!selection(env, 'current'), defined, jobs,
    stopped: existsSync(files.stopped), health, ready,
    failure: report ? describeFailure(dataDir, report) : null,
    coreAge: jobs.core?.running ? processAge(jobs.core.pid) : null,
    program: defined.core ? programProblem(definitionProgram(kind, service.core)) : null,
    logTail: logTail(dataDir),
    connectorRunning: connectorRunning ?? !!(await askConnector('status', {}, { env, timeout: 1000 })),
  };
}

/** The node's state from what was observed (§2.8; SEAMS §4), in this order:
 *  1. the core answers its health → `running` — unless the service condition says otherwise (no job, a person's stop,
 *     a manager that does not run the job), which wins, with `reachable: true`;
 *  2. nothing installed, nothing defined → `absent`;  3. no core job → `not-installed`;  4. stopped → `stopped-by-person`;
 *  5. the manager does not have the job loaded, hit its start limit, or its program is missing → `service-failed`;
 *  6. job running, not ready yet, for less than 60 s → `starting` (longer: `failed`, `ready.timeout`);
 *  7. job running and ready, health silent → `failed`, `hang`;
 *  8. job not running, the core's failure report there → `failed` with it;
 *  9. job not running, the manager will start it again → `backoff`, with the manager's count;
 *  10. otherwise → `failed`, `launch.exited`. */
export function deriveStatus(o) {
  const body = o.health?.status === 200 ? o.health.body : null;
  const base = { ok: true, service: o.service, installed: !!o.installed,
    core: body ? { pid: body.pid ?? null, version: body.version ?? null, api: body.api ?? null, launch_id: body.launch_id ?? null } : null,
    calls: body ? (typeof body.calls === 'number' ? body.calls : null) : null,
    failure: null, attempts: null, limit: null, since: null, window_started: null, next_retry_at: null,
    reachable: !!body, connector: { running: !!o.connectorRunning } };
  const job = o.jobs?.core;
  const serviceFailure = o.defined?.core ? (job?.reason || o.program || null) : null;
  const condition = () => {
    if (!o.installed && !o.defined?.core && !o.defined?.connector) return { ...base, state: 'absent' };
    if (!o.defined?.core) return { ...base, state: 'not-installed' };
    if (o.stopped) return { ...base, state: 'stopped-by-person' };
    if (serviceFailure) return { ...base, state: 'service-failed', failure: { key: serviceFailure } };
    return null;
  };
  const held = condition();
  if (body) return held ?? { ...base, state: 'running' };
  if (held) return held;
  const tail = o.logTail ?? [];
  const exited = detail => ({ key: 'launch.exited', step: 'run', message: t('launch.exited', { detail: detail ?? '?' }), detail, at: new Date().toISOString(), log_tail: tail });
  if (job?.running) {
    if (!o.ready || o.ready.pid !== job.pid) {
      return o.coreAge !== null && o.coreAge !== undefined && o.coreAge > STARTING_S
        ? { ...base, state: 'failed', failure: { key: 'ready.timeout', step: 'ready', message: t('ready.timeout'), at: new Date().toISOString(), log_tail: tail } }
        : { ...base, state: 'starting' };
    }
    return { ...base, state: 'failed', failure: { key: 'hang', step: 'health', message: t('hang'), at: new Date().toISOString(), log_tail: tail } };
  }
  if (o.failure) return { ...base, state: 'failed', failure: o.failure };
  const lastExit = job?.signal ?? job?.exit ?? null;
  if (job?.restarting) return { ...base, state: 'backoff', failure: exited(lastExit), attempts: job.runs ?? null, limit: o.service === 'systemd' ? START_LIMIT : null };
  return { ...base, state: 'failed', failure: exited(lastExit) };
}

/** `service status --json` (SEAMS §3–§4): derived, read-only, never starts anything. */
export async function status(env = process.env, options = {}) {
  return deriveStatus(await observe(env, options));
}

/** Whether a status is final for whoever just started the core: running, or a failure that will not change by waiting —
 *  the manager cannot run the job, or the core reported why it did not start. An exit the manager may follow with a
 *  start, a health probe unanswered a moment after a restart, a core still starting: not yet. */
export function settledState(now) {
  if (now.state === 'failed') return !['run', 'health', 'ready'].includes(now.failure?.step);
  return !['starting', 'backoff'].includes(now.state);
}

/** Wait, up to `timeout`, for the core's state to settle: what start and restart answer with. */
async function settled(env, timeout = SETTLE_MS()) {
  const deadline = Date.now() + timeout;
  let now = await status(env);
  while (!settledState(now) && Date.now() < deadline) { await wait(200); now = await status(env); }
  return now;
}

/* ----- what runs without a manager (§2.5 step 3) ----- */

/** The connector holding its lock, by the lock's record — and only while the lock is held: a lock this process can take
 *  has no holder, whatever its file says. */
async function lockHolder(env) {
  const file = connectorLockOf(env);
  if (!existsSync(file)) return null;
  const probe = await tryLock(file, { kind: 'probe', env });
  if (probe.held) { probe.release(); return null; }
  const record = probe.owner ?? readLock(file);
  return record && isProcess(record.pid, { start: record.start ?? null }) ? { pid: record.pid, start: record.start ?? null } : null;
}

/** Stop what runs outside a manager — an on-demand connector, and every core of this data directory (ready, or still
 *  starting) — each signalled only as its verified self, and wait until both are gone and their sockets silent. Returns
 *  `{killed, left}`. */
export async function stopOnDemand(env = process.env) {
  const dataDir = dataDirOf(env);
  const connector = await lockHolder(env);
  if (connector) signalVerified(connector.pid, 'SIGTERM', { start: connector.start });
  const connectorUp = () => !!connector && isProcess(connector.pid, { start: connector.start });
  const deadline = Date.now() + TEARDOWN_MS();
  while (connectorUp() && Date.now() < deadline) await wait(100);
  const killed = [];
  if (connectorUp() && signalVerified(connector.pid, 'SIGKILL', { start: connector.start })) killed.push(connector.pid);
  for (const pid of coreProcesses(dataDir)) {
    await terminateCore(dataDir, pid, { grace: TEARDOWN_MS() });
    if (coreRunning(dataDir, pid)) killed.push(pid);
  }
  for (let i = 0; i < 50 && (connectorUp() || (await askConnector('status', {}, { env, timeout: 300 }))); i++) await wait(100);
  const left = [connectorUp() && connector.pid, ...coreProcesses(dataDir)].filter(Boolean);
  return { killed, left };
}

/* ----- starting and stopping the jobs ----- */

/** Each job started from its definition as it is now: a changed definition is loaded again (launchd reads a plist only
 *  when it is bootstrapped; systemd reloads), and with `restart` a running job is restarted (launchd `kickstart -k`;
 *  systemd `reset-failed` + `restart`, which clears a start limit — an explicit retry, or a new release). */
export async function startJobs(env, { changed = [], restart = false, jobs = ORDER } = {}) {
  const kind = installedService(env)?.kind;
  // A core about to be started again: its last failure report is about a start that is over (the core deletes it
  // too, once it holds its lock), and must not be read as this start's while it begins.
  if (jobs.includes('core') && (restart || changed.includes('core'))) rmSync(failurePath(dataDirOf(env)), { force: true });
  const fail = (job, out) => { throw keyed('service.not-loaded', { detail: `${JOBS[job].label}: ${String(out.output || '').trim()}` }); };
  if (kind === 'launchd') {
    for (const job of jobs) {
      if (changed.includes(job) && loaded(kind, job, env)) { const out = await bootOut(env, job); if (!out.ok) fail(job, out); }
      if (!loaded(kind, job, env)) { const booted = await bootIn(env, job); if (!booted.ok) fail(job, booted); continue; }
      const kicked = manage(env, kind, ['kickstart', ...(restart ? ['-k'] : []), target(job)]);
      if (!kicked.ok) fail(job, kicked);
    }
  } else if (kind === 'systemd') {
    if (changed.length) manage(env, kind, ['--user', 'daemon-reload']);
    for (const job of jobs) {
      manage(env, kind, ['--user', 'enable', JOBS[job].unit]);
      manage(env, kind, ['--user', 'reset-failed', JOBS[job].unit]);
      const out = manage(env, kind, ['--user', restart || changed.includes(job) ? 'restart' : 'start', JOBS[job].unit]);
      if (!out.ok) throw keyed(`service.${managerState(kind, job, env).reason || programProblem(definitionProgram(kind, definitionPath(kind, job, env))) || 'not-loaded'}`, { detail: out.output.trim() });
    }
  } else throw keyed('service.no-manager');
}

/** Both jobs unloaded (the connector first): launchd `bootout`, waited out; systemd `stop` (or `disable --now`).
 *  Returns the first refusal, or null. */
async function unloadJobs(env, { disable = false } = {}) {
  const service = installedService(env);
  if (!service) return null;
  for (const job of [...ORDER].reverse()) {
    if (!service[job]) continue;
    if (service.kind === 'launchd') {
      if (!loaded('launchd', job, env)) continue;
      const out = await bootOut(env, job);
      if (!out.ok) return `${JOBS[job].label}: ${out.output}`;
    } else if (service.kind === 'systemd') {
      const out = manage(env, 'systemd', ['--user', ...(disable ? ['disable', '--now'] : ['stop']), JOBS[job].unit]);
      if (!out.ok && loaded('systemd', job, env)) return `${JOBS[job].unit}: ${out.output.trim()}`;
    }
  }
  return null;
}

/** The commands, each under the install lock (`underLock`). */
async function underLock(env, operation) {
  const release = await takeInstallLock(dataDirOf(env));
  try { return await operation(); } finally { release(); }
}

/** `service install`: both definitions written for the selected installation, anything running on demand stopped,
 *  both jobs started; answered with the core's state once it settles. Linux says what linger would add. */
export async function install(env = process.env) {
  return underLock(env, async () => {
    if (!selection(env, 'current')) throw keyed('service.no-installation');
    const kind = managerKind(env);
    if (kind === 'none') throw keyed('service.no-manager');
    rmSync(nodeFiles(dataDirOf(env)).stopped, { force: true });
    const had = installedService(env);
    const changed = writeDefinitions(kind, env);
    if (!had) await stopOnDemand(env);
    await startJobs(env, { changed });
    const now = await settled(env, Number(env.SIDEVOICE_INSTALL_VERIFY_MS || 60_000));
    return { ok: true, state: now.state, service: kind, ...(now.failure ? { failure: now.failure } : {}), ...(kind === 'systemd' ? { linger: linger(env) } : {}) };
  });
}

/** Linux: whether this user's services run without a session, and how to make them (O4). */
export function linger(env = process.env) {
  const user = env.USER || os.userInfo().username;
  const enabled = /Linger=yes/.test(manage(env, 'loginctl', ['show-user', user, '-p', 'Linger']).output);
  const command = `loginctl enable-linger ${user}`;
  if (!enabled && RUN_LINGER) manage(env, 'loginctl', ['enable-linger', user]);
  return { enabled, command, reason: t('service.linger-reason') };
}

/** `service start`: a stop no longer holds; the manager starts both jobs. With no jobs, nothing is started here — the
 *  next conversation starts Sidevoice on demand. */
export async function start(env = process.env) {
  const service = installedService(env);
  await underLock(env, async () => {
    rmSync(nodeFiles(dataDirOf(env)).stopped, { force: true });
    if (service) await startJobs(env);
  });
  const now = await settled(env);
  return { ok: true, state: now.state, service: service?.kind ?? 'none' };
}

/** `service stop`: the person's stop — written first, so nothing starts Sidevoice again meanwhile — then the manager
 *  stops both jobs (launchd: bootout, so KeepAlive does not bring them back until the next login). With no jobs, what
 *  runs on demand is stopped. */
export async function stop(env = process.env) {
  return underLock(env, async () => {
    writePrivate(nodeFiles(dataDirOf(env)).stopped, { at: new Date().toISOString() });
    const service = installedService(env);
    const refused = await unloadJobs(env);
    if (refused) throw keyed('service.unload-failed', { detail: refused });
    const down = await stopOnDemand(env);
    return { ok: !down.left.length, state: 'stopped-by-person', service: service?.kind ?? 'none', ...(down.killed.length ? { note: t('service.killed', { pids: down.killed.join(', ') }) } : {}) };
  });
}

/** `service restart` — the person's «Reintentar»: the core job restarted (its start limit cleared first); stopped, it is
 *  a start. With no jobs, the core running on demand is ended, and the next conversation starts it again. */
export async function restart(env = process.env) {
  if (existsSync(nodeFiles(dataDirOf(env)).stopped)) return start(env);
  const service = installedService(env);
  await underLock(env, async () => {
    if (service?.core) await startJobs(env, { restart: true, jobs: ['core'] });
    else for (const pid of coreProcesses(dataDirOf(env))) await terminateCore(dataDirOf(env), pid);
  });
  const now = await settled(env);
  return { ok: true, state: now.state, service: service?.kind ?? 'none' };
}

/** `service uninstall` (§2.5 steps 1–4): the stop written, both jobs unloaded — any refusal stops here, with nothing
 *  deleted — what runs on demand stopped, both definitions deleted and the manager told. `keepStopped`: the caller goes
 *  on taking the installation apart and clears the marker itself. Idempotent. */
export async function uninstall(env = process.env, { keepStopped = false } = {}) {
  return underLock(env, async () => {
    const files = nodeFiles(dataDirOf(env));
    const service = installedService(env);
    const kind = service?.kind ?? 'none';
    writePrivate(files.stopped, { at: new Date().toISOString() });
    const refused = await unloadJobs(env, { disable: true });
    if (refused) throw keyed('service.unload-failed', { detail: refused });
    const down = await stopOnDemand(env);
    if (down.left.length) throw keyed('service.unload-failed', { detail: `pid ${down.left.join(', ')} still running` });
    if (service) {
      for (const job of ORDER) if (service[job]) rmSync(service[job], { force: true });
      if (kind === 'systemd') {
        const reloaded = manage(env, kind, ['--user', 'daemon-reload']);
        if (!reloaded.ok) throw keyed('service.unload-failed', { detail: reloaded.output.trim() });
        for (const job of ORDER) manage(env, kind, ['--user', 'reset-failed', JOBS[job].unit]);
      }
    }
    if (!keepStopped) rmSync(files.stopped, { force: true });
    return { ok: true, state: selection(env, 'current') ? 'not-installed' : 'absent', service: kind,
      ...(down.killed.length ? { note: t('service.killed', { pids: down.killed.join(', ') }) } : {}) };
  });
}

/** Whether a core speaks what this connector speaks: `api` from its health, `link` from its ready file. */
export function compatibleCore(core, ready) {
  const within = (value, [low, high]) => Number.isInteger(value) && value >= low && value <= high;
  return within(core?.api, API_RANGE) && within(ready?.protocol, LINK_RANGE);
}

/** `sidevoice service <install|uninstall|start|stop|restart|status> [--json]`. */
export async function run(argv = [], env = process.env) {
  const [action] = argv.filter(item => !item.startsWith('-'));
  const json = argv.includes('--json');
  const actions = { install, uninstall, start, stop, restart, status };
  if (!actions[action]) {
    if (json) { console.log(JSON.stringify({ ok: false, error: { key: 'service.usage', message: t('service.usage') } })); return 1; }
    console.error(t('service.usage')); return 2;
  }
  let result;
  try { result = await actions[action](env); }
  catch (error) { result = { ok: false, error: { key: error.key || 'service.failed', message: error.message } }; }
  if (json) console.log(JSON.stringify(result));
  else if (!result.ok) console.error(result.error?.message || t('service.failed'));
  else {
    console.log(t(`service.state.${result.state}`, { service: result.service }) + (result.failure?.key ? ` (${result.failure.message || t('service.reason.' + result.failure.key)})` : ''));
    if (result.note) console.log(result.note);
    if (result.linger && !result.linger.enabled) console.log(`\n${result.linger.reason}\n    ${result.linger.command}`);
  }
  return result.ok ? 0 : 1;
}
