import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';

/** A fake Claude config dir: a settings file and a session registry entry. */
function config({ settings = {}, pid = 999999, sessionId = 's-1' } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'svcfg-'));
  mkdirSync(path.join(dir, 'sessions'));
  writeFileSync(path.join(dir, 'settings.json'), JSON.stringify(settings));
  writeFileSync(path.join(dir, 'sessions', `${pid}.json`), JSON.stringify({ sessionId, pid }));
  return dir;
}
async function inspect(dir, sessionId = 's-1') {
  process.env.CLAUDE_CONFIG_DIR = dir;
  const { inspectInbound } = await import('../harness-claude.mjs?' + Math.random());
  return inspectInbound(sessionId);
}

test('a prompting session is reachable', async () => {
  const r = await inspect(config({ settings: { permissions: { defaultMode: 'acceptEdits' } } }));
  assert.equal(r.ok, true);
  assert.equal(r.mode, 'acceptEdits');
});

test('a bypassing session is not reachable, and says why and how to fix it', async () => {
  const r = await inspect(config({ settings: { permissions: { defaultMode: 'bypassPermissions' } } }));
  assert.equal(r.ok, false);
  assert.equal(r.mode, 'bypassPermissions');
  assert.match(r.reason, /holds|held/);
  assert.match(r.remedy, /crossSessionInbound/);
  // The machine-wide option must state the safeguard it removes, not just the edit.
  assert.match(r.remedy, /any other local process/);
  assert.ok(r.confidence, 'says how sure it is');
});

test('crossSessionInbound accept makes a bypassing session reachable again', async () => {
  const r = await inspect(config({ settings: { permissions: { defaultMode: 'bypassPermissions' }, crossSessionInbound: 'accept' } }));
  assert.equal(r.ok, true);
  assert.equal(r.crossSessionInbound, 'accept');
});

test('refuse is reported as its own case', async () => {
  const r = await inspect(config({ settings: { permissions: { defaultMode: 'bypassPermissions' }, crossSessionInbound: 'refuse' } }));
  assert.equal(r.ok, false);
  assert.match(r.reason, /refuses/);
});

test('an unknown session still answers from settings, and lowers its confidence', async () => {
  const r = await inspect(config({ settings: { permissions: { defaultMode: 'bypassPermissions' } } }), 'not-a-session');
  assert.equal(r.ok, false);
  assert.match(r.confidence, /not found/);
});

test('a default install with no permission setting is reachable', async () => {
  const r = await inspect(config({ settings: {} }));
  assert.equal(r.ok, true);
  assert.equal(r.mode, 'default');
});
