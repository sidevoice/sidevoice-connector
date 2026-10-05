import { mkdtempSync, rmSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import WebSocket from 'ws';
import { ConnectorSupervisor, findConnector } from '../lib/connector.mjs';
import { FakeCore } from '../lib/fake-core.mjs';
import { createProfile } from '../lib/profile.mjs';

export async function waitFor(predicate, description, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${description}`);
}

/** A private profile (short path: Unix socket paths are limited) and a started fake Core in it. */
export async function startCore(options = {}) {
  const root = mkdtempSync('/tmp/svb-');
  const paths = createProfile(root);
  const core = await new FakeCore({ dataDir: paths.coreDir, ...options }).start();
  return {
    root,
    paths,
    core,
    async cleanup() {
      await core.stop();
      rmSync(root, { recursive: true, force: true });
    },
  };
}

/** A raw protocol peer on the fake Core's socket, standing in for a connector. */
export class Peer {
  constructor(socketPath) {
    this.ws = new WebSocket(`ws+unix://${socketPath}:/api/connectors/v3`);
    this.frames = [];
    this.waiters = [];
    this.closed = new Promise((resolve) => this.ws.once('close', (code, reason) => resolve({ code, reason: `${reason}` })));
    this.ws.on('message', (data) => {
      const frame = JSON.parse(data.toString());
      this.frames.push(frame);
      this.waiters = this.waiters.filter((waiter) => !waiter(frame));
    });
    this.opened = new Promise((resolve, reject) => {
      this.ws.once('open', resolve);
      this.ws.once('error', reject);
    });
    this.serial = 0;
  }

  send(frame) {
    this.ws.send(typeof frame === 'string' ? frame : JSON.stringify(frame));
  }

  /** The next frame (already received or still to come) that matches. */
  next(match, timeoutMs = 5000) {
    const seen = this.frames.find(match);
    if (seen) {
      this.frames.splice(this.frames.indexOf(seen), 1);
      return Promise.resolve(seen);
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('no matching frame')), timeoutMs);
      this.waiters.push((frame) => {
        if (!match(frame)) return false;
        clearTimeout(timer);
        this.frames.splice(this.frames.indexOf(frame), 1);
        resolve(frame);
        return true;
      });
    });
  }

  async request(method, params = {}) {
    const id = `c:${++this.serial}`;
    this.send({ jsonrpc: '2.0', id, method, params });
    return this.next((frame) => frame.id === id && !frame.method);
  }

  async hello(core, overrides = {}) {
    await this.opened;
    return this.request('connector.hello', {
      protocol: 3,
      connector_id: core.connectorId,
      token: core.token,
      host: 'test',
      platform: 'test',
      version: '0',
      harnesses: ['http'],
      ...overrides,
    });
  }

  close() {
    this.ws.close();
  }
}

/** The real Rust connector, linked to the core. Skips (or fails, under BENCH_REQUIRE_CONNECTOR=1) without it. */
export function connectorBinary(t) {
  const binary = process.env.SIDEVOICE_BENCH_CONNECTOR || findConnector();
  if (!binary) {
    if (process.env.BENCH_REQUIRE_CONNECTOR === '1') throw new Error('no connector binary built');
    t.skip('no connector binary: cargo build --release in packages/connector-rust');
  }
  return binary;
}

export async function startConnector(binary, root, core, env = {}) {
  const supervisor = new ConnectorSupervisor({ binary, profileRoot: root, env });
  supervisor.start();
  await waitFor(() => core.peer, `the connector to link (log: ${JSON.stringify(supervisor.log)})`);
  return supervisor;
}
