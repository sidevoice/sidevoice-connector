/** Shared host-side agent discovery helpers. Harness-specific registration behavior lives with each harness. */
import { execFileSync } from 'node:child_process';
import { constants, existsSync, accessSync, realpathSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { BUILD_PACKAGE_DIR } from './build-info.mjs';
import { recordedInstallation } from './node-files.mjs';
import { selectedMcpCommand, selection } from './release.mjs';
import { RUST_CONNECTOR_KIND } from './rust-connector.mjs';
import { runningAsSea } from './sea-runtime.mjs';
import { VERSION } from './identity.mjs';

const BINARY_NAMES = Object.freeze({
  claude: ['claude'],
  codex: ['codex'],
  cursor: ['cursor-agent', 'cursor'],
});

const OVERRIDE_NAMES = Object.freeze({ claude: 'SIDEVOICE_CLAUDE_BIN', codex: 'SIDEVOICE_CODEX_BIN', cursor: 'SIDEVOICE_CURSOR_BIN' });

function home(env) { return env.HOME || os.homedir(); }

/** Keep each local agent CLI probe short enough for the core's bounded host request. */
export function agentTimeout(env = process.env) {
  const requested = Number(env.SIDEVOICE_AGENT_TIMEOUT_MS || 2500);
  return Number.isFinite(requested) ? Math.min(2500, Math.max(100, requested)) : 2500;
}

/** Capture PATH from the user's login shell. Service PATH is intentionally not consulted. */
export function captureLoginPath(env = process.env) {
  // A GUI/service environment may not carry SHELL; its PATH is not a safe substitute for the login PATH.
  const shell = env.SHELL;
  if (!shell || !path.isAbsolute(shell)) return null;
  try {
    const output = execFileSync(shell, ['-lc', 'printf %s "$PATH"'], {
      encoding: 'utf8', timeout: 3000, maxBuffer: 1024 * 1024, env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'ignore'],
    });
    const value = output.trim();
    return value || null;
  } catch { return null; }
}

function knownBinaries(id, env) {
  const root = home(env);
  const local = env.LOCALAPPDATA || path.join(root, 'AppData', 'Local');
  const homeLocal = path.join(root, '.local', 'bin');
  const paths = {
    claude: [path.join(root, '.claude', 'local', 'claude'), path.join(homeLocal, 'claude'), '/usr/local/bin/claude', '/opt/homebrew/bin/claude'],
    codex: [path.join(homeLocal, 'codex'), '/usr/local/bin/codex', '/opt/homebrew/bin/codex', path.join(local, 'Programs', 'Codex', 'codex.exe')],
    cursor: [path.join(homeLocal, 'cursor-agent'), path.join(homeLocal, 'cursor'), '/usr/local/bin/cursor-agent', '/opt/homebrew/bin/cursor-agent',
      '/usr/local/bin/cursor', '/opt/homebrew/bin/cursor', '/Applications/Cursor.app/Contents/Resources/app/bin/cursor',
      path.join(local, 'Programs', 'cursor', 'resources', 'app', 'bin', 'cursor.cmd')],
  };
  return paths[id] || [];
}

function executable(file) {
  if (!file || !path.isAbsolute(file)) return false;
  try { accessSync(file, process.platform === 'win32' ? constants.F_OK : constants.X_OK); return true; } catch { return false; }
}

function fromPath(names, value) {
  if (!value) return null;
  for (const directory of value.split(path.delimiter).filter(Boolean)) {
    for (const name of names) {
      const candidate = path.join(directory, name);
      if (executable(candidate)) return candidate;
    }
  }
  return null;
}

/** Resolve an agent executable using explicit test/user overrides, the captured login PATH, then known locations. */
export function resolveAgentBinary(id, env = process.env, { loginPath = null, binaries = {} } = {}) {
  const names = BINARY_NAMES[id] || [];
  const override = env[OVERRIDE_NAMES[id]];
  if (override && path.isAbsolute(override) && executable(override)) return override;
  const saved = binaries[id];
  if (saved && executable(saved)) return saved;
  const fromLogin = fromPath(names, loginPath);
  if (fromLogin) return fromLogin;
  return knownBinaries(id, env).find(executable) || null;
}

/** Run a known, absolute executable with a short bound and return its first useful version line. */
export function executableVersion(binary, args = ['--version'], env = process.env) {
  if (!binary || !path.isAbsolute(binary)) return null;
  try {
    const output = execFileSync(binary, args, { encoding: 'utf8', timeout: agentTimeout(env), maxBuffer: 1024 * 1024,
      env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    return output.trim().split(/\r?\n/).map(line => line.trim()).find(Boolean) || null;
  } catch { return null; }
}

/** The exact installed command and selected connector version, for manual MCP configuration. */
export function connectorMcpCommand(env = process.env) {
  let command, args;
  const selected = selection(env, 'current')?.release;
  if (selected?.runtime_kind === RUST_CONNECTOR_KIND) {
    [command, ...args] = selectedMcpCommand(env);
    return { command, args, version: selected.connector || VERSION };
  }
  let record = null;
  try { record = recordedInstallation(env); } catch {}
  const installedCommand = record?.command;
  if (Array.isArray(installedCommand) && installedCommand.length === 1
      && typeof installedCommand[0] === 'string' && path.isAbsolute(installedCommand[0])) {
    command = installedCommand[0]; args = ['mcp'];
  } else if (Array.isArray(installedCommand) && installedCommand.length === 2
      && installedCommand.every(value => typeof value === 'string' && path.isAbsolute(value))) {
    const [program, cli] = installedCommand;
    command = program; args = [cli, 'mcp'];
  } else if (runningAsSea() && path.isAbsolute(process.execPath)) {
    // A SEA can provide its own real executable even before install.json exists.
    command = process.execPath; args = ['mcp'];
  } else {
    // When invoked from an unpacked package before install, describe the actual running CLI if available.
    const running = process.argv[1];
    let cli = running && path.isAbsolute(running) && /(?:^|[\\/])cli\.mjs$/.test(running) && existsSync(running) ? running : null;
    if (!cli) {
      const packaged = path.join(BUILD_PACKAGE_DIR, 'cli.mjs');
      if (existsSync(packaged)) cli = packaged;
    }
    if (!cli) {
      const error = new Error('Sidevoice has no installed or running CLI command.');
      error.key = 'agents.connector-not-installed';
      throw error;
    }
    try { cli = realpathSync(cli); } catch {}
    command = process.execPath; args = [cli, 'mcp'];
  }
  let version = VERSION;
  try { version = selection(env, 'current')?.release?.connector || VERSION; } catch {}
  return { command, args, version };
}

export function quoteShell(value) {
  return `'${String(value).replace(/'/g, `'\\''`)}'`;
}

export function shellCommand(command, args) {
  return [command, ...args].map(quoteShell).join(' ');
}

export function mcpJson(command, args) {
  return JSON.stringify({ mcpServers: { sidevoice: { command, args } } }, null, 2);
}

export function agentConfigPath(id, env = process.env) {
  const root = home(env);
  if (id === 'claude') return env.CLAUDE_CONFIG_DIR || path.join(root, '.claude');
  if (id === 'codex') return env.CODEX_HOME || path.join(root, '.codex');
  if (id === 'cursor') return env.CURSOR_CONFIG_DIR || path.join(root, '.cursor');
  return null;
}

export function agentEvidence(id, env = process.env, binary = null) {
  const result = [];
  const config = agentConfigPath(id, env);
  if (config && existsSync(config)) result.push({ kind: 'config-dir', path: config });
  if (binary) result.push({ kind: 'binary', path: binary });
  if (id === 'cursor') {
    const app = '/Applications/Cursor.app';
    if (existsSync(app) && !result.some(item => item.path === app)) result.push({ kind: 'app', path: app });
  }
  return result;
}
