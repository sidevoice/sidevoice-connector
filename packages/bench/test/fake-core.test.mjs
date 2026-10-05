// The fake Core's side of protocol v3, driven by a raw WebSocket peer. Each refusal and shape here mirrors
// sidevoice-core rust/server/connectors_v3.rs and rust/control/room.rs at 0a9da38; no Rust is needed.

import assert from 'node:assert/strict';
import { readFileSync, statSync } from 'node:fs';
import http from 'node:http';
import { afterEach, beforeEach, describe, test } from 'node:test';
import { Peer, startCore, waitFor } from './helpers.mjs';

let env;
let peers;
beforeEach(async () => {
  env = await startCore();
  peers = [];
});
afterEach(async () => {
  for (const peer of peers) peer.close();
  await env.cleanup();
});

function peer() {
  const p = new Peer(env.core.socketPath);
  peers.push(p);
  return p;
}

async function linked() {
  const p = peer();
  const answer = await p.hello(env.core);
  assert.deepEqual(answer.result, { protocol: 3 });
  return p;
}

async function register(p, params = {}) {
  const answer = await p.request('binding.register', { thread: 'thread-1', harness: 'http', title: 'T', ...params });
  return answer.result;
}

describe('discovery', () => {
  test('core.json names the socket and the v3 credential, both private', () => {
    const ready = JSON.parse(readFileSync(env.core.readyPath, 'utf8'));
    assert.equal(ready.socket, env.core.socketPath);
    assert.deepEqual(ready.connector_protocols, [3]);
    assert.equal(ready.pid, process.pid);
    assert.equal(ready.connector_id, env.core.connectorId);
    assert.equal(statSync(env.core.readyPath).mode & 0o777, 0o600);
    assert.equal(statSync(env.core.socketPath).mode & 0o077, 0);
  });

  test('health answers with the launch id and a Content-Length body', async () => {
    const res = await new Promise((resolve, reject) => {
      http.get({ socketPath: env.core.socketPath, path: '/api/local/health' }, resolve).on('error', reject);
    });
    assert.equal(res.statusCode, 200);
    assert.ok(res.headers['content-length']);
    let body = '';
    for await (const chunk of res) body += chunk;
    const health = JSON.parse(body);
    assert.equal(health.launch_id, env.core.launchId);
    assert.equal(health.pid, process.pid);
  });
});

describe('handshake', () => {
  test('welcome and rendezvous follow the hello result, in order', async () => {
    const p = peer();
    await p.hello(env.core);
    const welcome = await p.next((f) => f.method === 'connector.welcome');
    assert.deepEqual(welcome.params, { protocol: 3 });
    const rendezvous = await p.next((f) => f.method === 'node.rendezvous');
    assert.equal(rendezvous.params.connected, true);
    assert.equal(env.core.snapshot().connector.host, 'test');
  });

  test('a wrong credential is refused and closed with 1008', async () => {
    const p = peer();
    const answer = await p.hello(env.core, { token: 'wrong' });
    assert.deepEqual(answer.error, { code: -32001, message: 'Connector credential refused' });
    assert.equal((await p.closed).code, 1008);
  });

  test('another protocol, or another first method, needs a protocol 3 hello', async () => {
    const p = peer();
    const answer = await p.hello(env.core, { protocol: 2 });
    assert.equal(answer.error.code, -32002);
    const q = peer();
    await q.opened;
    const other = await q.request('binding.register', { thread: 'x' });
    assert.equal(other.error.code, -32002);
    assert.equal((await q.closed).code, 1008);
  });

  test('a second hello with the same connector id replaces the first with 1000', async () => {
    const first = await linked();
    await register(first);
    await linked();
    assert.equal((await first.closed).code, 1000);
    assert.equal(env.core.snapshot().bindings[0].live, false);
  });

  test('a malformed frame closes the link with 1002', async () => {
    const p = await linked();
    p.send({ jsonrpc: '1.0', id: 'c:9', method: 'x' });
    assert.equal((await p.closed).code, 1002);
  });

  test('an unknown method is -32601', async () => {
    const p = await linked();
    const answer = await p.request('no.such.method');
    assert.equal(answer.error.code, -32601);
  });
});

describe('bindings', () => {
  test('register answers the binding; capabilities are normalised and unknown is never false', async () => {
    const p = await linked();
    const result = await register(p, {
      client_ref: 'ref',
      capabilities: { deliver: 'supported', working: 'yes' },
      experimental: ['deliver', 'working'],
    });
    assert.equal(result.thread, 'thread-1');
    assert.equal(result.client_ref, 'ref');
    const [binding] = env.core.snapshot().bindings;
    assert.equal(binding.capabilities.deliver, 'supported');
    assert.equal(binding.capabilities.working, 'unknown');
    assert.deepEqual(binding.experimental, ['deliver']);
  });

  test('an invalid thread, mode or foreign binding id is refused in the result', async () => {
    const p = await linked();
    assert.deepEqual(await register(p, { thread: 'bad thread' }), { error: 'room.thread_invalid' });
    assert.deepEqual(await register(p, { input_mode: 'poll' }), { error: 'room.input_mode_invalid' });
    const { binding_id: id } = await register(p);
    assert.deepEqual(await register(p, { thread: 'other', binding_id: id }), { error: 'room.binding_foreign' });
    assert.equal((await register(p, { binding_id: id })).binding_id, id);
  });

  test('working, engine and unregister update what the room shows', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    p.send({ jsonrpc: '2.0', method: 'input.working', params: { binding_id: id, working: true, turn_id: 't1' } });
    p.send({ jsonrpc: '2.0', method: 'input.engine', params: { binding_id: id, engine: { model: 'm', effort: 'high' } } });
    await waitFor(() => env.core.snapshot().bindings[0].engine, 'engine');
    const [binding] = env.core.snapshot().bindings;
    assert.equal(binding.working, true);
    assert.equal(binding.turn.turn_id, 't1');
    assert.deepEqual(binding.engine, { model: 'm', effort: 'high', thinking: null });
    p.send({ jsonrpc: '2.0', method: 'binding.unregister', params: { binding_id: id } });
    await waitFor(() => !env.core.snapshot().bindings[0].live, 'unregister');
  });

  test('closing from the room tells the connector', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    env.core.closeBinding(id);
    const close = await p.next((f) => f.method === 'binding.close');
    assert.deepEqual(close.params, { binding_id: id, thread: 'thread-1', reason: 'closed_from_room' });
  });
});

describe('push delivery and receipts', () => {
  async function delivery(p) {
    return p.next((f) => f.method === 'input.deliver', 3000);
  }

  test('accepted → delivered, then input.read → read', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    const message = env.core.say('thread-1', 'hello');
    const frame = await delivery(p);
    assert.deepEqual(Object.keys(frame.params).sort(), [
      'binding_id',
      'channel',
      'event_id',
      'message_id',
      'revision',
      'session_id',
      'text',
      'thread',
    ]);
    assert.equal(frame.params.binding_id, id);
    assert.equal(frame.params.text, 'hello');
    p.send({ jsonrpc: '2.0', id: frame.id, result: { status: 'accepted' } });
    await waitFor(() => message.status === 'delivered', 'delivered');
    p.send({ jsonrpc: '2.0', method: 'input.read', params: { binding_id: id, message_id: message.message_id } });
    await waitFor(() => message.status === 'read', 'read');
  });

  test('unknown → unconfirmed, unsupported → not sent, failed → retried', async () => {
    const p = await linked();
    await register(p);
    const cases = [
      ['unknown', 'unconfirmed'],
      ['unsupported', 'not_sent'],
    ];
    for (const [ack, status] of cases) {
      const message = env.core.say('thread-1', ack);
      const frame = await delivery(p);
      p.send({ jsonrpc: '2.0', id: frame.id, result: { status: ack } });
      await waitFor(() => message.status === status, status);
    }
    const message = env.core.say('thread-1', 'fails');
    const frame = await delivery(p);
    p.send({ jsonrpc: '2.0', id: frame.id, result: { status: 'failed' } });
    await waitFor(() => message.receipts.some((r) => r.status === 'retry'), 'retry');
    assert.equal(message.status, 'pending');
  });

  test('one message in flight per thread', async () => {
    const p = await linked();
    await register(p);
    env.core.say('thread-1', 'one');
    env.core.say('thread-1', 'two');
    const first = await delivery(p);
    await assert.rejects(p.next((f) => f.method === 'input.deliver', 600));
    p.send({ jsonrpc: '2.0', id: first.id, result: { status: 'accepted' } });
    assert.equal((await delivery(p)).params.text, 'two');
  });
});

describe('speech', () => {
  const speech = (id, extra = {}) => ({
    binding_id: id,
    event_id: 'e1',
    utterance_id: 'u1',
    session_id: 's',
    revision: 1,
    text: 'hi',
    ...extra,
  });

  test('publish is saved and echoes the ids; the same utterance again is not stored twice', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    const answer = await p.request('speech.publish', speech(id, { language: 'en' }));
    assert.equal(answer.result.text_saved, true);
    assert.equal(answer.result.event_id, 'e1');
    assert.equal(answer.result.utterance_id, 'u1');
    await p.request('speech.publish', speech(id, { language: 'en' }));
    assert.equal(env.core.speech.length, 1);
  });

  test('unknown binding, invalid speech and missing ids', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    assert.equal((await p.request('speech.publish', speech('nope'))).result.status, 'unknown_binding');
    const rejected = (await p.request('speech.publish', speech(id, { language: 'xx' }))).result;
    assert.equal(rejected.status, 'rejected');
    assert.equal(rejected.terminal, true);
    assert.equal((await p.request('speech.publish', speech(id, { event_id: '' }))).error.code, -32602);
  });

  test('policies change the answer, including never answering', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p);
    env.core.setPolicy('speech.publish', 'text_only');
    assert.equal((await p.request('speech.publish', speech(id))).result.status, 'text_only');
    env.core.setPolicy('speech.publish', 'silent');
    p.send({ jsonrpc: '2.0', id: 'c:silent', method: 'speech.publish', params: speech(id, { utterance_id: 'u2' }) });
    await assert.rejects(p.next((f) => f.id === 'c:silent', 500));
    env.core.setPolicy('binding.register', 'refuse');
    assert.deepEqual(await register(p), { error: 'room.thread_invalid' });
  });
});

describe('pull delivery', () => {
  test('nothing is pushed; get claims, repeats until acknowledged, and ack marks read', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p, { input_mode: 'pull' });
    const message = env.core.say('thread-1', 'pulled');
    await assert.rejects(p.next((f) => f.method === 'input.deliver', 600));
    const check = (await p.request('input.pull', { binding_id: id, operation: 'check' })).result;
    assert.deepEqual(check, { connected: true, pending: true, count: 1, fresh: 1 });
    const got = (await p.request('input.pull', { binding_id: id, operation: 'get' })).result;
    assert.equal(got.messages.length, 1);
    assert.equal(got.messages[0].message_id, message.message_id);
    assert.equal(message.status, 'delivered');
    assert.equal(got.fresh, 0);
    const again = (await p.request('input.pull', { binding_id: id, operation: 'get' })).result;
    assert.equal(again.messages.length, 1);
    const acked = (await p.request('input.pull', { binding_id: id, operation: 'get', ack_ids: [message.message_id] }))
      .result;
    assert.equal(acked.acknowledged, 1);
    assert.equal(acked.messages.length, 0);
    assert.equal(message.status, 'read');
  });

  test('refusals: a push binding, a superseded one, a bad operation, acks on check', async () => {
    const p = await linked();
    const { binding_id: push } = await register(p, { thread: 'push-thread' });
    assert.equal((await p.request('input.pull', { binding_id: push, operation: 'check' })).error.code, -403);
    const { binding_id: old } = await register(p, { input_mode: 'pull' });
    const { binding_id: current } = await register(p, { input_mode: 'pull' });
    assert.equal((await p.request('input.pull', { binding_id: old, operation: 'check' })).error.code, -409);
    assert.equal((await p.request('input.pull', { binding_id: current, operation: 'peek' })).error.code, -400);
    const acks = await p.request('input.pull', { binding_id: current, operation: 'check', ack_ids: ['x'] });
    assert.equal(acks.error.message, 'room.pull_ack_invalid');
  });

  test('claims go back to pending when the binding leaves', async () => {
    const p = await linked();
    const { binding_id: id } = await register(p, { input_mode: 'pull' });
    const message = env.core.say('thread-1', 'claimed');
    await p.request('input.pull', { binding_id: id, operation: 'get' });
    assert.equal(message.status, 'delivered');
    p.send({ jsonrpc: '2.0', method: 'binding.unregister', params: { binding_id: id } });
    await waitFor(() => message.status === 'pending', 'released claim');
  });
});

describe('Core → connector requests', () => {
  test('request resolves with the connector result; rejects on error', async () => {
    const p = await linked();
    const pending = env.core.request('agents.list', { rescan: true });
    const frame = await p.next((f) => f.method === 'agents.list');
    assert.match(frame.id, /^s:\d+$/);
    p.send({ jsonrpc: '2.0', id: frame.id, result: { agents: [] } });
    assert.deepEqual(await pending, { agents: [] });
    const failing = env.core.request('node.status');
    const second = await p.next((f) => f.method === 'node.status');
    p.send({ jsonrpc: '2.0', id: second.id, error: { code: -1, message: 'no' } });
    await assert.rejects(failing);
  });
});
