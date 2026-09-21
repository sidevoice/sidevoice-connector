import test from 'node:test';
import assert from 'node:assert/strict';
import {
  CAPABILITIES,
  SUPPORTED,
  UNKNOWN,
  advertisedCapabilities,
  capabilityState,
  defineHarness,
} from '../harness-contract.mjs';
import { claudeHarness } from '../harness-claude.mjs';
import { codexHarness } from '../harness-codex.mjs';
import { httpHarness } from '../harness-http.mjs';
import { harnessFor, identifyHarness } from '../harnesses.mjs';

for (const harness of [claudeHarness, codexHarness, httpHarness]) {
  test(`${harness.name} satisfies the harness contract`, () => {
    assert.deepEqual(Object.keys(advertisedCapabilities(harness)), [...CAPABILITIES]);
    for (const capability of CAPABILITIES) {
      assert.notEqual(capabilityState(harness, capability), UNKNOWN, `${capability} must be declared`);
      if (capabilityState(harness, capability) === SUPPORTED) {
        // Working state and the end of a turn are both observed: one watcher answers for both.
        const method = capability === 'working' || capability === 'endOfTurn' ? 'observe' : capability;
        assert.equal(typeof harness[method], 'function', `${capability} must be implemented`);
      }
    }
  });
}

test('an undeclared capability is unknown, not unsupported or absent', () => {
  const partial = { capabilities: { deliver: SUPPORTED } };
  assert.equal(capabilityState(partial, 'working'), UNKNOWN);
  assert.deepEqual(advertisedCapabilities(partial), {
    deliver: 'supported', inspectInbound: 'unknown', working: 'unknown', endOfTurn: 'unknown', sessionIdentity: 'unknown',
  });
});

test('a supported declaration without an implementation is rejected', () => {
  assert.throws(() => defineHarness({
    name: 'broken',
    capabilities: Object.fromEntries(CAPABILITIES.map(name => [name, name === 'deliver' ? 'supported' : 'unsupported'])),
  }), /declares deliver supported but does not implement it/);
});

test('Codex identity comes from tool metadata, and its working state is observed rather than reported', () => {
  const meta = { 'x-codex-turn-metadata': { thread_id: 'codex-thread', turn_id: 'turn-1' } };
  const identity = identifyHarness(meta, {});
  assert.equal(identity.module, codexHarness);
  assert.deepEqual({ harness: identity.harness, thread: identity.thread, delivery: identity.delivery }, {
    harness: 'codex', thread: 'codex-thread', delivery: { kind: 'codex-queue', thread: 'codex-thread' },
  });
  assert.equal(typeof codexHarness.observe, 'function');
  assert.equal(capabilityState(codexHarness, 'working'), 'supported');
  assert.equal(capabilityState(codexHarness, 'endOfTurn'), 'supported');
  assert.equal(capabilityState(codexHarness, 'inspectInbound'), 'unsupported');
});

test('the generic HTTP module identifies an explicitly configured external harness', () => {
  const identity = identifyHarness({}, { SIDEVOICE_THREAD: 'external-1', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/inbox', SIDEVOICE_HARNESS: 'custom' });
  assert.equal(identity.module, httpHarness);
  assert.equal(identity.harness, 'custom');
  assert.equal(identity.delivery.kind, 'http');
  assert.equal(harnessFor('unrecognized'), httpHarness);
});
