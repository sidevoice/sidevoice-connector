/** What this machine says about itself, in the one place that says it.
 *
 *  It is said twice — when the machine is paired, and in every handshake afterwards — and the room
 *  keeps the latest, so a person looking at the list of paired machines reads a machine and not a
 *  UUID. Every field is read from what this machine actually is: nothing is asked of a harness,
 *  nothing is asked of the room, and nothing is guessed. */
import os from 'node:os';
import path from 'node:path';
import { existsSync } from 'node:fs';
import { BUILD_PACKAGE } from './build-info.mjs';

/** The version is compiled into SEA builds and comes from the package in source builds. */
export const VERSION = BUILD_PACKAGE.version;

/** Which harnesses live on this machine, by the home each of them keeps. Nothing is run to find out. */
export function harnessesPresent(env = process.env) {
  const found = [];
  if (existsSync(env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'))) found.push('claude');
  if (existsSync(env.CODEX_HOME || path.join(os.homedir(), '.codex'))) found.push('codex');
  // Cursor, the CLI or the editor: both keep ~/.cursor, and both read the MCP servers in it.
  if (existsSync(path.join(env.HOME || os.homedir(), '.cursor'))) found.push('cursor');
  return found;
}

/** What Node calls this platform, said the way the person reading the room's list calls it. Anything
 *  not named here travels as Node names it: a machine we cannot put a word to is not a machine to
 *  invent one for. */
const PLATFORMS = { darwin: 'macOS', win32: 'Windows', linux: 'Linux' };

/** This machine, as the room will list it. */
export function machineIdentity(env = process.env) {
  return {
    host: env.SIDEVOICE_HOST_ID || os.hostname(),
    platform: `${PLATFORMS[os.platform()] || os.platform()} ${os.arch()}`,
    version: VERSION,
    harnesses: harnessesPresent(env),
  };
}
