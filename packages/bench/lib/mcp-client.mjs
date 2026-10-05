// A minimal MCP client over stdio (newline-delimited JSON-RPC), standing in for the harness that starts the
// connector's `mcp` server. It is only what the tests need: initialize, tools/list, tools/call.

import { spawn } from 'node:child_process';
import readline from 'node:readline';

export class McpClient {
  constructor(binary, args, { env = process.env, clientInfo = { name: 'sidevoice-bench', version: '0' } } = {}) {
    this.child = spawn(binary, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
    this.clientInfo = clientInfo;
    this.serial = 0;
    this.pending = new Map();
    this.stderr = '';
    this.child.stderr.setEncoding('utf8').on('data', (text) => (this.stderr += text));
    readline.createInterface({ input: this.child.stdout }).on('line', (line) => {
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        return;
      }
      const waiter = this.pending.get(frame.id);
      if (!waiter) return;
      this.pending.delete(frame.id);
      if (frame.error) waiter.reject(new Error(JSON.stringify(frame.error)));
      else waiter.resolve(frame.result);
    });
    this.exited = new Promise((resolve) => this.child.once('exit', resolve));
  }

  request(method, params = {}, timeoutMs = 20_000) {
    const id = ++this.serial;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`MCP ${method} timed out; stderr: ${this.stderr}`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => (clearTimeout(timer), resolve(value)),
        reject: (error) => (clearTimeout(timer), reject(error)),
      });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  }

  notify(method, params = {}) {
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method, params })}\n`);
  }

  async initialize() {
    const result = await this.request('initialize', {
      protocolVersion: '2025-06-18',
      capabilities: {},
      clientInfo: this.clientInfo,
    });
    this.notify('notifications/initialized');
    return result;
  }

  /** Calls a tool and returns its JSON payload (the first text content, parsed when it is JSON). */
  async call(name, args = {}, meta) {
    const result = await this.request('tools/call', { name, arguments: args, ...(meta ? { _meta: meta } : {}) });
    const text = result?.content?.find((item) => item.type === 'text')?.text;
    let payload = text;
    try {
      payload = JSON.parse(text);
    } catch {
      // plain text
    }
    return { isError: Boolean(result?.isError), payload, raw: result };
  }

  async close() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill('SIGKILL'), 3000);
    await this.exited;
    clearTimeout(timer);
  }
}
