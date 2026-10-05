// The real Rust connector (`mcp` and `connector`) against the fake Core, with each harness's delivery route
// faked: no model, no real agent, no real configuration. For each route the whole loop must close:
// voice_connect → binding.register → input.deliver → the harness receives it → input.read / working state →
// voice_say → speech.publish → voice_disconnect → the binding leaves.
//
// Skipped without a built binary; BENCH_REQUIRE_CONNECTOR=1 (CI) makes that a failure instead.

import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { afterEach, describe, test } from 'node:test';
import { cleanEnv } from '../lib/connector.mjs';
import { McpClient } from '../lib/mcp-client.mjs';
import { claudeFake, codexFake, httpReceiver } from './harness-fakes.mjs';
import { connectorBinary, startConnector, startCore, waitFor } from './helpers.mjs';

const cleanups = [];
afterEach(async () => {
  while (cleanups.length) await cleanups.pop()();
});

/** Core, connector and an MCP client for one conversation, all in a fresh profile. */
async function rig(t, { mcpEnv = {}, connectorEnv = {}, before } = {}) {
  const binary = connectorBinary(t);
  if (!binary) return null;
  const env = await startCore();
  cleanups.push(env.cleanup);
  const extra = before ? await before(env) : {};
  const supervisor = await startConnector(binary, env.root, env.core, { ...connectorEnv, ...extra.connectorEnv });
  cleanups.push(() => supervisor.stop());
  const mcp = new McpClient(binary, ['--profile-root', env.root, 'mcp'], {
    env: cleanEnv({ ...mcpEnv, ...extra.mcpEnv }),
  });
  cleanups.push(() => mcp.close());
  const init = await mcp.initialize();
  assert.match(init.instructions, /voice/i, 'the MCP server carries its instructions');
  return { ...env, ...extra, supervisor, mcp, binary };
}

async function join(r, thread) {
  const { payload, isError } = await r.mcp.call('voice_connect', { title: `bench ${thread}` });
  assert.equal(isError, false, JSON.stringify(payload));
  assert.equal(payload.status, 'joined', JSON.stringify(payload));
  const binding = await waitFor(
    () => r.core.snapshot().bindings.find((b) => b.thread === thread && b.live),
    'binding.register at Core',
  );
  assert.equal(binding.binding_id, payload.binding_id);
  return { payload, binding };
}

async function speakBack(r, message, thread) {
  const { payload, isError } = await r.mcp.call('voice_say', {
    text: `reply to ${message.text}`,
    session_id: message.session_id,
    revision: message.revision,
    language: 'en',
  });
  assert.equal(isError, false, JSON.stringify(payload));
  const spoken = await waitFor(() => r.core.speech.find((s) => s.thread === thread), 'speech.publish at Core');
  assert.equal(spoken.text, `reply to ${message.text}`);
  assert.equal(spoken.revision, message.revision);
  assert.equal(spoken.session_id, message.session_id);
}

async function leave(r, binding) {
  const { isError } = await r.mcp.call('voice_disconnect', {});
  assert.equal(isError, false);
  await waitFor(() => !r.core.bindings.get(binding.binding_id).live, 'binding.unregister at Core');
}

describe('harness connectivity (real connector, fake Core, fake harness)', { timeout: 90_000 }, () => {
  test('link: the connector proves it linked with protocol 3', async (t) => {
    const r = await rig(t);
    if (!r) return;
    const proof = JSON.parse(readFileSync(path.join(r.paths.data, 'proof.json'), 'utf8'));
    assert.equal(proof.protocol, 3);
    assert.equal(proof.core_launch_id, r.core.launchId);
    const status = await r.core.request('node.status');
    assert.equal(status.core.launch_id, r.core.launchId);
  });

  test('http: SIDEVOICE_DELIVERY_URL receives voice; capabilities say what it cannot do', async (t) => {
    const receiver = await httpReceiver();
    cleanups.push(() => receiver.close());
    const r = await rig(t, {
      mcpEnv: { SIDEVOICE_THREAD: 'http-thread', SIDEVOICE_DELIVERY_URL: receiver.url, SIDEVOICE_HARNESS: 'http' },
    });
    if (!r) return;
    const { binding, payload } = await join(r, 'http-thread');
    assert.equal(payload.harness, 'http');
    assert.equal(binding.input_mode, 'push');
    assert.deepEqual(binding.capabilities, {
      deliver: 'supported',
      inspectInbound: 'unsupported',
      working: 'unsupported',
      endOfTurn: 'unsupported',
      sessionIdentity: 'supported',
    });
    const message = r.core.say('http-thread', 'hello over http');
    await waitFor(() => message.status === 'delivered', 'delivered receipt');
    const [delivery] = receiver.received;
    assert.equal(delivery.text, 'hello over http');
    assert.equal(delivery.message_id, message.message_id);
    assert.equal(delivery.thread_id, 'http-thread');
    await speakBack(r, message, 'http-thread');
    await leave(r, binding);
  });

  test('claude-uds: the messaging socket receives the envelope; the transcript makes it read', async (t) => {
    const sessionId = randomUUID();
    const r = await rig(t, {
      before: async (env) => {
        const claude = await claudeFake(env.paths.bench, { sessionId });
        cleanups.push(() => claude.close());
        const shared = { CLAUDE_CONFIG_DIR: claude.env.CLAUDE_CONFIG_DIR };
        return { claude, mcpEnv: claude.env, connectorEnv: shared };
      },
    });
    if (!r) return;
    const { binding, payload } = await join(r, sessionId);
    assert.equal(payload.harness, 'claude');
    assert.equal(binding.harness, 'claude');
    assert.equal(binding.delivery.kind, 'claude-uds');
    assert.equal(binding.capabilities.inspectInbound, 'supported');
    assert.equal(binding.inbound?.ok, true);
    const message = r.core.say(sessionId, 'hello claude');
    await waitFor(() => r.claude.received.length, 'the messaging socket to receive');
    const [content] = r.claude.received;
    const header = JSON.parse(content.slice(content.indexOf('{'), content.indexOf('}') + 1));
    assert.deepEqual(header, {
      channel: 'voice',
      session_id: message.session_id,
      revision: message.revision,
      message_id: message.message_id,
    });
    assert.ok(content.includes('hello claude'));
    await waitFor(() => message.status === 'read', 'read receipt from the transcript');
    r.claude.setStatus('busy');
    await waitFor(() => r.core.bindings.get(binding.binding_id).working === true, 'working: true');
    r.claude.setStatus('idle');
    await waitFor(() => r.core.bindings.get(binding.binding_id).working === false, 'working: false');
    await speakBack(r, message, sessionId);
    await leave(r, binding);
  });

  test('codex-queue: `codex queue` gets the thread and envelope; the rollout makes it read and working', async (t) => {
    const threadId = randomUUID();
    let codex;
    const r = await rig(t, {
      mcpEnv: { CODEX_THREAD_ID: threadId },
      before: async (env) => {
        codex = codexFake(env.paths.bench, { threadId, codexHome: env.paths.codex });
        return { connectorEnv: { SIDEVOICE_CODEX_BIN: codex.bin } };
      },
    });
    if (!r) return;
    const { binding, payload } = await join(r, threadId);
    assert.equal(payload.harness, 'codex');
    assert.equal(binding.delivery.kind, 'codex-queue');
    assert.equal(binding.capabilities.working, 'supported');
    const turns = [];
    r.core.on('state', () => {
      const working = r.core.bindings.get(binding.binding_id)?.working;
      if (turns.at(-1) !== working) turns.push(working);
    });
    const message = r.core.say(threadId, 'hello codex');
    await waitFor(() => message.status === 'read', 'read receipt from the rollout');
    const [call] = codex.calls();
    assert.deepEqual(call.args.slice(0, 4), ['queue', '--thread', threadId, '--message']);
    assert.ok(call.args[4].includes(message.message_id));
    assert.equal(call.codexHome, r.paths.codex, 'delivery uses the profile CODEX_HOME, never the real one');
    await waitFor(() => turns.includes(true) && turns.at(-1) === false, `working true then false (saw ${turns})`);
    await waitFor(() => r.core.bindings.get(binding.binding_id).engine?.model === 'bench-model', 'engine');
    await speakBack(r, message, threadId);
    await leave(r, binding);
  });

  test('outbox: speech Core refused is kept and replayed when the link comes back', async (t) => {
    const receiver = await httpReceiver();
    cleanups.push(() => receiver.close());
    const r = await rig(t, { mcpEnv: { SIDEVOICE_THREAD: 'outbox', SIDEVOICE_DELIVERY_URL: receiver.url } });
    if (!r) return;
    const { binding } = await join(r, 'outbox');
    const message = r.core.say('outbox', 'say something');
    await waitFor(() => message.status === 'delivered', 'delivered');
    r.core.setPolicy('speech.publish', 'error');
    const { payload } = await r.mcp.call('voice_say', {
      text: 'kept for later',
      session_id: message.session_id,
      revision: message.revision,
    });
    assert.equal(payload.status, 'queued', JSON.stringify(payload));
    assert.equal(r.core.speech.length, 0);
    const outbox = JSON.parse(readFileSync(path.join(r.paths.data, 'outbox.json'), 'utf8'));
    assert.equal(outbox.length, 1, 'the speech is on disk');
    r.core.setPolicy('speech.publish', 'normal');
    r.core.dropLink();
    await waitFor(() => r.core.speech.find((s) => s.text === 'kept for later'), 'the replayed speech');
    await waitFor(() => r.core.bindings.get(binding.binding_id)?.live, 'the binding re-registered with its id');
    await waitFor(
      () => JSON.parse(readFileSync(path.join(r.paths.data, 'outbox.json'), 'utf8')).length === 0,
      'the outbox emptied',
    );
  });

  test('connector restart: the façade registers its conversation again by itself', async (t) => {
    const receiver = await httpReceiver();
    cleanups.push(() => receiver.close());
    const r = await rig(t, { mcpEnv: { SIDEVOICE_THREAD: 'restart', SIDEVOICE_DELIVERY_URL: receiver.url } });
    if (!r) return;
    await join(r, 'restart');
    await r.supervisor.stop();
    await waitFor(() => !r.core.peer, 'the link to drop');
    r.supervisor.start();
    await waitFor(() => r.core.snapshot().bindings.find((b) => b.thread === 'restart' && b.live), 're-registered');
    const message = r.core.say('restart', 'after the restart');
    await waitFor(() => message.status === 'delivered', 'delivered after the restart', 20_000);
    assert.equal(receiver.received.at(-1).text, 'after the restart');
  });

  test('revoked pairing: the room refusing this machine closes its conversations', async (t) => {
    const receiver = await httpReceiver();
    cleanups.push(() => receiver.close());
    const r = await rig(t, { mcpEnv: { SIDEVOICE_THREAD: 'revoked', SIDEVOICE_DELIVERY_URL: receiver.url } });
    if (!r) return;
    await join(r, 'revoked');
    r.core.setRendezvous({ connected: false, refused: 'connector_revoked' });
    await waitFor(async () => {
      const { payload } = await r.mcp.call('voice_status', {});
      return JSON.stringify(payload).includes('connector_revoked');
    }, 'voice_status to report the revocation');
  });

  test('room close: binding.close from Core ends the conversation for the agent', async (t) => {
    const receiver = await httpReceiver();
    cleanups.push(() => receiver.close());
    const r = await rig(t, { mcpEnv: { SIDEVOICE_THREAD: 'closing', SIDEVOICE_DELIVERY_URL: receiver.url } });
    if (!r) return;
    const { binding } = await join(r, 'closing');
    r.core.closeBinding(binding.binding_id);
    await waitFor(async () => {
      const { payload } = await r.mcp.call('voice_status', {});
      return JSON.stringify(payload).includes('closed_from_room');
    }, 'voice_status to report the room closed it');
  });
});
