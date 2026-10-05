// The private profile root the Rust connector runs in with `--profile-root` (connector-rust src/proof.rs
// `Profile::from_root`): every directory 0700, owned by this user, nothing of the real installation inside.

import { chmodSync, existsSync, lstatSync, mkdirSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const CHILDREN = [
  'home',
  'claude',
  'codex',
  'cursor',
  'cursor/config',
  'cursor/data',
  'xdg',
  'xdg/config',
  'xdg/data',
  'sidevoice',
  'sidevoice/core',
  'bench',
];

export function defaultProfileRoot() {
  return path.join(os.homedir(), '.sidevoice-bench', 'profile');
}

export function createProfile(root, { reset = false } = {}) {
  if (!path.isAbsolute(root)) throw new Error('profile root must be absolute');
  if (reset) rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true, mode: 0o700 });
  if (lstatSync(root).isSymbolicLink()) throw new Error('profile root cannot be a symlink');
  chmodSync(root, 0o700);
  for (const child of CHILDREN) {
    const dir = path.join(root, child);
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    chmodSync(dir, 0o700);
  }
  if (existsSync(path.join(root, 'sidevoice', 'install.json'))) {
    throw new Error('the profile holds installation data; use another root');
  }
  return profilePaths(root);
}

export function profilePaths(root) {
  return {
    root,
    home: path.join(root, 'home'),
    claude: path.join(root, 'claude'),
    codex: path.join(root, 'codex'),
    cursorConfig: path.join(root, 'cursor', 'config'),
    cursorData: path.join(root, 'cursor', 'data'),
    xdgConfig: path.join(root, 'xdg', 'config'),
    xdgData: path.join(root, 'xdg', 'data'),
    data: path.join(root, 'sidevoice'),
    coreDir: path.join(root, 'sidevoice', 'core'),
    bench: path.join(root, 'bench'),
  };
}

/** The environment `Profile::from_env` checks, for tools that need the profile as variables rather than a flag. */
export function profileEnv(paths) {
  return {
    HOME: paths.home,
    CLAUDE_CONFIG_DIR: paths.claude,
    CODEX_HOME: paths.codex,
    CURSOR_CONFIG_DIR: paths.cursorConfig,
    CURSOR_DATA_DIR: paths.cursorData,
    XDG_CONFIG_HOME: paths.xdgConfig,
    XDG_DATA_HOME: paths.xdgData,
    SIDEVOICE_DATA_DIR: paths.data,
  };
}
