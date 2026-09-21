/** `sidevoice skill install|remove|status [--dir <skills dir>]`: the Claude Code skill that joins the room
 *  (`/voice-room`). One file; running install again repairs it. A directory of the same name that is not
 *  ours is never touched. */
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
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

export function install(dir) {
  const current = status(dir);
  if (current.state === 'foreign') throw new Error(`${current.target} already holds a skill that is not Sidevoice's; remove or rename it first.`);
  mkdirSync(current.target, { recursive: true });
  writeFileSync(path.join(current.target, 'SKILL.md'), readFileSync(path.join(here, 'skill', SKILL_NAME, 'SKILL.md'), 'utf8'));
  for (const stale of ['hook.mjs', 'harness-contract.mjs', 'harnesses.mjs', 'harness-claude.mjs', 'harness-codex.mjs', 'harness-http.mjs']) {
    rmSync(path.join(current.target, stale), { force: true });   // an older copy carried a hook runtime; it is gone
  }
  return { ...status(dir), action: current.state === 'installed' ? 'updated' : 'installed' };
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
    const result = command === 'install' ? install(dir) : command === 'remove' ? remove(dir) : command === 'status' ? status(dir) : null;
    if (!result) { console.error('usage: sidevoice skill <install|remove|status> [--dir <skills dir>]'); process.exit(2); }
    console.log(`${result.action || result.state}: ${result.target}`);
    if (result.action === 'installed' || result.action === 'updated') {
      console.log('In Claude Code, /voice-room joins the room for that conversation. New sessions see the skill; a session already open needs a restart.');
    }
  } catch (error) { console.error(error.message); process.exit(1); }
}
