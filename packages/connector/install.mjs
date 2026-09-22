#!/usr/bin/env node
/** `sidevoice install` — put this version of Sidevoice in front of the harnesses on this machine.
 *
 *  It registers the MCP server (re-pinned to this version when an older one was registered), installs
 *  the skill, says what it changed, and is safe to run twice: run it again after an upgrade and the
 *  harness points at the new version. It pairs with nothing. Pairing is a person's act — the room shows
 *  a one-time code to whoever is in it, and a conversation asks for it the first time it joins — so
 *  the installer only reports whether this machine is paired, and with which room.
 *
 *  What it does not do is decide for the person: it never edits a machine-wide Codex configuration it
 *  does not own, and it never relaxes Claude Code's inbound safeguard — those it prints, with the reason. */
import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { harnessesPresent } from './identity.mjs';
import { pairedRoom } from './pair.mjs';
import { remove as removeSkill, skillsDir, status as skillStatus } from './skill.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
/** Where the package's own root is: next to these modules in the checkout, and one level up once
 *  they have been bundled into `dist/`. Only copying needs to know — everything else reads the
 *  `package.json` beside it, which the build puts there precisely so that this stays the one
 *  place that has to tell the two apart. */
const packageRoot = existsSync(path.join(here, '..', 'package.json')) ? path.dirname(here) : here;
const manifest = JSON.parse(readFileSync(path.join(here, 'package.json'), 'utf8'));
const VERSION = manifest.version;
/** The files that make up this package, copied as they are — no install step, nothing fetched. */
const PACKAGE_FILES = manifest.files.concat('package.json');
/** What `node` is given to run this package, relative to a copy of it. */
const ENTRY = manifest.bin.sidevoice.replace(/^\.\//, '');

function fromSource(env) {
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '0') return false;
  return env.SIDEVOICE_INSTALL_FROM_SOURCE === '1' || existsSync(path.join(packageRoot, '..', '..', '.git'));
}

/** Where installed copies live: one directory per version, under the XDG data home. */
export function copiesDir(env = process.env) {
  return path.join(env.XDG_DATA_HOME || path.join(os.homedir(), '.local', 'share'), 'sidevoice');
}

/** What a harness should run to start the server. From a checkout it names that checkout, so a machine that
 *  installed from source keeps working when the published version moves. Otherwise it names a copy of this
 *  package that install placed on disk — never `npx`: a session start is not the moment to resolve a package
 *  (a cold cache, a bin whose name differs from the package's, a 30 s startup budget; one session found no
 *  `sidevoice` binary at all, 2026-09-21). */
export function serverCommand(env = process.env) {
  const cli = fromSource(env) ? path.join(here, 'cli.mjs') : path.join(copiesDir(env), VERSION, ENTRY);
  return { command: 'node', args: [cli, 'mcp'] };
}

/** Put this version's files where serverCommand points, and drop the other versions: an installed copy is
 *  disposable and there is one current one. From a checkout nothing is copied. */
export function materialize(env = process.env) {
  if (fromSource(env)) return { action: 'checkout', target: here };
  const root = copiesDir(env), target = path.join(root, VERSION);
  mkdirSync(target, { recursive: true });
  for (const file of PACKAGE_FILES) {
    const source = path.join(packageRoot, file);
    if (existsSync(source)) cpSync(source, path.join(target, file), { recursive: true });
  }
  const removed = [];
  for (const name of readdirSync(root)) {
    if (name !== VERSION) { rmSync(path.join(root, name), { recursive: true, force: true }); removed.push(name); }
  }
  return { action: 'copied', target, removed };
}

/** The connector that holds this machine's socket, if any, and which version it is: a façade uses whatever
 *  connector is running, so one left over from before an upgrade serves every new session with old code. */
export function runningConnector(env = process.env) {
  const dataDir = env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
  const socketPath = env.SIDEVOICE_CONNECTOR_SOCKET || path.join(dataDir, 'connector.sock');
  let pid = null;
  try { pid = Number(readFileSync(socketPath + '.lock', 'utf8')) || null; } catch { return null; }
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

function claude(args, env) {
  return execFileSync(env.SIDEVOICE_CLAUDE_BIN || 'claude', args, { encoding: 'utf8', timeout: 30_000, env, stdio: ['ignore', 'pipe', 'pipe'] });
}

/** What Claude Code currently runs for `sidevoice`, read from its own `mcp get`: null when nothing is
 *  registered or there is no `claude` to ask. The scope matters — only a user-scope entry is ours to move. */
export function claudeRegistration(env = process.env) {
  let output;
  try { output = claude(['mcp', 'get', 'sidevoice'], env); } catch { return null; }
  const field = name => (output.match(new RegExp(`^\\s*${name}:\\s*(.*)$`, 'm')) || [])[1]?.trim() ?? '';
  const command = field('Command'), args = field('Args');
  if (!command) return null;
  return { scope: /user/i.test(field('Scope')) ? 'user' : 'other', line: [command, args].filter(Boolean).join(' ') };
}

function registerWithClaude(done, env) {
  const { command, args } = serverCommand(env);
  const wanted = [command, ...args].join(' ');
  const manual = `claude mcp add --scope user sidevoice -- ${wanted}`;
  const current = claudeRegistration(env);
  if (current?.line === wanted) { done.push('Claude Code already runs this version of the MCP server.'); return; }
  if (current && current.scope !== 'user') {
    done.push(`Claude Code has a sidevoice MCP server registered outside user scope (${current.line}); not touched. To move it:\n    ${manual}`);
    return;
  }
  try {
    if (current) claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env);
    claude(['mcp', 'add', '--scope', 'user', 'sidevoice', '--', command, ...args], env);
    done.push(current ? `Re-pointed Claude Code's MCP server to this version (was: ${current.line}).`
                      : 'Registered the MCP server with Claude Code (user scope).');
  } catch (error) {
    done.push(`Could not register with Claude Code automatically (${(error.message || '').split('\n')[0]}). Run:\n    ${manual}`);
  }
}

/** Codex keeps one machine-wide file that may hold anything its user put there: we never rewrite it. */
export function codexInstructions(env = process.env) {
  const { command, args } = serverCommand(env);
  return [
    `Add to ${env.CODEX_HOME || path.join(os.homedir(), '.codex')}/config.toml — it is machine-wide and`,
    'this package does not rewrite it:',
    '',
    '  [mcp_servers.sidevoice]',
    `  command = "${command}"`,
    `  args = [${args.map(a => `"${a}"`).join(', ')}]`,
    '',
    'Then restart Codex. That is all: read receipts and working state come from what Codex records about the thread.',
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

export async function install(argv = process.argv.slice(2), env = process.env) {
  const stray = argv.find(item => !item.startsWith('-') && argv[argv.indexOf(item) - 1] !== '--harness');
  if (stray) throw new Error(`usage: sidevoice install [--harness claude|codex]\n` +
    `Pairing is not part of installing: a conversation asks for the room's code the first time it joins, ` +
    `or run  sidevoice pair <room-url> <code>  with the code the room shows under "Emparejar máquina".`);
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];

  done.push(`Sidevoice ${VERSION}.`);
  const copy = materialize(env);
  if (copy.action === 'copied') done.push(`Copied this version to ${copy.target}${copy.removed.length ? ` (removed: ${copy.removed.join(', ')})` : ''}.`);
  if (harnesses.includes('claude')) {
    registerWithClaude(done, env);
    // The join shortcut is a prompt the server offers; a skill copy from an earlier version is taken away.
    if (skillStatus(skillsDir([], env)).state === 'installed') done.push(`Removed the voice-room skill copy at ${removeSkill(skillsDir([], env)).target}: the server offers it as the prompt /mcp__sidevoice__voice-room.`);
  }

  const paired = pairedRoom(env);
  done.push(paired ? `This machine is paired with ${paired.origin} (connector ${paired.connector_id}).`
                   : 'This machine is not paired with any room yet.');
  const running = await runningConnector(env);
  if (running && running.version !== VERSION) {
    next.push(`A connector from ${running.version ? 'version ' + running.version : 'an older version'} is still running (pid ${running.pid}) and every conversation on this machine uses it. ` +
              `It exits by itself 15 s after the last conversation leaves it; to switch now: kill ${running.pid}, then join again from each conversation.`);
  }

  if (harnesses.includes('claude')) {
    next.push('In a conversation, ask to join the voice room (or run /mcp__sidevoice__voice-room).' +
              (paired ? '' : ' The first time, the conversation asks you for the room\'s address and the one-time code the room shows under "Emparejar máquina".'));
    next.push('Sessions already open need a restart before they see the server.');
    const warning = inboundWarning(env);
    if (warning) next.push(warning);
  }
  if (harnesses.includes('codex')) next.push(codexInstructions(env));
  if (!harnesses.length) next.push('No harness found on this machine. Pass --harness claude or --harness codex.');
  return { done, next };
}

/** `sidevoice uninstall`: the reverse of install, for this machine. Unregisters the MCP server from Claude
 *  Code, stops the connector, removes the installed copies, the skill copy an older version left, and the
 *  pairing credential. The room keeps this machine's pairing until it is revoked under "Máquinas" on
 *  the room's page — say so, and say where. Codex's machine-wide file is, as always, printed and not
 *  touched. */
export async function uninstall(argv = process.argv.slice(2), env = process.env) {
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];
  if (harnesses.includes('claude')) {
    const current = claudeRegistration(env);
    if (current?.scope === 'user') {
      try { claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env); done.push('Unregistered the MCP server from Claude Code.'); }
      catch (error) { done.push(`Could not unregister from Claude Code (${(error.message || '').split('\n')[0]}). Run:\n    claude mcp remove --scope user sidevoice`); }
    } else if (current) {
      next.push(`Claude Code has a sidevoice MCP server registered outside user scope (${current.line}); remove it where it was added.`);
    } else done.push('Claude Code had no sidevoice MCP server registered.');
    if (skillStatus(skillsDir([], env)).state === 'installed') done.push(`Removed the voice-room skill copy at ${removeSkill(skillsDir([], env)).target}.`);
  }
  const running = await runningConnector(env);
  if (running?.pid) {
    try { process.kill(running.pid, 'SIGTERM'); done.push(`Stopped the connector (pid ${running.pid}${running.version ? ', version ' + running.version : ''}).`); }
    catch (error) { next.push(`A connector is running (pid ${running.pid}) and could not be stopped (${error.code || error.message}); stop it yourself.`); }
  }
  if (!fromSource(env) && existsSync(copiesDir(env))) { rmSync(copiesDir(env), { recursive: true, force: true }); done.push(`Removed the installed copies under ${copiesDir(env)}.`); }
  const dataDir = env.SIDEVOICE_DATA_DIR || path.join(os.homedir(), '.sidevoice');
  const paired = pairedRoom(env);
  if (existsSync(dataDir)) {
    rmSync(dataDir, { recursive: true, force: true });
    done.push(`Removed ${dataDir} (credential, socket, outbox, log).`);
    if (paired) next.push(`The room at ${paired.origin} still lists this machine as paired (connector ${paired.connector_id}) until you revoke it under "Máquinas" on the room's page.`);
  }
  if (harnesses.includes('codex')) next.push(`Remove the [mcp_servers.sidevoice] table from ${env.CODEX_HOME || path.join(os.homedir(), '.codex')}/config.toml — it is machine-wide and this package does not rewrite it.`);
  next.push('Sessions already open keep their MCP server until they end.');
  return { done, next };
}

if (process.env.SIDEVOICE_UNINSTALL_MAIN === '1') {
  try {
    const { done, next } = await uninstall();
    for (const line of done) console.log('· ' + line);
    if (next.length) { console.log('\nLeft for you:'); for (const line of next) console.log('\n' + line); }
  } catch (error) { console.error(error.message); process.exit(1); }
}

if (process.env.SIDEVOICE_INSTALL_MAIN === '1') {
  try {
    const { done, next } = await install();
    for (const line of done) console.log('· ' + line);
    if (next.length) {
      console.log('\nLeft for you:');
      for (const line of next) console.log('\n' + line);
    }
  } catch (error) { console.error(error.message); process.exit(1); }
}
