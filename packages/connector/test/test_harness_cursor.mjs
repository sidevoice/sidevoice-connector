/** The Cursor CLI module, against the files cursor-agent writes. The shapes are the ones its own code
 *  writes (cursor-agent 2026.09.28-64d2043): the chat store `chats/<md5>/<chat>/store.db` with its
 *  `meta.json` sidecar, and `projects/<slug>/agent-transcripts/<chat>/<chat>.jsonl`. No logged-in
 *  Cursor was available to record a live chat; the fd lookup is exercised for real on this process. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { DatabaseSync } from 'node:sqlite';
import {
  chatOfProcess, chatOfStore, cursorHarness, engine, findChat, interpretTranscript, observe, openFiles,
  sessionIdentity, storeModel, transcriptPath,
} from '../harness-cursor.mjs';

const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(check, timeout = 4000) { const start = Date.now(); while (Date.now() - start < timeout) { if (check()) return; await wait(20); } throw new Error('timed out waiting'); }

const WORKSPACE_HASH = 'd41d8cd98f00b204e9800998ecf8427e';

/** A chat store the way the CLI makes one: WAL, `blobs` and `meta`, the metadata row hex-encoded JSON. */
export function chatStore(root, chat, { model = 'claude-4.5-sonnet', meta = {} } = {}) {
  const dir = path.join(root, 'chats', WORKSPACE_HASH, chat);
  mkdirSync(dir, { recursive: true });
  const db = new DatabaseSync(path.join(dir, 'store.db'));
  db.exec('PRAGMA journal_mode = WAL; CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, data BLOB); CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);');
  const row = { agentId: chat, latestRootBlobId: '', name: 'New Agent', mode: 'default', createdAt: Date.now(), lastUsedModel: model };
  db.prepare('INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)').run('0', Buffer.from(JSON.stringify(row)).toString('hex'));
  writeFileSync(path.join(dir, 'meta.json'), JSON.stringify({ schemaVersion: 1, createdAtMs: Date.now(), updatedAtMs: Date.now(), hasConversation: true, title: 'New Agent', cwd: '/work/app', ...meta }));
  return { db, store: path.join(dir, 'store.db'), dir };
}

function transcript(root, chat) {
  const dir = path.join(root, 'projects', 'work-app', 'agent-transcripts', chat);
  mkdirSync(dir, { recursive: true });
  return path.join(dir, chat + '.jsonl');
}
const user = text => JSON.stringify({ role: 'user', message: { content: [{ type: 'text', text: `<user_query>\n${text}\n</user_query>` }] } }) + '\n';
const assistant = text => JSON.stringify({ role: 'assistant', message: { content: [{ type: 'text', text }, { type: 'tool_use', name: 'Shell', input: { command: 'ls' } }] } }) + '\n';
const ended = (status = 'success') => JSON.stringify({ type: 'turn_ended', status }) + '\n';

test('cursor: declares what Cursor offers — observed working and end of turn, identity, and no delivery', () => {
  assert.deepEqual({ ...cursorHarness.capabilities }, {
    deliver: 'unsupported', inspectInbound: 'unsupported', working: 'supported', endOfTurn: 'supported', sessionIdentity: 'supported',
  });
  assert.equal(cursorHarness.deliver, undefined, 'nothing pretends to deliver');
});

test('cursor: the conversation is the chat store the spawning process holds open, never a model\'s word', () => {
  assert.deepEqual(chatOfStore('/h/.cursor/chats/' + WORKSPACE_HASH + '/c-1/store.db'), { chat: 'c-1', store: '/h/.cursor/chats/' + WORKSPACE_HASH + '/c-1/store.db', dir: '/h/.cursor/chats/' + WORKSPACE_HASH + '/c-1' });
  assert.equal(chatOfStore('/h/.cursor/chats/' + WORKSPACE_HASH + '/c-1/store.db-wal'), null);
  assert.equal(chatOfStore('/h/.cursor/ai-tracking/ai-code-tracking.db'), null);

  // A subagent's store is open beside the chat's; of two main ones, the one Cursor updated last is the chat calling.
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const main = chatStore(root, 'chat-main', { meta: { updatedAtMs: 2000 } });
  const sub = chatStore(root, 'chat-sub', { meta: { updatedAtMs: 9000, isSubagent: true } });
  const old = chatStore(root, 'chat-old', { meta: { updatedAtMs: 1000 } });
  const files = [main.store, main.store + '-wal', sub.store, old.store, '/dev/null'];
  assert.deepEqual(chatOfProcess(42, files), { chat: 'chat-main', store: main.store, cwd: '/work/app', pid: 42 });
  assert.equal(chatOfProcess(42, ['/dev/null']), null);

  // Found through a wrapper: the chat is held two levels up.
  const tree = { 30: 20, 20: 10, 10: 1 };
  const held = { 10: files };
  assert.equal(findChat(30, { filesOf: pid => held[pid] || [], parent: pid => tree[pid] }).chat, 'chat-main');
  assert.equal(findChat(30, { filesOf: () => [], parent: pid => tree[pid] }), null);
  for (const store of [main, sub, old]) store.db.close();
});

test('cursor: this process\'s own open chat store is seen through the operating system', () => {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const { db, store } = chatStore(root, 'chat-live');
  try {
    assert.ok(openFiles(process.pid).includes(store), 'an open SQLite store shows among the process\'s files');
    assert.equal(chatOfProcess(process.pid).chat, 'chat-live');
  } finally { db.close(); }
});

test('cursor: identity is claimed only for a client that says it is Cursor, and the editor is refused with the reason', () => {
  const locate = () => ({ chat: 'chat-9', store: '/x/store.db' });
  assert.equal(sessionIdentity({ client: { name: 'claude-code' }, locate }), null);
  assert.equal(sessionIdentity({ client: null, locate }), null);
  assert.deepEqual(sessionIdentity({ client: { name: 'Cursor', version: '1.0.0' }, locate }),
    { harness: 'cursor', thread: 'chat-9', delivery: { kind: 'none', chat: 'chat-9' } });
  assert.throws(() => sessionIdentity({ client: { name: 'Cursor' }, locate: () => null }), /Cursor editor .*cursor-agent/s);
});

test('cursor: transcript lines read as the contract — a user message, the end of a turn, or nothing', () => {
  assert.deepEqual(interpretTranscript(JSON.parse(user('hola'))), { text: '<user_query>\nhola\n</user_query>' });
  assert.equal(interpretTranscript(JSON.parse(assistant('hecho'))), null);
  assert.deepEqual(interpretTranscript({ type: 'turn_ended', status: 'aborted', error: 'x' }), { ended: true, status: 'aborted' });
  assert.equal(interpretTranscript({ role: 'user', message: { content: [{ type: 'image' }] } }), null);
});

test('cursor: the model is the chat store\'s lastUsedModel, hex-encoded or plain', async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const { db, store } = chatStore(root, 'chat-m', { model: 'gpt-5.5' });
  assert.equal(await storeModel(store), 'gpt-5.5');
  assert.deepEqual(await engine('chat-m', { HOME: '/nonexistent', CURSOR_CONFIG_DIR: root }), { model: 'gpt-5.5', effort: null, thinking: null });
  db.prepare('UPDATE meta SET value = ? WHERE key = ?').run(JSON.stringify({ lastUsedModel: 'composer-2' }), '0');
  assert.equal(await storeModel(store), 'composer-2');
  assert.equal(await storeModel(path.join(root, 'missing.db')), null);
  db.close();
});

test('cursor: working and end of turn follow the transcript, through the rewrite Cursor makes after every turn', async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const env = { HOME: '/nonexistent', CURSOR_DATA_DIR: root, CURSOR_CONFIG_DIR: root };
  const { db } = chatStore(root, 'chat-t', { model: 'claude-4.5-sonnet' });
  const file = transcript(root, 'chat-t');
  // What was there before we looked is not news: an earlier turn, finished.
  writeFileSync(file, user('primero') + assistant('uno') + ended());
  assert.equal(transcriptPath('chat-t', env), file);
  const seen = [];
  const previous = process.env.SIDEVOICE_WORK_POLL_MS;
  const stop = observe('chat-t', {
    working: (value, extra) => seen.push({ working: value, ...extra }),
    userMessage: message => seen.push({ user: message.text, turn_id: message.turn_id }),
    engine: value => seen.push({ engine: value.model }),
  }, env);
  try {
    await until(() => seen.some(s => s.working === false));
    assert.deepEqual(seen.filter(s => 'user' in s), [], 'old messages are not re-read');
    await until(() => seen.some(s => s.engine));
    assert.equal(seen.find(s => s.engine).engine, 'claude-4.5-sonnet');

    // A new turn: Cursor rewrites the whole file, without the earlier turn_ended, and adds the new message.
    writeFileSync(file, user('primero') + assistant('uno') + user('segundo, escrito en Cursor'));
    await until(() => seen.some(s => s.user?.includes('segundo')));
    assert.deepEqual(seen.filter(s => 'user' in s).map(s => s.user), ['<user_query>\nsegundo, escrito en Cursor\n</user_query>']);
    assert.equal(seen.filter(s => 'user' in s)[0].turn_id, null, 'Cursor writes no turn ids');
    assert.equal(seen.filter(s => 'working' in s).at(-1).working, true);

    // It works, appends, and ends the turn; the model changed for this turn and is said again.
    db.prepare('UPDATE meta SET value = ? WHERE key = ?').run(Buffer.from(JSON.stringify({ lastUsedModel: 'gpt-5.5' })).toString('hex'), '0');
    appendFileSync(file, assistant('dos'));
    appendFileSync(file, ended());
    await until(() => seen.filter(s => 'working' in s).at(-1).working === false);
    // The next message arrives by append (a turn after a summary) and is news exactly once.
    appendFileSync(file, user('tercero'));
    await until(() => seen.some(s => s.user?.includes('tercero')));
    await wait(100);
    assert.deepEqual(seen.filter(s => 'user' in s).map(s => s.user.replace(/<\/?user_query>|\n/g, '')), ['segundo, escrito en Cursor', 'tercero']);
    const transitions = seen.filter(s => 'working' in s).map(s => s.working);
    assert.deepEqual(transitions, [false, true, false, true], 'one transition per change, none repeated');
  } finally {
    stop(); db.close();
    if (previous === undefined) delete process.env.SIDEVOICE_WORK_POLL_MS; else process.env.SIDEVOICE_WORK_POLL_MS = previous;
  }
});

test('cursor install: our one key in ~/.cursor/mcp.json, everything else kept, a foreign entry left alone', async () => {
  const { registerWithCursor, unregisterFromCursor, cursorMcpFile, serverCommand } = await import('../install.mjs');
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
  const env = { ...process.env, HOME: home, SIDEVOICE_INSTALL_FROM_SOURCE: '0', XDG_DATA_HOME: path.join(home, 'xdg') };
  const file = cursorMcpFile(env);
  assert.equal(file, path.join(home, '.cursor', 'mcp.json'));
  const { command, args } = serverCommand(env);

  const done = [];
  registerWithCursor(done, env);
  assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')), { mcpServers: { sidevoice: { command, args } } });
  assert.match(done.at(-1), /Registered the MCP server with Cursor/);
  registerWithCursor(done, env);
  assert.match(done.at(-1), /already runs this version/);

  // An older version of ours is re-pointed, keeping what the person added to it; their other servers stay.
  writeFileSync(file, JSON.stringify({ mcpServers: { other: { url: 'https://x' }, sidevoice: { command: 'node', args: ['/old/sidevoice/0.5.0/dist/cli.mjs', 'mcp'], env: { A: '1' } } }, extra: true }));
  registerWithCursor(done, env);
  assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')), { mcpServers: { other: { url: 'https://x' }, sidevoice: { command, args, env: { A: '1' } } }, extra: true });
  assert.match(done.at(-1), /Re-pointed/);

  // Not ours: printed, not touched. Not JSON: printed, not touched.
  const foreign = JSON.stringify({ mcpServers: { sidevoice: { command: 'my-wrapper', args: [] } } });
  writeFileSync(file, foreign); registerWithCursor(done, env);
  assert.equal(readFileSync(file, 'utf8'), foreign); assert.match(done.at(-1), /not touched/);
  writeFileSync(file, '{ // comment'); registerWithCursor(done, env);
  assert.equal(readFileSync(file, 'utf8'), '{ // comment'); assert.match(done.at(-1), /not valid JSON/);

  // Uninstall takes our key and nothing else.
  writeFileSync(file, JSON.stringify({ mcpServers: { other: { url: 'https://x' }, sidevoice: { command, args } } }));
  const next = [];
  unregisterFromCursor(done, next, env);
  assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')), { mcpServers: { other: { url: 'https://x' } } });
  writeFileSync(file, foreign); unregisterFromCursor(done, next, env);
  assert.equal(readFileSync(file, 'utf8'), foreign); assert.match(next.at(-1), /did not write/);
  assert.ok(!existsSync(path.join(home, '.cursor', 'hooks.json')), 'nothing but the MCP server is asked of Cursor');
});
