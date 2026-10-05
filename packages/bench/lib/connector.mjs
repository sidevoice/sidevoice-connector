// Finds and runs the real Rust connector against the bench. Unmanaged, the connector leaves 15 s after its last
// conversation does (connector-rust src/daemon.rs), so the supervisor starts it again: an agent's MCP server can
// only reach a connector that is already running.

import { spawn } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { accessSync, constants, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const RUST_PACKAGE = path.resolve(HERE, '../../connector-rust');
const BINARY = 'sidevoice-rust-proof';

/** The newest built binary under packages/connector-rust/target, or null. */
export function findConnector() {
  const candidates = ['release', 'debug'].map((profile) => path.join(RUST_PACKAGE, 'target', profile, BINARY));
  let best = null;
  for (const candidate of candidates) {
    try {
      accessSync(candidate, constants.X_OK);
      const mtime = statSync(candidate).mtimeMs;
      if (!best || mtime > best.mtime) best = { path: candidate, mtime };
    } catch {
      // not built
    }
  }
  return best?.path ?? null;
}

const LOG_LIMIT = 400;

export class ConnectorSupervisor extends EventEmitter {
  constructor({ binary, profileRoot, env = {} }) {
    super();
    this.binary = binary;
    this.profileRoot = profileRoot;
    this.env = env;
    this.child = null;
    this.wanted = false;
    this.restarts = 0;
    this.log = [];
    this.timer = null;
  }

  start() {
    this.wanted = true;
    this.#spawn();
  }

  async stop() {
    this.wanted = false;
    clearTimeout(this.timer);
    const child = this.child;
    if (!child) return;
    await new Promise((resolve) => {
      child.once('exit', resolve);
      child.kill('SIGTERM');
      setTimeout(() => child.kill('SIGKILL'), 3000).unref();
    });
  }

  status() {
    return {
      binary: this.binary,
      running: Boolean(this.child),
      pid: this.child?.pid ?? null,
      wanted: this.wanted,
      restarts: this.restarts,
    };
  }

  #line(stream, text) {
    for (const line of text.split('\n')) {
      if (!line.trim()) continue;
      const entry = { at: Date.now(), stream, line };
      this.log.push(entry);
      if (this.log.length > LOG_LIMIT) this.log.shift();
      this.emit('log', entry);
    }
  }

  #spawn() {
    if (this.child || !this.wanted) return;
    const child = spawn(this.binary, ['--profile-root', this.profileRoot, 'connector'], {
      env: cleanEnv(this.env),
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    this.child = child;
    this.#line('bench', `started ${this.binary} (pid ${child.pid})`);
    child.stdout.setEncoding('utf8').on('data', (text) => this.#line('stdout', text));
    child.stderr.setEncoding('utf8').on('data', (text) => this.#line('stderr', text));
    child.on('error', (error) => this.#line('bench', `spawn failed: ${error.message}`));
    child.on('exit', (code, signal) => {
      this.child = null;
      this.#line('bench', `exited (${signal ?? code})`);
      this.emit('status');
      if (this.wanted) {
        this.restarts += 1;
        this.timer = setTimeout(() => this.#spawn(), 1000);
      }
    });
    this.emit('status');
  }
}

// Variables by which a harness tells the connector which conversation it is in. A process started from inside an
// agent (this bench run from Claude Code, say) inherits them; the bench and its tests must not.
const HARNESS_VARIABLES = /^(CLAUDECODE$|CLAUDE_CODE_|CLAUDE_CONFIG_DIR$|CODEX_|CURSOR_|SIDEVOICE_)/;

export function cleanEnv(extra = {}) {
  const env = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (!HARNESS_VARIABLES.test(key)) env[key] = value;
  }
  return { ...env, ...extra };
}
