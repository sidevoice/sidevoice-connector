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
import { existsSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { pairedRoom } from './pair.mjs';
import { install as installSkill, skillsDir } from './skill.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const VERSION = JSON.parse(readFileSync(path.join(here, 'package.json'), 'utf8')).version;
/** What a harness should run to start the server. From a checkout it names this copy, so a machine that
 *  installed from source keeps working when the published version moves; otherwise the pinned package. */
export function serverCommand(env = process.env) {
  const fromSource = env.SIDEVOICE_INSTALL_FROM_SOURCE === '1' || existsSync(path.join(here, '..', '..', '.git'));
  return fromSource
    ? { command: 'node', args: [path.join(here, 'cli.mjs'), 'mcp'] }
    : { command: 'npx', args: ['-y', `@sidevoice/uplink@${VERSION}`, 'mcp'] };
}

export function flag(argv, name) {
  const index = argv.indexOf(name);
  return index >= 0 ? argv[index + 1] : undefined;
}

/** Which harnesses this machine has, by what they leave behind. */
export function harnessesPresent(env = process.env) {
  const found = [];
  if (existsSync(env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'))) found.push('claude');
  if (existsSync(env.CODEX_HOME || path.join(os.homedir(), '.codex'))) found.push('codex');
  return found;
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
  const hook = [command, ...args.slice(0, -1), 'hook', '--harness', 'codex'].join(' ');
  return [
    `Add to ${env.CODEX_HOME || path.join(os.homedir(), '.codex')}/config.toml — it is machine-wide and`,
    'this package does not rewrite it:',
    '',
    '  [mcp_servers.sidevoice]',
    `  command = "${command}"`,
    `  args = [${args.map(a => `"${a}"`).join(', ')}]`,
    '',
    '  [[hooks.UserPromptSubmit]]',
    `  hooks = [ { type = "command", command = "${hook}" } ]`,
    '',
    '  [[hooks.Stop]]',
    `  hooks = [ { type = "command", command = "${hook}" } ]`,
    '',
    'Then restart Codex. Without the hooks it works, but the room shows no read receipt and no working state.',
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
    `or run  sidevoice pair <room-url> <code>  with the code the room shows under "Emparejar conector".`);
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];

  done.push(`Sidevoice ${VERSION}.`);
  if (harnesses.includes('claude')) {
    registerWithClaude(done, env);
    const outcome = installSkill(skillsDir([], env));
    done.push(`Skill ${outcome.action} at ${outcome.target}.`);
  }

  const paired = pairedRoom(env);
  done.push(paired ? `This machine is paired with ${paired.origin} (connector ${paired.connector_id}).`
                   : 'This machine is not paired with any room yet.');

  if (harnesses.includes('claude')) {
    next.push('In a conversation, run /voice-room to join the room and register this session\'s hooks.' +
              (paired ? '' : ' The first time, the conversation asks you for the room\'s address and the one-time code the room shows under "Emparejar conector".'));
    next.push('Sessions already open need a restart before they can see the skill.');
    const warning = inboundWarning(env);
    if (warning) next.push(warning);
  }
  if (harnesses.includes('codex')) next.push(codexInstructions(env));
  if (!harnesses.length) next.push('No harness found on this machine. Pass --harness claude or --harness codex.');
  return { done, next };
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
