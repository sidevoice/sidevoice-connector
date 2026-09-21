#!/usr/bin/env node
/** `sidevoice install <room-url>` — the whole onboarding, in one command.
 *
 *  Everything a machine needs to talk to a room is mechanical: pair with the room, register the MCP
 *  server with the harness, install the skill. This does all three, says what it changed, and is safe
 *  to run twice. What it does not do is decide for the person: it never edits a machine-wide Codex
 *  configuration it does not own, and it never relaxes Claude Code's inbound safeguard — those it
 *  prints, with the reason, for a human to apply.
 */
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { dataDir, pair, requestCode } from './pair.mjs';
import { install as installSkill, skillsDir, status as skillStatus } from './skill.mjs';

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

function alreadyPaired(env = process.env) {
  try { return !!JSON.parse(readFileSync(path.join(dataDir(env), 'credentials.json'), 'utf8')).token; }
  catch { return false; }
}

function claudeRegistered() {
  try { return execFileSync('claude', ['mcp', 'list'], { encoding: 'utf8', timeout: 15_000 }).includes('sidevoice'); }
  catch { return null; }   // no claude on PATH: not an error, just unknown
}

function registerWithClaude(done) {
  const { command, args } = serverCommand();
  if (claudeRegistered()) { done.push('Claude Code already had the MCP server registered.'); return; }
  try {
    execFileSync('claude', ['mcp', 'add', '--scope', 'user', 'sidevoice', '--', command, ...args],
                 { encoding: 'utf8', timeout: 30_000 });
    done.push('Registered the MCP server with Claude Code (user scope).');
  } catch (error) {
    done.push(`Could not register with Claude Code automatically (${(error.message || '').split('\n')[0]}). Run:\n` +
              `    claude mcp add --scope user sidevoice -- ${command} ${args.join(' ')}`);
  }
}

/** Codex keeps one machine-wide file that may hold anything its user put there: we never rewrite it. */
export function codexInstructions() {
  const { command, args } = serverCommand();
  const hook = [command, ...args.slice(0, -1), 'hook', '--harness', 'codex'].join(' ');
  return [
    `Add to ${process.env.CODEX_HOME || path.join(os.homedir(), '.codex')}/config.toml — it is machine-wide and`,
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
  const room = argv.find(item => !item.startsWith('-')) || null;
  const wanted = flag(argv, '--harness');
  const harnesses = wanted ? [wanted] : harnessesPresent(env);
  const done = [], next = [];

  if (alreadyPaired(env) && !argv.includes('--repair')) {
    done.push(`Already paired; credential in ${dataDir(env)}. Use --repair to pair again.`);
  } else if (!room) {
    throw new Error('usage: sidevoice install <room-url> [--code <pairing-code>] [--harness claude|codex] [--repair]');
  } else {
    const code = flag(argv, '--code') || await requestCode(room);
    const result = await pair(room, code, env);
    done.push(`Paired with ${result.origin} as connector ${result.connector_id}.`);
  }

  if (harnesses.includes('claude')) {
    registerWithClaude(done);
    const outcome = installSkill(skillsDir([], env));
    done.push(`Skill ${outcome.action} at ${outcome.target}.`);
    next.push('In a conversation, run /voice-room to join the room and register this session\'s hooks.');
    next.push('Sessions already open need a restart before they can see the skill.');
    const warning = inboundWarning(env);
    if (warning) next.push(warning);
  }
  if (harnesses.includes('codex')) next.push(codexInstructions());
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
