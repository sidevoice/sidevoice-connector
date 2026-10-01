/** Our MCP server in each harness, and the installation it runs: what `install` registers, what an install
 *  transaction re-points (§4.3), and what `uninstall` takes out — only ever Sidevoice's own entries.
 *
 *  An installation is a record (`install.json`, SEAMS §1): its `command`, the absolute argv prefix that runs this
 *  package's CLI — R1 `node <copy>/dist/cli.mjs`, or a checkout's `cli.mjs`; R4 the single executable — and what
 *  it is (`connector`, `core`, `channel`, `build_seq`). A harness is given that command with `mcp`. */
import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readFileSync, realpathSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CORE_VERSION } from './core.mjs';
import { dataDirOf, nodeFiles, readJson } from './node-files.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
/** Where the package's own root is: next to these modules in the checkout, and one level up once
 *  they have been bundled into `dist/`. Only copying needs to know — everything else reads the
 *  `package.json` beside it, which the build puts there precisely so that this stays the one
 *  place that has to tell the two apart. */
const packageRoot = () => (existsSync(path.join(here, '..', 'package.json')) ? path.dirname(here) : here);
let manifestCache = null;
const manifest = () => (manifestCache ??= JSON.parse(readFileSync(path.join(here, 'package.json'), 'utf8')));

export function fromSource(env = process.env) {
  if (env.SIDEVOICE_INSTALL_FROM_SOURCE === '0') return false;
  return env.SIDEVOICE_INSTALL_FROM_SOURCE === '1' || existsSync(path.join(packageRoot(), '..', '..', '.git'));
}

/** Where installed copies live: one directory per installation, under the XDG data home. */
export function copiesDir(env = process.env) {
  return path.join(env.XDG_DATA_HOME || path.join(env.HOME || os.homedir(), '.local', 'share'), 'sidevoice');
}

/** The installation this package would make: its version, its channel and build (`sidevoice` in the manifest,
 *  stamped by CI; a checkout's is `source`), the copy it would live in and the command that runs it. A nightly
 *  is its own directory (`<version>-nightly.<build_seq>`): it carries the version of the last release. */
export function candidate(env = process.env) {
  const { version, sidevoice = {}, bin } = manifest();
  const source = fromSource(env);
  const channel = source ? 'source' : sidevoice.channel || 'release';
  const build_seq = Number(sidevoice.build_seq) || 0;
  const id = channel === 'nightly' ? `${version}-nightly.${build_seq}` : version;
  const copy = source ? null : path.join(copiesDir(env), id);
  const cli = source ? path.join(here, 'cli.mjs') : path.join(copy, bin.sidevoice.replace(/^\.\//, ''));
  return { id, connector: version, core: CORE_VERSION, channel, build_seq, sha256: null,
    runtime: env.SIDEVOICE_CORE_BIN ? 'external' : 'uv', copy, command: [process.execPath, cli] };
}

/** The selected installation (`install.json`), or — nothing installed yet — the one this package would make. */
export function selected(env = process.env) {
  return readJson(nodeFiles(dataDirOf(env)).install) || candidate(env);
}

/** What a harness runs for an installation: R1 `node <cli.mjs> mcp` — `node` from the harness's own PATH, as
 *  before — and R4 `<executable> mcp`. */
export function registration(record) {
  const [program, cli] = record.command;
  return cli ? { command: 'node', args: [cli, 'mcp'] } : { command: program, args: ['mcp'] };
}

/** What a harness should run to start the server. From a checkout it names that checkout, so a machine that
 *  installed from source keeps working when the published version moves. Otherwise it names a copy of this
 *  package that install placed on disk — never `npx`: a session start is not the moment to resolve a package
 *  (a cold cache, a bin whose name differs from the package's, a 30 s startup budget; one session found no
 *  `sidevoice` binary at all, 2026-09-21). */
export function serverCommand(env = process.env, record = selected(env)) {
  return registration(record);
}

/** This package's files, copied as they are — no install step, nothing fetched — to `<copy>.staging`; the
 *  commit renames it into place (`install-txn.mjs`). */
export function stageCopy(record) {
  if (!record.copy) return null;
  const staging = record.copy + '.staging';
  rmSync(staging, { recursive: true, force: true });
  mkdirSync(staging, { recursive: true });
  for (const file of manifest().files.concat('package.json')) {
    const source = path.join(packageRoot(), file);
    if (existsSync(source)) cpSync(source, path.join(staging, file), { recursive: true });
  }
  return staging;
}

/** A Claude Code entry of ours: a Sidevoice copy's or checkout's `cli.mjs`, the executable, or the package. */
export function oursInClaude(line) {
  return /(^|[\s/])(cli\.mjs|sidevoice) mcp$/.test(line || '') || /@sidevoice\/uplink(@\S+)? mcp$/.test(line || '');
}

/** Point Claude Code's entry at this installation — only when the entry there is ours (§4.3: existing ones are
 *  re-pointed, none is added). What it pointed at before, when it changed it. */
export function repointClaude(env, record) {
  const current = claudeRegistration(env);
  if (!current || current.scope !== 'user' || !oursInClaude(current.line)) return null;
  const { command, args } = registration(record);
  if (current.line === [command, ...args].join(' ')) return null;
  claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env);
  claude(['mcp', 'add', '--scope', 'user', 'sidevoice', '--', command, ...args], env);
  return current.line;
}

/** The same for Cursor's `mcp.json`. */
export function repointCursor(env, record, done = []) {
  if (!cursorHasOurs(env)) return false;
  registerWithCursor(done, env, record);
  return true;
}

/** Take our entry out of Claude Code — only ours, only at user scope. */
export function unregisterFromClaude(done, next, env = process.env) {
  const current = claudeRegistration(env);
  if (current?.scope === 'user' && oursInClaude(current.line)) {
    try { claude(['mcp', 'remove', '--scope', 'user', 'sidevoice'], env); done.push('Unregistered the MCP server from Claude Code.'); }
    catch (error) { done.push(`Could not unregister from Claude Code (${(error.message || '').split('\n')[0]}). Run:\n    claude mcp remove --scope user sidevoice`); }
  } else if (current) {
    next.push(`Claude Code has a sidevoice MCP server that is not this package's or is registered outside user scope (${current.line}); remove it where it was added.`);
  } else done.push('Claude Code had no sidevoice MCP server registered.');
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

export function registerWithClaude(done, env = process.env, record = selected(env)) {
  const { command, args } = registration(record);
  const wanted = [command, ...args].join(' ');
  const manual = `claude mcp add --scope user sidevoice -- ${wanted}`;
  const current = claudeRegistration(env);
  if (current?.line === wanted) { done.push('Claude Code already runs this version of the MCP server.'); return; }
  if (current && current.scope !== 'user') {
    done.push(`Claude Code has a sidevoice MCP server registered outside user scope (${current.line}); not touched. To move it:\n    ${manual}`);
    return;
  }
  if (current && !oursInClaude(current.line)) {
    done.push(`Claude Code has a sidevoice MCP server that is not Sidevoice's (${current.line}); not touched. To replace it:\n    claude mcp remove --scope user sidevoice && ${manual}`);
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

/** Where Cursor reads its user-wide MCP servers — the CLI and the editor alike: `~/.cursor/mcp.json`. */
export function cursorMcpFile(env = process.env) {
  return path.join(env.HOME || os.homedir(), '.cursor', 'mcp.json');
}

/** An entry this package wrote: node running a sidevoice `cli.mjs` as `mcp`. Anything else is the person's. */
export function oursInCursor(entry) {
  return entry?.command === 'node' && Array.isArray(entry.args) && entry.args.length === 2
    && entry.args[1] === 'mcp' && /cli\.mjs$/.test(entry.args[0] || '');
}

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

function cursorManual(env, record) {
  const { command, args } = registration(record);
  return `Add to ${cursorMcpFile(env)}:\n\n  { "mcpServers": { "sidevoice": { "command": "${command}", "args": [${args.map(a => `"${a}"`).join(', ')}] } } }`;
}

/** Register with Cursor by writing our one key into its user-wide `mcp.json`; the rest of the file is kept. */
export function registerWithCursor(done, env = process.env, record = selected(env)) {
  const file = cursorMcpFile(env);
  const { command, args } = registration(record);
  let config = {};
  if (existsSync(file)) {
    try { config = JSON.parse(readFileSync(file, 'utf8')); } catch {
      done.push(`${file} is not valid JSON; not touched. ${cursorManual(env, record)}`); return;
    }
    if (!config || typeof config !== 'object' || Array.isArray(config)) { done.push(`${file} is not a JSON object; not touched. ${cursorManual(env, record)}`); return; }
  }
  const servers = config.mcpServers && typeof config.mcpServers === 'object' ? config.mcpServers : {};
  const current = servers.sidevoice;
  if (current && !oursInCursor(current)) { done.push(`Cursor has a sidevoice MCP server of its own in ${file} (${current.command || current.url || '?'}); not touched. ${cursorManual(env, record)}`); return; }
  if (current && current.args[0] === args[0]) { done.push('Cursor already runs this version of the MCP server.'); return; }
  config.mcpServers = { ...servers, sidevoice: { ...(current || {}), command, args } };
  rewriteKept(file, JSON.stringify(config, null, 2) + '\n');
  done.push(current ? `Re-pointed Cursor's MCP server to this version in ${file} (was: ${current.args.join(' ')}).`
                    : `Registered the MCP server with Cursor in ${file}.`);
}

export function cursorHasOurs(env = process.env) {
  try { return oursInCursor(JSON.parse(readFileSync(cursorMcpFile(env), 'utf8'))?.mcpServers?.sidevoice); } catch { return false; }
}

/** Take our key out of Cursor's `mcp.json`, leaving everything else as it was. */
export function unregisterFromCursor(done, next, env = process.env) {
  const file = cursorMcpFile(env);
  let config;
  try { config = JSON.parse(readFileSync(file, 'utf8')); } catch { done.push('Cursor had no sidevoice MCP server registered.'); return; }
  const current = config?.mcpServers?.sidevoice;
  if (!current) { done.push('Cursor had no sidevoice MCP server registered.'); return; }
  if (!oursInCursor(current)) { next.push(`Cursor has a sidevoice MCP server this package did not write in ${file}; remove it there if you want it gone.`); return; }
  delete config.mcpServers.sidevoice;
  rewriteKept(file, JSON.stringify(config, null, 2) + '\n');
  done.push(`Unregistered the MCP server from Cursor (${file}).`);
}

