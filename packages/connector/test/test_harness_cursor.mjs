/** The Cursor CLI module, against the files cursor-agent writes. The shapes are the ones its own code
 *  writes (cursor-agent 2026.09.28-64d2043): the chat store `chats/<md5>/<chat>/store.db` with its
 *  `meta.json` sidecar, and `projects/<slug>/agent-transcripts/<chat>/<chat>.jsonl`. No logged-in
 *  Cursor was available to record a live chat; the fd lookup is exercised for real on this process. */
import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { appendFileSync, chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, statSync, symlinkSync, truncateSync, unlinkSync, writeFileSync } from 'node:fs';
import { conversationCapabilities, envelope, experimentalCapabilities, tailJsonl } from '../harness-contract.mjs';
import { harnessesPresent } from '../identity.mjs';
import { httpHarness } from '../harness-http.mjs';
import { identifyHarness } from '../harnesses.mjs';
import { DatabaseSync } from 'node:sqlite';
import { execFileSync } from 'node:child_process';
import http from 'node:http';
import {
  chatOfProcess, chatOfStore, cursorHarness, deliver, engine, findChat, interpretTranscript, observe, openFiles,
  parseSessions, persistSession, sessionIdentity, storeModel, transcriptPath,
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

test('cursor: declares what Cursor offers — observed working and end of turn, identity, and delivery marked experimental', () => {
  assert.deepEqual({ ...cursorHarness.capabilities }, {
    deliver: 'supported', inspectInbound: 'unsupported', working: 'supported', endOfTurn: 'supported', sessionIdentity: 'supported',
  });
  assert.deepEqual(experimentalCapabilities(cursorHarness), ['deliver']);
  // A chat not under persist loses delivery, and with it the experimental mark.
  const narrowed = conversationCapabilities(cursorHarness, { capabilities: { deliver: 'unsupported' } });
  assert.equal(narrowed.deliver, 'unsupported');
  assert.deepEqual(experimentalCapabilities(cursorHarness, narrowed), []);
  assert.equal(conversationCapabilities(cursorHarness, { capabilities: { inspectInbound: 'supported' } }).inspectInbound, 'unsupported', 'a conversation never gains one');
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
  const plain = sessionIdentity({ client: { name: 'Cursor', version: '1.0.0' }, locate, session: () => null });
  assert.deepEqual({ ...plain, deliverNote: undefined }, { harness: 'cursor', thread: 'chat-9', route: 'cursor-cli', delivery: { kind: 'none', chat: 'chat-9' }, capabilities: { deliver: 'unsupported' }, deliverNote: undefined });
  assert.match(plain.deliverNote, /cursor-agent persist/);
  assert.deepEqual(sessionIdentity({ client: { name: 'Cursor' }, locate, session: () => ({ name: 'cursor-1', chat: 'chat-9' }) }),
    { harness: 'cursor', thread: 'chat-9', route: 'cursor-cli-persist', delivery: { kind: 'cursor-tmux', chat: 'chat-9' } });
  assert.throws(() => sessionIdentity({ client: { name: 'Cursor' }, locate: () => null }), /did not declare MCP Apps support.*cursor-agent/s);
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

  // An older copy of ours (under the copies directory) is re-pointed, keeping what the person added to it; their
  // other servers stay.
  writeFileSync(file, JSON.stringify({ mcpServers: { other: { url: 'https://x' }, sidevoice: { command: 'node', args: [path.join(env.XDG_DATA_HOME, 'sidevoice', '0.5.0', 'dist', 'cli.mjs'), 'mcp'], env: { A: '1' } } }, extra: true }));
  registerWithCursor(done, env);
  assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')), { mcpServers: { other: { url: 'https://x' }, sidevoice: { command, args, env: { A: '1' } } }, extra: true });
  assert.match(done.at(-1), /Re-pointed/);

  // Not ours: printed, not touched — a program merely called cli.mjs is not Sidevoice's. Not JSON: printed, not touched.
  const foreign = JSON.stringify({ mcpServers: { sidevoice: { command: 'my-wrapper', args: [] } } });
  writeFileSync(file, foreign); registerWithCursor(done, env);
  assert.equal(readFileSync(file, 'utf8'), foreign); assert.match(done.at(-1), /not touched/);
  const lookalike = JSON.stringify({ mcpServers: { sidevoice: { command: 'node', args: ['/opt/unrelated/cli.mjs', 'mcp'] } } });
  writeFileSync(file, lookalike); registerWithCursor(done, env);
  assert.equal(readFileSync(file, 'utf8'), lookalike, 'an unrelated cli.mjs is not ours'); assert.match(done.at(-1), /not touched/);
  unregisterFromCursor(done, [], env);
  assert.equal(readFileSync(file, 'utf8'), lookalike, 'and is never removed');
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

test('cursor: a rewrite seen half done, a summary and a dropped duplicate still report each new message once', async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-'));
  const env = { HOME: '/nonexistent', CURSOR_DATA_DIR: root, CURSOR_CONFIG_DIR: root };
  const file = transcript(root, 'chat-r');
  writeFileSync(file, user('uno') + user('uno') + assistant('a') + ended());
  const said = [];
  const stop = observe('chat-r', { working() {}, userMessage: m => said.push(m.text.replace(/<\/?user_query>|\n/g, '')) }, env);
  try {
    await wait(500);
    // Truncated, then written in two goes: the first half ends mid-line.
    const whole = user('uno') + assistant('a') + user('dos');
    truncateSync(file, 0); await wait(450);
    writeFileSync(file, whole.slice(0, whole.length - 20)); await wait(450);
    writeFileSync(file, whole);
    await until(() => said.includes('dos'));
    // A summary: most of the history gone, then a new message.
    writeFileSync(file, user('resumen') + user('tres'));
    await until(() => said.includes('tres'));
    await wait(450);
    assert.deepEqual(said, ['dos', 'resumen', 'tres'], 'nothing old re-reported, nothing new missed');
  } finally { stop(); }
});

test('tail: a transcript that disappears and comes back short is read again, not given up on', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-tail-'));
  const file = path.join(dir, 't.jsonl');
  writeFileSync(file, JSON.stringify({ n: 'a'.repeat(100) }) + '\n');
  const lines = [];
  const stop = tailJsonl(() => (existsSync(file) ? file : null), (entry, replayed) => lines.push({ ...entry, replayed }), { intervalMs: 20, catchUp: true, rewrites: true });
  try {
    await until(() => lines.length === 1);
    unlinkSync(file); await wait(80);
    writeFileSync(file, '{"n":1}\n');
    await until(() => lines.some(l => l.n === 1));
  } finally { stop(); }
});

test('cursor install: the person\'s mcp.json keeps its permissions and its symlink, and an install for another harness keeps Cursor pointing at a copy that exists', async () => {
  const { install, uninstall, registerWithCursor, cursorMcpFile, serverCommand } = await import('../install.mjs');
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
  const env = { ...process.env, HOME: home, SIDEVOICE_DATA_DIR: path.join(home, '.sidevoice'), CLAUDE_CONFIG_DIR: path.join(home, '.claude'),
    SIDEVOICE_CLAUDE_BIN: '/nonexistent/claude', SIDEVOICE_INSTALL_FROM_SOURCE: '0', XDG_DATA_HOME: path.join(home, 'xdg') };
  const dotfiles = path.join(home, 'dotfiles'); mkdirSync(dotfiles); mkdirSync(path.join(home, '.cursor'));
  const real = path.join(dotfiles, 'mcp.json');
  writeFileSync(real, JSON.stringify({ mcpServers: { gh: { command: 'gh-mcp', env: { GITHUB_TOKEN: 'secret' } } } }));
  chmodSync(real, 0o600);
  symlinkSync(real, cursorMcpFile(env));
  registerWithCursor([], env);
  assert.ok(lstatSync(cursorMcpFile(env)).isSymbolicLink(), 'still a symlink');
  assert.equal(statSync(real).mode & 0o777, 0o600, 'still private');
  assert.equal(JSON.parse(readFileSync(real, 'utf8')).mcpServers.sidevoice.command, 'node');

  // An older copy of ours is registered in Cursor; installing for Claude Code removes that copy, so Cursor follows.
  writeFileSync(real, JSON.stringify({ mcpServers: { sidevoice: { command: 'node', args: [path.join(env.XDG_DATA_HOME, 'sidevoice', '0.0.1', 'dist', 'cli.mjs'), 'mcp'] } } }));
  mkdirSync(path.join(env.XDG_DATA_HOME, 'sidevoice', '0.0.1'), { recursive: true });
  const installed = await install(['--harness', 'claude', '--no-core'], env);
  assert.deepEqual(JSON.parse(readFileSync(real, 'utf8')).mcpServers.sidevoice.args, serverCommand(env).args);
  assert.match(installed.done.join('\n'), /Re-pointed Cursor/);
  const gone = await uninstall(['--harness', 'claude', '--no-core'], env);
  assert.match(gone.next.join('\n'), /Cursor still lists the sidevoice MCP server .* points at nothing/);
});

test('cursor: an explicitly configured receiver wins over Cursor, and ~/.cursor makes Cursor present', () => {
  const identity = identifyHarness({}, { SIDEVOICE_THREAD: 'ext-1', SIDEVOICE_DELIVERY_URL: 'http://127.0.0.1:9/inbox' }, { name: 'Cursor' });
  assert.equal(identity.module, httpHarness);
  // The editor hands its MCP servers its whole environment: a Claude Code session that opened it leaves its id there.
  const editor = { name: 'cursor-vscode', capabilities: { extensions: { 'io.modelcontextprotocol/ui': {} } } };
  assert.equal(identifyHarness({}, { CLAUDE_CODE_SESSION_ID: 'claude-1', CODEX_THREAD_ID: 'codex-1' }, editor).harness, 'cursor');
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-home-'));
  assert.ok(!harnessesPresent({ HOME: home, PATH: '' }).includes('cursor'));
  mkdirSync(path.join(home, '.cursor'));
  assert.ok(harnessesPresent({ HOME: home, PATH: '' }).includes('cursor'), 'the editor alone is Cursor too: its chats can join');
});

/** A real tmux, when this machine has one: `SIDEVOICE_TMUX_BIN`, else `tmux` on PATH, else ~/tools/bin/tmux. */
function findTmux() {
  for (const candidate of [process.env.SIDEVOICE_TMUX_BIN, 'tmux', path.join(os.homedir(), 'tools', 'bin', 'tmux')].filter(Boolean)) {
    try { execFileSync(candidate, ['-V'], { stdio: 'ignore' }); return candidate; } catch {}
  }
  return null;
}
export const TMUX = findTmux();

/** A tmux server laid out as `cursor-agent persist` lays it out: its own -L server under TMUX_TMPDIR=/tmp, a
 *  managed session tagged with its chat. In the pane, a stand-in for the CLI's input: raw mode, bracketed
 *  paste turned on (\e[?2004h, as cursor-agent's input does), every byte it receives recorded. */
export async function fakePersist(chat) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-tmux-'));
  const received = path.join(dir, 'received.bin');
  const input = path.join(dir, 'input.mjs');
  writeFileSync(input, `import { appendFileSync } from 'node:fs';
process.stdin.setRawMode(true); process.stdout.write('\\x1b[?2004h> ');
process.stdin.on('data', chunk => appendFileSync(${JSON.stringify(received)}, chunk));`);
  const env = { SIDEVOICE_TMUX_BIN: TMUX, CURSOR_AGENT_TMUX_SERVER_NAME: 'sv-test-' + process.pid + '-' + Math.random().toString(36).slice(2, 8) };
  const tmux = (...args) => execFileSync(TMUX, ['-u', '-L', env.CURSOR_AGENT_TMUX_SERVER_NAME, '-f', '/dev/null', ...args], { env: { ...process.env, TMUX_TMPDIR: '/tmp', TMUX: '' }, encoding: 'utf8' });
  tmux('new-session', '-d', '-s', 'cursor-test', '-x', '120', '-y', '30', `${process.execPath} ${input}`, ';',
    'set-option', '-t', 'cursor-test', '@cursor_managed', '1', ';', 'set-option', '-t', 'cursor-test', '@cursor_chat_id', chat);
  await until(() => tmux('capture-pane', '-p', '-t', 'cursor-test:').includes('>'));
  return { env, received, tmux, stop: () => { try { tmux('kill-server'); } catch {} } };
}

test('cursor persist: sessions are read as cursor-agent lists its own — managed ones, by the chat they run', () => {
  const out = 'cursor-a\t1\t11111111-1111-1111-1111-111111111111\t1\nplain\t\t\t0\ncursor-b\t1\t\t0\n';
  assert.deepEqual(parseSessions(out), [{ name: 'cursor-a', chat: '11111111-1111-1111-1111-111111111111', attached: 1 }]);
});

test('cursor persist: a voice message is pasted whole into the chat\'s pane and sent with Enter, through a real tmux', { skip: !TMUX && 'no tmux on this machine' }, async () => {
  const chat = '5f0c9a8e-1d2b-4c3a-9e8f-7a6b5c4d3e2f';
  const fake = await fakePersist(chat);
  const env = { ...process.env, ...fake.env }, { received, tmux } = fake;
  tmux('new-session', '-d', '-s', 'someone-else', 'sleep 60');
  try {
    assert.deepEqual(persistSession(chat.toUpperCase(), env), { name: 'cursor-test', chat, attached: 0 });
    assert.equal(persistSession('00000000-0000-0000-0000-000000000000', env), null);

    const event = { channel: 'voice', session_id: 's-1', revision: 3, message_id: 'm-1', text: 'primera línea\nsegunda "con comillas" y $HOME; \'x\'' };
    const outcome = await deliver({ kind: 'cursor-tmux', chat }, event, env);
    assert.equal(outcome.status, 'unknown', 'tmux proves the keys reached the pane, not that Cursor took them');
    await until(() => existsSync(received) && readFileSync(received, 'latin1').endsWith('\r'));
    const bytes = readFileSync(received, 'utf8');
    // One bracketed paste holding the whole message, newlines as newlines, nothing expanded; then Enter alone.
    assert.equal(bytes, '\x1b[200~' + envelope(event) + '\x1b[201~' + '\r');

    // /new in the chat's session: the pane now runs another chat, and nothing is typed into it for this one.
    tmux('set-option', '-t', 'cursor-test', '@cursor_chat_id', '99999999-9999-9999-9999-999999999999');
    const gone = await deliver({ kind: 'cursor-tmux', chat }, event, env);
    assert.equal(gone.status, 'unsupported', 'nothing will take it, now or later: not retried');
    assert.match(gone.error, /no longer running in a cursor-agent persist session/);
    assert.equal(readFileSync(received, 'utf8'), bytes, 'nothing more was typed');
  } finally { fake.stop(); }
});

/** A stand-in for Cursor's MCP Apps host (3.22.12's McpAppView): it answers `ui/initialize` with the tool
 *  call in `hostContext.toolInfo`, sends the tool result once the view says it is initialized, and answers
 *  `ui/message` the way `handleUiMessage` does — `{}` once submitted, an error otherwise. The view's own
 *  script runs unchanged; the webview's Origin is added to its requests, as the browser engine would. */
export function runView({ html, toolResult, toolCallId = 'toolu_1', submit = async () => {}, origin = 'vscode-webview://sv-test' }) {
  const script = html.slice(html.indexOf('<script>') + 8, html.lastIndexOf('</script>'));
  const listeners = [], submitted = [], dispatched = [], statuses = [];
  const toView = data => { for (const listener of listeners) listener({ data }); };
  const window = {
    addEventListener: (type, listener) => { if (type === 'message') listeners.push(listener); },
    parent: { postMessage: async message => {
      if (message.method === 'ui/initialize') return toView({ jsonrpc: '2.0', id: message.id, result: { protocolVersion: '2026-01-26', hostContext: { toolInfo: { id: toolCallId } } } });
      if (message.method === 'ui/notifications/initialized') return toView({ jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: toolResult });
      if (message.method === 'ui/message') {
        dispatched.push(message.params);
        try { await submit(message.params); submitted.push(message.params); toView({ jsonrpc: '2.0', id: message.id, result: {} }); }
        catch (error) { toView({ jsonrpc: '2.0', id: message.id, error: { code: -32000, message: error.message } }); }
      }
    } },
  };
  const element = { set textContent(text) { statuses.push(text); }, set className(_) {} };
  const document = { getElementById: () => element };
  const controller = new AbortController();
  // Once stopped, the view's retry pause never ends: its loop is left waiting, holding no timer.
  const pause = (fn, ms) => (controller.signal.aborted ? undefined : setTimeout(fn, ms));
  const fetchAsView = (url, options = {}) => fetch(url, { ...options, signal: controller.signal, headers: { ...(options.headers || {}), origin } });
  new Function('window', 'document', 'fetch', 'setTimeout', script)(window, document, fetchAsView, pause);
  return { submitted, dispatched, statuses, stop: () => controller.abort() };
}

test('cursor editor: a client that draws MCP Apps views gets a conversation of its own, marked experimental', () => {
  const editor = { name: 'cursor-vscode', version: '1.0.0', capabilities: { extensions: { 'io.modelcontextprotocol/ui': { mimeTypes: ['text/html;profile=mcp-app'] } } } };
  const identity = sessionIdentity({ client: editor, locate: () => null });
  assert.match(identity.thread, /^cursor-editor-[0-9a-f-]{36}$/);
  assert.deepEqual({ ...identity.delivery, key: undefined }, { kind: 'cursor-app', thread: identity.thread, key: undefined });
  assert.match(identity.delivery.key, /^[0-9a-f]{64}$/, 'a key of its own for the view');
  const capabilities = conversationCapabilities(cursorHarness, identity);
  assert.deepEqual([capabilities.deliver, capabilities.working, capabilities.endOfTurn, capabilities.sessionIdentity], ['supported', 'supported', 'supported', 'supported']);
  assert.deepEqual(experimentalCapabilities(cursorHarness, capabilities, identity), ['deliver', 'working', 'endOfTurn', 'sessionIdentity']);
  assert.notEqual(sessionIdentity({ client: editor, locate: () => null }).thread, identity.thread, 'each join is its own conversation');
  // The editor takes the card route even if its process holds a chat store open: only the CLI is found by one.
  const heldToo = sessionIdentity({ client: editor, locate: () => ({ chat: 'chat-x', store: '/x/store.db' }) });
  assert.equal(heldToo.route, 'cursor-editor-view'); assert.equal(heldToo.chatStoreHeld, true); assert.match(heldToo.thread, /^cursor-editor-/);
  // Without views there is no way in, and it says so.
  assert.throws(() => sessionIdentity({ client: { name: 'cursor-vscode', capabilities: {} }, locate: () => null }), /did not declare MCP Apps support/);
});

test('cursor editor: the view dispatches a voice message to its chat through ui/message, over a bridge only its own view can use', async () => {
  const app = await import('../harness-cursor-app.mjs?' + Math.random());
  const thread = 'cursor-editor-1b2c3d4e-5f60-4a1b-8c2d-3e4f5a6b7c8d', key = app.viewKey();
  const logged = [];
  const { port, close } = await app.openView(thread, key, { log: line => logged.push(line) });
  const read = app.resource();
  assert.equal(read.mimeType, 'text/html;profile=mcp-app');
  assert.deepEqual(read._meta.ui.csp.connectDomains, ['http://127.0.0.1:*'], 'loopback only; the port comes with the call');
  // The tool result as Cursor hands it to the view: our voice_connect answer, with the link.
  const toolResult = { content: [{ type: 'text', text: JSON.stringify({ status: 'joined', conversation: thread, view_link: { conversation: thread, port, key } }) }] };
  // Cursor answers ui/message when the turn it starts ends: here, long after.
  let finishTurn; const turn = new Promise(resolve => { finishTurn = resolve; });
  const view = runView({ html: read.text, toolResult, toolCallId: 'toolu_42', submit: () => turn });
  try {
    await until(() => app.viewState(thread)?.toolCall === 'toolu_42');
    const event = { channel: 'voice', session_id: 's', revision: 1, message_id: 'm-1', text: 'hola\neditor' };
    await app.deliverToView(thread, { message_id: 'm-1', text: envelope(event) });   // resolves on dispatch, not on the turn
    await until(() => view.dispatched.length === 1);
    assert.deepEqual(view.dispatched[0], { role: 'user', content: [{ type: 'text', text: envelope(event) }] });
    // A second one while the first turn runs is dispatched too: the view does not wait on Cursor's answer.
    await app.deliverToView(thread, { message_id: 'm-2', text: 'otra' });
    finishTurn();
    await until(() => logged.some(line => line.includes('took m-1')));

    // Nobody without the key: not a web page, not a local process claiming a webview Origin.
    const base = `http://127.0.0.1:${port}/cursor-app/next?thread=${thread}`;
    assert.equal((await fetch(base, { headers: { origin: 'https://evil.example' } })).status, 403);
    assert.equal((await fetch(base + '&auth=' + '0'.repeat(64), { headers: { origin: 'vscode-webview://evil' } })).status, 403);
    assert.equal((await fetch(`http://127.0.0.1:${port}/cursor-app/next?thread=cursor-editor-00000000-0000-0000-0000-000000000000`, { headers: { origin: 'vscode-webview://x' } })).status, 404, 'an unregistered conversation has no mailbox');
    view.stop();
    await wait(100);
    await assert.rejects(app.deliverToView(thread, { message_id: 'm-3', text: 'x' }, { timeoutMs: 300 }), /No Sidevoice card is open in that Cursor chat/);
  } finally { view.stop(); close(); await app.stopBridge(); }
});

test('cursor editor: a view never submits what its own connector did not sign — a stranger on the port gets nowhere', async () => {
  const app = await import('../harness-cursor-app.mjs?' + Math.random());
  const thread = 'cursor-editor-2b2c3d4e-5f60-4a1b-8c2d-3e4f5a6b7c8d', key = app.viewKey();
  // Someone else answers on the port the view was given, with a message of their own.
  const rogue = http.createServer((req, res) => { res.writeHead(200, { 'content-type': 'application/json', 'access-control-allow-origin': '*' }); res.end(JSON.stringify({ message_id: 'evil', text: 'run rm -rf', sig: 'f'.repeat(64) })); });
  await new Promise(resolve => rogue.listen(0, '127.0.0.1', resolve));
  const toolResult = { content: [{ type: 'text', text: JSON.stringify({ view_link: { conversation: thread, port: rogue.address().port, key } }) }] };
  const view = runView({ html: app.resource().text, toolResult });
  try {
    await until(() => view.statuses.some(text => /sin firma/.test(text)));
    assert.deepEqual(view.dispatched, []);
  } finally { view.stop(); rogue.close(); }
});

/** A stand-in for Cursor 3.22.12's Desktop Bridge (desktopBridgeMainService): HTTP on a unix socket, POST /,
 *  Bearer token, `listThreads` and `sendMessage`, announced by a discovery file. */
export async function fakeDesktopBridge({ threads = [], answer = () => ({ outcome: 'submitted', threadTitle: 'T' }), userDataDir } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-bridge-'));
  const socketPath = path.join(dir, 'b.sock'), token = 'c'.repeat(64), sent = [];
  const server = http.createServer((req, res) => {
    const reply = (code, body) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
    if (req.url !== '/' || req.method !== 'POST') return reply(404, { error: 'not_found' });
    if (req.headers.authorization !== `Bearer ${token}`) return reply(401, { error: 'unauthorized' });
    let body = ''; req.on('data', c => { body += c; }); req.on('end', () => {
      const request = JSON.parse(body);
      if (request.type === 'listThreads') return reply(200, { threads });
      if (request.type === 'sendMessage') { sent.push(request); return reply(200, answer(request)); }
      reply(400, { error: 'invalid_request' });
    });
  });
  await new Promise(resolve => server.listen(socketPath, resolve));
  writeFileSync(path.join(dir, 'abcdef0123456789.json'), JSON.stringify({ protocolVersion: 1, pid: process.pid, socketPath, token, appName: 'Cursor', appVersion: '3.22.12', userDataDir: userDataDir || dir, createdAt: Date.now() }), { mode: 0o600 });
  return { dir, sent, threads, env: { CURSOR_DESKTOP_BRIDGE_DIR: dir }, close: () => new Promise(resolve => server.close(resolve)) };
}

test('cursor desktop bridge: the calling chat is the only one working, and a message goes to a chat by its id', async () => {
  const bridge = await import('../harness-cursor-desktop.mjs');
  const thread = (id, status, source = 'local') => ({ id, title: id, source, status, lastUpdatedAt: Date.now(), windowId: 1 });
  const fake = await fakeDesktopBridge({ threads: [thread('comp-A', 'running'), thread('comp-B', 'idle'), thread('cloud-1', 'running', 'cloud')],
    answer: request => request.threadId === 'ghost' ? { outcome: 'not-found' } : { outcome: request.threadId === 'comp-B' ? 'queued' : 'submitted', threadTitle: request.threadId } });
  try {
    assert.equal(bridge.bridgeInstances(fake.env).length, 1);
    assert.equal(bridge.bridgeInstances({ CURSOR_DESKTOP_BRIDGE_DIR: '/nonexistent' }).length, 0);
    assert.deepEqual((await bridge.listThreads(fake.env)).map(t => t.id), ['comp-A', 'comp-B', 'cloud-1']);
    assert.equal(await bridge.callingThread(fake.env), 'comp-A', 'the only local chat working is the candidate');
    fake.threads.push(thread('comp-C', 'running'));
    assert.equal(await bridge.callingThread(fake.env), null, 'two working: not told');
    assert.deepEqual(await bridge.sendToThread('comp-B', 'hola\nqué tal', fake.env), { outcome: 'queued', title: 'comp-B' });
    assert.deepEqual(fake.sent.at(-1), { type: 'sendMessage', threadId: 'comp-B', text: 'hola\nqué tal', force: false });
    await assert.rejects(bridge.sendToThread('ghost', 'x', fake.env), error => error.code === 'NOT_FOUND');
    // An announcement from a Cursor that is gone is not a bridge.
    writeFileSync(path.join(fake.dir, 'dead.json'), JSON.stringify({ protocolVersion: 1, pid: 2 ** 22 + 7, socketPath: '/nonexistent.sock', token: 'x', createdAt: Date.now() + 1 }));
    assert.equal(bridge.bridgeInstances(fake.env).length, 1);
  } finally { await fake.close(); }
});

/** A Cursor state database with a chat whose bubble holds this marker (its voice_connect result), and another. */
export function fakeStateDb(userDataDir, { chat = 'comp-joined', marker, other = 'comp-other' } = {}) {
  mkdirSync(path.join(userDataDir, 'User', 'globalStorage'), { recursive: true });
  const db = new DatabaseSync(path.join(userDataDir, 'User', 'globalStorage', 'state.vscdb'));
  db.exec('CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)');
  db.prepare('INSERT INTO cursorDiskKV VALUES (?, ?)').run(`bubbleId:${other}:b1`, JSON.stringify({ type: 2, text: 'nada' }));
  if (marker) db.prepare('INSERT INTO cursorDiskKV VALUES (?, ?)').run(`bubbleId:${chat}:b7`, Buffer.from(JSON.stringify({ type: 2, toolFormerData: { result: JSON.stringify({ status: 'joined', view_link: { key: marker } }) } })));
  db.close();
}

test('cursor desktop bridge: the chat that joined is found in Cursor\'s state database by the key its voice_connect returned — one chat, or none', async () => {
  const bridge = await import('../harness-cursor-desktop.mjs');
  const userDataDir = mkdtempSync(path.join(os.tmpdir(), 'sv-cursor-user-'));
  const key = 'd'.repeat(64);
  fakeStateDb(userDataDir, { marker: key });
  const fake = await fakeDesktopBridge({ userDataDir });
  try {
    assert.equal(await bridge.composerHolding(key, fake.env), 'comp-joined');
    assert.equal(await bridge.composerHolding(key, fake.env, { candidate: 'comp-joined' }), 'comp-joined', 'a candidate is confirmed by its own rows');
    assert.equal(await bridge.composerHolding(key, fake.env, { candidate: 'comp-other' }), null, 'a wrong candidate is not');
    assert.equal(await bridge.composerHolding('e'.repeat(64), fake.env), null);
    assert.equal(await bridge.composerHolding('not-a-key', fake.env), null);
    // Another chat that came to hold the same key (it read it somewhere): two chats, so none is named.
    fakeStateDb(userDataDir, { chat: 'comp-reader', marker: key });
    assert.equal(await bridge.composerHolding(key, fake.env), null);
  } finally { await fake.close(); }
});

test('cursor desktop bridge: a request Cursor took and never answered is not sent again; one that never reached it is', async () => {
  const bridge = await import('../harness-cursor-desktop.mjs');
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sv-bridge-'));
  const socketPath = path.join(dir, 'b.sock');
  const server = http.createServer(req => { req.on('data', () => {}); req.on('end', () => req.socket.destroy()); });
  await new Promise(resolve => server.listen(socketPath, resolve));
  const announce = socket => writeFileSync(path.join(dir, 'x.json'), JSON.stringify({ protocolVersion: 1, pid: process.pid, socketPath: socket, token: 't', createdAt: Date.now() }));
  try {
    announce(socketPath);
    await assert.rejects(bridge.sendToThread('comp-A', 'hola', { CURSOR_DESKTOP_BRIDGE_DIR: dir }), error => error.code === 'UNKNOWN');
    // A socket that is announced but gone: never reached Cursor, safe to try elsewhere.
    const gone = path.join(dir, 'gone.sock'); writeFileSync(gone, '');
    announce(gone);
    await assert.rejects(bridge.sendToThread('comp-A', 'hola', { CURSOR_DESKTOP_BRIDGE_DIR: dir }), error => error.code === 'UNREACHABLE');
  } finally { server.close(); }
});

test('cursor desktop bridge: Cursor\'s "unknown-thread" is a chat it does not know, not a refusal', async () => {
  const bridge = await import('../harness-cursor-desktop.mjs');
  const fake = await fakeDesktopBridge({ answer: () => ({ status: 'unknown-thread' }) });
  try { await assert.rejects(bridge.sendToThread('comp-X', 'x', fake.env), error => error.code === 'NOT_FOUND'); } finally { await fake.close(); }
});
