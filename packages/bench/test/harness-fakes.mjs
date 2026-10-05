// Stand-ins for the harness side of each delivery route: only what the connector touches, nothing that needs a
// model. Each is the narrowest thing that lets the real connector complete its loop: deliver, observe the read,
// observe working state.

import { appendFileSync, chmodSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import readline from 'node:readline';

/** `SIDEVOICE_DELIVERY_URL`: an HTTP receiver that records each delivery. */
export async function httpReceiver() {
  const received = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => (body += chunk));
    req.on('end', () => {
      received.push(JSON.parse(body));
      res.end('ok');
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}/`, received, close: () => server.close() };
}

/**
 * Claude Code, as the connector sees it (connector-rust src/adapters/claude.rs): a session registry entry under
 * CLAUDE_CONFIG_DIR/sessions (working state), a transcript under CLAUDE_CONFIG_DIR/projects (read receipts), and
 * the messaging socket CLAUDE_CODE_MESSAGING_SOCKET that takes `auth` then `user` lines and, like Claude Code,
 * never answers. What arrives on the socket is appended to the transcript, as Claude Code does when it reads it.
 */
export async function claudeFake(dir, { sessionId, token = 'bench-token' }) {
  const config = path.join(dir, 'claude-config');
  mkdirSync(path.join(config, 'sessions'), { recursive: true });
  mkdirSync(path.join(config, 'projects', 'bench'), { recursive: true });
  const registry = path.join(config, 'sessions', 'bench.json');
  const transcript = path.join(config, 'projects', 'bench', `${sessionId}.jsonl`);
  const setStatus = (status) => writeFileSync(registry, JSON.stringify({ sessionId, status }));
  setStatus('idle');
  writeFileSync(transcript, '');
  const socket = path.join(dir, 'claude-inbox.sock');
  const received = [];
  const server = net.createServer((conn) => {
    let authed = false;
    readline.createInterface({ input: conn }).on('line', (line) => {
      const frame = JSON.parse(line);
      if (frame.type === 'auth') {
        authed = frame.token === token;
        return;
      }
      if (frame.type === 'user' && authed) {
        received.push(frame.message.content);
        appendFileSync(transcript, `${JSON.stringify({ type: 'user', message: frame.message })}\n`);
      }
    });
    conn.on('error', () => {});
  });
  await new Promise((resolve) => server.listen(socket, resolve));
  return {
    env: {
      CLAUDE_CONFIG_DIR: config,
      CLAUDE_CODE_SESSION_ID: sessionId,
      CLAUDE_CODE_MESSAGING_SOCKET: socket,
      CLAUDE_CODE_MESSAGING_TOKEN: token,
    },
    received,
    setStatus,
    close: () => server.close(),
  };
}

/**
 * Codex, as the connector sees it (connector-rust src/adapters/codex.rs, daemon.rs `watch_rollout`): a `codex`
 * executable (SIDEVOICE_CODEX_BIN) whose `queue --thread T --message M` records the call and writes the turn into
 * the thread's rollout under CODEX_HOME/sessions, the way Codex does once it takes the queued message.
 */
export function codexFake(dir, { threadId, codexHome }) {
  const day = path.join(codexHome, 'sessions', '2026', '10', '05');
  mkdirSync(day, { recursive: true });
  const rollout = path.join(day, `rollout-2026-10-05T00-00-00-${threadId}.jsonl`);
  writeFileSync(rollout, `${JSON.stringify({ type: 'session_meta', payload: { id: threadId, model: 'bench-model' } })}\n`);
  const calls = path.join(dir, 'codex-calls.jsonl');
  writeFileSync(calls, '');
  const bin = path.join(dir, 'codex');
  writeFileSync(
    bin,
    `#!${process.execPath}
const fs = require('node:fs');
const args = process.argv.slice(2);
fs.appendFileSync(${JSON.stringify(calls)}, JSON.stringify({ args, codexHome: process.env.CODEX_HOME }) + '\\n');
const at = (flag) => args[args.indexOf(flag) + 1];
if (args[0] !== 'queue' || at('--thread') !== ${JSON.stringify(threadId)}) process.exit(2);
const turn = 'turn-' + Date.now();
const lines = [
  { type: 'event_msg', payload: { type: 'task_started', turn_id: turn } },
  { type: 'response_item', payload: { type: 'message', role: 'user', content: [{ type: 'input_text', text: at('--message') }] } },
  { type: 'event_msg', payload: { type: 'task_complete', turn_id: turn } },
];
setTimeout(() => fs.appendFileSync(${JSON.stringify(rollout)}, lines.map((l) => JSON.stringify(l) + '\\n').join('')), 200);
`,
  );
  chmodSync(bin, 0o755);
  return {
    bin,
    calls: () =>
      readFileSync(calls, 'utf8')
        .split('\n')
        .filter(Boolean)
        .map((line) => JSON.parse(line)),
  };
}
