/** Our MCP server in each harness, and the installation it runs: what `install` registers and what `uninstall` takes
 *  out — only ever Sidevoice's own entries.
 *
 *  An installation is run by one command (`install.json`, SEAMS §1): R1 `node R/current/dist/cli.mjs`, R4 the single
 *  executable `R/current/sidevoice` — a path through `R/current` (`release.mjs`), so a registration is written once,
 *  when the person consents, and an update never touches it. A harness is given that command with `mcp`. */
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { dataDirOf, nodeFiles, readJson } from './node-files.mjs';
import { t } from './i18n.mjs';
import { crash } from './testpoint.mjs';
import { releaseRoot, stableCommand } from './release.mjs';

/** Where installations live (`R`): `$XDG_DATA_HOME/sidevoice`. */
export function copiesDir(env = process.env) {
  return releaseRoot(env);
}

/** The installation as `install.json` records it — or, nothing installed yet, the one an install would record. */
export function selected(env = process.env) {
  let record = null;
  try { record = readJson(nodeFiles(dataDirOf(env)).install); } catch {}
  return Array.isArray(record?.command) && record.command.length ? record : { command: stableCommand(env) };
}

/** What a harness runs for an installation: R1 `node <cli.mjs> mcp` — `node` from the harness's own PATH, as
 *  before — and R4 `<executable> mcp`. */
export function registration(record) {
  const [program, cli] = record.command;
  return cli ? { command: 'node', args: [cli, 'mcp'] } : { command: program, args: ['mcp'] };
}

/** What a harness should run to start the server: the installation's command — never `npx`: a session start is not
 *  the moment to resolve a package (a cold cache, a bin whose name differs from the package's, a 30 s startup budget;
 *  one session found no `sidevoice` binary at all, 2026-09-21). */
export function serverCommand(env = process.env, record = selected(env)) {
  return registration(record);
}

/* ----- what is ours ----- */

/** Whether this program (a CLI path run by node, or an executable) is one of ours: under `R` — through `current`, in a
 *  release, or in a copy an earlier version made (`R/<id>`) — or the program `install.json` records. */
export function ourProgram(program, env = process.env) {
  if (!program || !path.isAbsolute(program)) return false;
  const root = copiesDir(env) + path.sep;
  if (program.startsWith(root)) {
    let rest = program.slice(root.length).split(path.sep);
    if (rest[0] === 'releases') rest = rest.slice(1);
    // <R>/{current|releases/<id>|<id>}/dist/cli.mjs (R1) or …/dist/sidevoice (R4); an id is one path segment.
    if (rest.length === 3 && rest[1] === 'dist' && ['cli.mjs', 'sidevoice'].includes(rest[2])) return /^[\w.+-]+$/.test(rest[0]);
  }
  let record = null; try { record = readJson(nodeFiles(dataDirOf(env)).install); } catch {}
  return !!record?.command && (record.command[1] || record.command[0]) === program;
}

/** Whether a harness entry `{command, args}` runs one of ours as `mcp` — or is the package itself through npx. */
export function oursEntry(entry, env = process.env) {
  const { command, args = [] } = entry || {};
  if (!command || !Array.isArray(args) || args.at(-1) !== 'mcp') return false;
  if (args.some(arg => /^@sidevoice\/uplink(@[\w.-]+)?$/.test(arg))) return true;
  if (args.length === 2 && path.basename(command) === 'node') return ourProgram(args[0], env);
  if (args.length === 1) return ourProgram(command, env);
  return false;
}

/* ----- Claude Code ----- */

function claude(args, env) {
  return execFileSync(env.SIDEVOICE_CLAUDE_BIN || 'claude', args, { encoding: 'utf8', timeout: 30_000, env, stdio: ['ignore', 'pipe', 'pipe'] });
}
/** Whether there is a `claude` to ask: an entry can only be added through it. */
export function claudeReachable(env = process.env) {
  try { claude(['--version'], env); return true; } catch (error) { return error.code !== 'ENOENT' && error.code !== 'EACCES'; }
}

/** What Claude Code currently runs for `sidevoice`, read from its own `mcp get`: null when nothing is registered
 *  or there is no `claude` to ask. Only a user-scope entry is ours to move. */
export function claudeRegistration(env = process.env) {
  let output;
  try { output = claude(['mcp', 'get', 'sidevoice'], env); } catch { return null; }
  const field = name => (output.match(new RegExp(`^\\s*${name}:\\s*(.*)$`, 'm')) || [])[1]?.trim() ?? '';
  const command = field('Command'), args = field('Args');
  if (!command) return null;
  return { scope: /user/i.test(field('Scope')) ? 'user' : 'other', line: [command, args].filter(Boolean).join(' '), command, args: args ? args.split(/\s+/) : [] };
}

/** Claude Code's entry, judged: `absent`, `ours` (with its line) or `foreign`. */
export function claudeState(env = process.env) {
  const current = claudeRegistration(env);
  if (!current) return { state: 'absent' };
  return { state: current.scope === 'user' && oursEntry(current, env) ? 'ours' : 'foreign', line: current.line };
}

/** Make Claude Code run this installation. Replacing is two steps of Claude's own (remove, add): an installer
 *  that dies between them leaves no entry, which recovery adds back (the journal says it was there). */
export function setClaude(env, record) {
  const { command, args } = registration(record);
  const current = claudeState(env);
  if (current.state === 'foreign') return 'foreign';
  if (current.state === 'ours' && current.line === [command, ...args].join(' ')) return 'unchanged';
  if (current.state === 'ours') { claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env); crash('claude-removed'); }
  claude(['mcp', 'add', '--scope', 'user', 'sidevoice', '--', command, ...args], env);
  return current.state === 'ours' ? 'repointed' : 'added';
}

/** Take our entry out of Claude Code; a foreign one stays. */
export function removeClaude(env) {
  const current = claudeState(env);
  if (current.state !== 'ours') return current.state;
  claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env);
  return 'removed';
}

/** `install`'s words for Claude Code, after the transaction set it. */
export function registerWithClaude(done, env = process.env, record = selected(env)) {
  const { command, args } = registration(record);
  const current = claudeRegistration(env);
  if (current && !(current.scope === 'user' && oursEntry(current, env))) {
    done.push(t('install.claude-foreign', { line: current.line, manual: `claude mcp add --scope user sidevoice -- ${[command, ...args].join(' ')}` }));
  }
}

/** Take our entry out of Claude Code — only ours, only at user scope. */
export function unregisterFromClaude(done, next, env = process.env) {
  const current = claudeState(env);
  if (current.state === 'ours') {
    try { removeClaude(env); done.push('Unregistered the MCP server from Claude Code.'); }
    catch (error) { done.push(`Could not unregister from Claude Code (${(error.message || '').split('\n')[0]}). Run:\n    claude mcp remove --scope user sidevoice`); }
  } else if (current.state === 'foreign') {
    next.push(`Claude Code has a sidevoice MCP server that is not this package's or is registered outside user scope (${current.line}); remove it where it was added.`);
  } else done.push('Claude Code had no sidevoice MCP server registered.');
}

/** Codex keeps one machine-wide file that may hold anything its user put there: we never rewrite it. */
export function codexInstructions(env = process.env, record = selected(env)) {
  const { command, args } = registration(record);
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

/* ----- Cursor ----- */

/** Where Cursor reads its user-wide MCP servers — the CLI and the editor alike: `~/.cursor/mcp.json`. */
export function cursorMcpFile(env = process.env) {
  return path.join(env.HOME || os.homedir(), '.cursor', 'mcp.json');
}

/** An entry this package wrote. Anything else is the person's. */
export function oursInCursor(entry, env = process.env) { return oursEntry(entry, env); }

/** Replace a file the person owns without changing what it is: through a symlink to where it really lives,
 *  and with the permissions it had — an mcp.json often holds tokens. */
function rewriteKept(file, text) {
  let target = file, mode = 0o600;
  try { target = realpathSync(file); mode = statSync(target).mode & 0o777; } catch {}
  mkdirSync(path.dirname(target), { recursive: true });
  const temporary = target + '.' + process.pid + '.tmp';
  writeFileSync(temporary, text, { mode });
  renameSync(temporary, target);
}

/** Cursor's `mcp.json`, parsed: `{config}` (empty when there is no file), or `{invalid}` when it is not a JSON object. */
function cursorConfig(env) {
  const file = cursorMcpFile(env);
  if (!existsSync(file)) return { config: {} };
  let config;
  try { config = JSON.parse(readFileSync(file, 'utf8')); } catch { return { invalid: 'not valid JSON' }; }
  if (!config || typeof config !== 'object' || Array.isArray(config)) return { invalid: 'not a JSON object' };
  return { config };
}

/** Cursor's entry, judged: `absent`, `ours`, `foreign` — or `invalid` (a file we cannot read is never written). */
export function cursorState(env = process.env) {
  const { config, invalid } = cursorConfig(env);
  if (invalid) return { state: 'invalid', why: invalid };
  const current = config.mcpServers?.sidevoice;
  if (!current) return { state: 'absent' };
  return { state: oursEntry(current, env) ? 'ours' : 'foreign', entry: current };
}

/** Make Cursor run this installation: our one key written, the rest of the file kept. */
export function setCursor(env, record) {
  const { command, args } = registration(record);
  const { config, invalid } = cursorConfig(env);
  if (invalid) return 'invalid';
  const servers = config.mcpServers && typeof config.mcpServers === 'object' ? config.mcpServers : {};
  const current = servers.sidevoice;
  if (current && !oursEntry(current, env)) return 'foreign';
  if (current && current.command === command && JSON.stringify(current.args) === JSON.stringify(args)) return 'unchanged';
  config.mcpServers = { ...servers, sidevoice: { ...(current || {}), command, args } };
  rewriteKept(cursorMcpFile(env), JSON.stringify(config, null, 2) + '\n');
  return current ? 'repointed' : 'added';
}

/** Take our key out of Cursor's `mcp.json`, leaving everything else as it was. */
export function removeCursor(env) {
  const { config, invalid } = cursorConfig(env);
  if (invalid) return 'invalid';
  const current = config.mcpServers?.sidevoice;
  if (!current) return 'absent';
  if (!oursEntry(current, env)) return 'foreign';
  delete config.mcpServers.sidevoice;
  rewriteKept(cursorMcpFile(env), JSON.stringify(config, null, 2) + '\n');
  return 'removed';
}

/** `install`'s words for Cursor (and what a test of the old entry point still calls): set it, and say what happened. */
export function registerWithCursor(done, env = process.env, record = selected(env)) {
  const outcome = setCursor(env, record);
  const file = cursorMcpFile(env);
  const { command, args } = registration(record);
  const manual = `Add to ${file}:\n\n  { "mcpServers": { "sidevoice": { "command": "${command}", "args": [${args.map(a => `"${a}"`).join(', ')}] } } }`;
  if (outcome === 'invalid') done.push(t('cursor.invalid', { file, why: cursorState(env).why, manual }));
  else if (outcome === 'foreign') done.push(t('cursor.foreign', { file, manual }));
  else if (outcome === 'unchanged') done.push('Cursor already runs this version of the MCP server.');
  else done.push(outcome === 'repointed' ? `Re-pointed Cursor's MCP server to this version in ${file}.` : `Registered the MCP server with Cursor in ${file}.`);
  return outcome;
}

export function cursorHasOurs(env = process.env) {
  return cursorState(env).state === 'ours';
}

/** `uninstall`'s words for Cursor. */
export function unregisterFromCursor(done, next, env = process.env) {
  const file = cursorMcpFile(env);
  const outcome = removeCursor(env);
  if (outcome === 'removed') done.push(`Unregistered the MCP server from Cursor (${file}).`);
  else if (outcome === 'foreign') next.push(`Cursor has a sidevoice MCP server this package did not write in ${file}; remove it there if you want it gone.`);
  else if (outcome === 'invalid') next.push(t('cursor.unreadable', { file }));
  else done.push('Cursor had no sidevoice MCP server registered.');
}

/** The harnesses whose registration an installation owns, with their operations. */
export const HARNESS_REGISTRATIONS = {
  claude: { state: claudeState, set: setClaude, remove: removeClaude, reachable: claudeReachable },
  cursor: { state: cursorState, set: setCursor, remove: removeCursor, reachable: () => true },
};
