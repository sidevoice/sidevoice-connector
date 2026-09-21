/** `sidevoice skill remove|status [--dir <skills dir>]`: the voice-room skill is no longer installed — the
 *  server carries the same steps as an MCP prompt — but copies from earlier versions are still on disk, and
 *  this takes ours away. A directory of the same name that is not ours is never touched. */
import { existsSync, readFileSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const SKILL_NAME = 'voice-room';
const MARKER = 'sidevoice: installed copy';

export function skillsDir(argv = process.argv.slice(2), env = process.env) {
  const index = argv.indexOf('--dir');
  if (index >= 0 && argv[index + 1]) return path.resolve(argv[index + 1]);
  return path.join(env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'), 'skills');
}

export function status(dir) {
  const target = path.join(dir, SKILL_NAME);
  const manifest = path.join(target, 'SKILL.md');
  if (!existsSync(target)) return { state: 'absent', target };
  let ours = false;
  try { ours = readFileSync(manifest, 'utf8').includes(MARKER); } catch {}
  return { state: ours ? 'installed' : 'foreign', target };
}

export function remove(dir) {
  const current = status(dir);
  if (current.state === 'foreign') throw new Error(`${current.target} is not Sidevoice's skill; left as it is.`);
  if (current.state === 'installed') rmSync(current.target, { recursive: true, force: true });
  return { state: 'absent', target: current.target, action: current.state === 'installed' ? 'removed' : 'nothing to remove' };
}

if (process.env.SIDEVOICE_SKILL_MAIN === '1') {
  const [command] = process.argv.slice(2);
  const dir = skillsDir();
  try {
    const result = command === 'remove' ? remove(dir) : command === 'status' ? status(dir) : null;
    if (!result) { console.error('usage: sidevoice skill <remove|status> [--dir <skills dir>]   (the skill is no longer installed: the MCP server offers the voice-room prompt)'); process.exit(2); }
    console.log(`${result.action || result.state}: ${result.target}`);
  } catch (error) { console.error(error.message); process.exit(1); }
}
