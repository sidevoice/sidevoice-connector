/** What this machine says about itself, in the one place that says it.
 *
 *  It is said twice — when the machine is paired, and in every handshake afterwards — and the room
 *  keeps the latest, so a person looking at the list of paired machines reads a machine and not a
 *  UUID. Every field is read from what this machine actually is: nothing is asked of a harness,
 *  nothing is asked of the room, and nothing is guessed. */
import os from 'node:os';
import path from 'node:path';
import { existsSync, readFileSync } from 'node:fs';

/** This package's version, from the `package.json` beside these modules — true of the checkout and
 *  of the bundle alike, which is why the build copies that file next to it. */
export const VERSION = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')).version;

/** Which harnesses live on this machine, by the home each of them keeps. Nothing is run to find out. */
export function harnessesPresent(env = process.env) {
  const found = [];
  if (existsSync(env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'))) found.push('claude');
  if (existsSync(env.CODEX_HOME || path.join(os.homedir(), '.codex'))) found.push('codex');
  return found;
}

/** This machine, as the room will list it. */
export function machineIdentity(env = process.env) {
  return {
    host: env.SIDEVOICE_HOST_ID || os.hostname(),
    platform: `${os.platform()} ${os.arch()}`,
    version: VERSION,
    harnesses: harnessesPresent(env),
  };
}
