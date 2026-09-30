/** Cursor harness implementation — the Cursor CLI (`cursor-agent`).
 *
 *  Cursor hands an MCP server nothing about the conversation it serves: the environment is scrubbed
 *  to HOME, PATH, SHELL and a few more, and `tools/call` carries no metadata. What it does say is its
 *  name in `initialize` (`clientInfo.name` "Cursor"), and the process that spawned the server holds
 *  the chat it is running open: `chats/<md5 of cwd>/<chat id>/store.db`, a SQLite store the CLI keeps
 *  open for as long as the chat is its current one. That open file is the identity — nothing is asked
 *  of the model. The Cursor editor keeps its chats elsewhere and serves every chat of a window from one
 *  MCP process, so it has no such file to point at: a conversation there is refused, saying why.
 *
 *  Working state and the messages a chat admits come from the transcript Cursor itself writes for the
 *  chat, `projects/<workspace slug>/agent-transcripts/<chat id>/<chat id>.jsonl`, and the model from the
 *  chat's own store. Nothing is installed in Cursor.
 *
 *  Delivery is unsupported: Cursor offers no way to put a message into a chat someone is using (see
 *  docs/HARNESS_CONTRACT.md, "Cursor"). Read from cursor-agent 2026.09.28-64d2043; not yet seen live. */
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, readlinkSync, statSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { defineHarness, SUPPORTED, UNSUPPORTED, tailJsonl } from './harness-contract.mjs';

/** Where the CLI may keep chats (`chats/`): `CURSOR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cursor`, else
 *  `~/.cursor`. Cursor scrubs the environment of its MCP servers, so the variables the CLI saw may not be
 *  the ones we see: every place it could have used is tried. */
export function configDirs(env = process.env) {
  const home = env.HOME || os.homedir();
  return [...new Set([env.CURSOR_CONFIG_DIR?.trim(), env.XDG_CONFIG_HOME?.trim() && path.join(env.XDG_CONFIG_HOME.trim(), 'cursor'),
    path.join(home, '.config', 'cursor'), path.join(home, '.cursor')].filter(Boolean))];
}
/** Where it writes transcripts (`projects/`): `CURSOR_DATA_DIR`, else `~/.cursor`. */
export function dataDirs(env = process.env) {
  return [...new Set([env.CURSOR_DATA_DIR?.trim(), path.join(env.HOME || os.homedir(), '.cursor')].filter(Boolean))];
}

/** A chat store path, split into what it names; null for any other file. */
const STORE = /[\\/]chats[\\/]([0-9a-f]{32})[\\/]([^\\/]+)[\\/]store\.db$/;
export function chatOfStore(file) {
  const match = STORE.exec(file || '');
  return match ? { chat: match[2], store: file, dir: path.dirname(file) } : null;
}

/** The files a process has open: /proc on Linux, lsof on macOS. Empty when neither answers. */
export function openFiles(pid) {
  try {
    const fds = path.join('/proc', String(pid), 'fd');
    return readdirSync(fds).map(fd => { try { return readlinkSync(path.join(fds, fd)); } catch { return null; } }).filter(Boolean);
  } catch {}
  try {
    return execFileSync('lsof', ['-n', '-P', '-Fn', '-p', String(pid)], { encoding: 'utf8', timeout: 4000, stdio: ['ignore', 'pipe', 'ignore'] })
      .split('\n').filter(line => line.startsWith('n')).map(line => line.slice(1));
  } catch { return []; }
}

function parentOf(pid) {
  try { return Number(execFileSync('ps', ['-o', 'ppid=', '-p', String(pid)], { encoding: 'utf8', timeout: 4000 }).trim()) || null; }
  catch { return null; }
}

function readJson(file) { try { return JSON.parse(readFileSync(file, 'utf8')); } catch { return null; } }

/** The chat a process is running: the main chat store it holds open. Subagent stores are open beside it
 *  and say so in their sidecar; of what is left, the one Cursor updated last — the chat whose turn is
 *  calling us — when a switch has not closed the previous one yet. */
export function chatOfProcess(pid, files = openFiles(pid)) {
  const chats = new Map();
  for (const file of files) {
    const chat = chatOfStore(file);
    if (chat && !chats.has(chat.chat)) chats.set(chat.chat, chat);
  }
  const candidates = [...chats.values()].map(chat => ({ ...chat, meta: readJson(path.join(chat.dir, 'meta.json')) || {} }))
    .filter(chat => !chat.meta.isSubagent);
  if (!candidates.length) return null;
  const updated = chat => Number(chat.meta.updatedAtMs) || (() => { try { return statSync(chat.store).mtimeMs; } catch { return 0; } })();
  candidates.sort((a, b) => updated(b) - updated(a));
  const { chat, store, meta } = candidates[0];
  return { chat, store, cwd: typeof meta.cwd === 'string' ? meta.cwd : null, pid };
}

/** Up the process tree from the one that spawned us, to the first that holds a chat open. A server started
 *  through a wrapper (a shell, npx) sits one or two levels below cursor-agent. */
export function findChat(start = process.ppid, { depth = 4, filesOf = openFiles, parent = parentOf } = {}) {
  for (let pid = start, level = 0; pid && pid > 1 && level < depth; pid = parent(pid), level++) {
    const found = chatOfProcess(pid, filesOf(pid));
    if (found) return found;
  }
  return null;
}

const EDITOR_NOTE = 'Cursor did not say which chat this is: only the Cursor CLI (cursor-agent) keeps the chat it runs open where '
  + 'Sidevoice can see it. The Cursor editor serves every chat of a window from one MCP server and tells it nothing about '
  + 'which one is calling, so a chat in the editor cannot join the voice room. Run the conversation in cursor-agent to use voice.';

/** A conversation in Cursor, by what Cursor itself shows: its name in `initialize`, and the chat store the
 *  spawning process holds. Not Cursor: null. Cursor, but no chat to point at: refused with the reason. */
export function sessionIdentity({ client, locate = findChat } = {}) {
  if (!/^cursor\b/i.test(client?.name || '')) return null;
  const found = locate();
  if (!found) throw new Error(EDITOR_NOTE);
  return { harness: 'cursor', thread: found.chat, delivery: { kind: 'none', chat: found.chat } };
}

/** The store of a chat, found by its id under every workspace's directory. */
export function storePath(chatId, env = process.env) {
  for (const dir of configDirs(env)) {
    const root = path.join(dir, 'chats');
    let workspaces = [];
    try { workspaces = readdirSync(root); } catch { continue; }
    for (const workspace of workspaces) {
      const candidate = path.join(root, workspace, chatId, 'store.db');
      if (existsSync(candidate)) return candidate;
    }
  }
  return null;
}

/** The model the chat last ran a turn with: `lastUsedModel` in the store's metadata row, which the CLI
 *  sets as each turn starts. The row is JSON, hex-encoded; plain JSON is read too. Null when the store,
 *  or Node's SQLite, is not there. */
export async function storeModel(store) {
  if (!store || !existsSync(store)) return null;
  let DatabaseSync;
  try { ({ DatabaseSync } = await import('node:sqlite')); } catch { return null; }
  let db;
  try {
    db = new DatabaseSync(store, { readOnly: true });
    const row = db.prepare('SELECT value FROM meta WHERE key = ?').get('0');
    const value = String(row?.value ?? '');
    let meta = null;
    try { meta = JSON.parse(value); } catch { try { meta = JSON.parse(Buffer.from(value, 'hex').toString('utf8')); } catch {} }
    const model = meta?.lastUsedModel;
    return typeof model === 'string' && model ? model : null;
  } catch { return null; } finally { try { db?.close(); } catch {} }
}

/** Which model the chat runs, from its store. Asked once at voice_connect; the watch keeps it current. */
export async function engine(chatId, env = process.env) {
  const model = await storeModel(storePath(chatId, env));
  return model ? { model, effort: null, thinking: null } : null;
}

/** The transcript of a chat, found by its name under every workspace's project directory, so the
 *  workspace slug never has to be derived. */
export function transcriptPath(chatId, env = process.env) {
  for (const root of dataDirs(env)) {
    const projects = path.join(root, 'projects');
    let dirs = [];
    try { dirs = readdirSync(projects); } catch { continue; }
    for (const dir of dirs) {
      const candidate = path.join(projects, dir, 'agent-transcripts', chatId, chatId + '.jsonl');
      if (existsSync(candidate)) return candidate;
    }
  }
  return null;
}

/** What one transcript line says, in the terms of the contract: a user message, the end of a turn, or
 *  nothing. Lines are `{"role":"user"|"assistant","message":{"content":[{type:"text",text}, …]}}` and
 *  `{"type":"turn_ended","status":"success"|"error"|"aborted"}`; there are no turn ids. */
export function interpretTranscript(entry) {
  if (entry?.type === 'turn_ended') return { ended: true, status: entry.status || null };
  if (entry?.role !== 'user') return null;
  const content = entry.message?.content;
  const text = typeof content === 'string' ? content
    : Array.isArray(content) ? content.filter(part => part?.type === 'text').map(part => part.text).join('\n') : '';
  return text ? { text } : null;
}

const POLL_MS = Number(process.env.SIDEVOICE_WORK_POLL_MS || 400);

/** Watch one chat through its transcript. A user line opens a turn, `turn_ended` closes it. The CLI
 *  rewrites the whole file on the first write after a turn ends — dropping the earlier `turn_ended` lines,
 *  and after a summary most of the history — so a user message is news only when the file now holds that
 *  text more times than any earlier version of it did. The model is read from the chat's store when a turn
 *  starts, since that is when it is set. */
export function observe(chatId, handlers, env = process.env) {
  let working = null, reported = null, attached = false, model = null;
  let current = new Map();                 // user texts in the file as it is now, and how many times
  const known = new Map();                 // the most times each text has been seen in any version of it
  const sayWorking = () => { if (working !== null && working !== reported) { reported = working; handlers.working(working, {}); } };
  const checkModel = () => {
    storeModel(storePath(chatId, env)).then(found => {
      if (!found || found === model) return;
      model = found;
      handlers.engine?.({ model, effort: null, thinking: null });
    }).catch(() => {});
  };
  return tailJsonl(() => transcriptPath(chatId, env), (entry, replayed) => {
    const seen = interpretTranscript(entry);
    if (!seen) return;
    if (seen.ended) { working = false; if (!replayed) sayWorking(); return; }
    working = true;
    const count = (current.get(seen.text) || 0) + 1;
    current.set(seen.text, count);
    if (count <= (known.get(seen.text) || 0)) return;          // this version of the file already had it
    known.set(seen.text, count);
    if (!attached) return;                                     // there before we looked: not news
    sayWorking();
    handlers.userMessage({ text: seen.text, turn_id: null });
    checkModel();
  }, { intervalMs: POLL_MS, catchUp: true, rewrites: true,
    rewound: () => { current = new Map(); },
    caughtUp: () => { attached = true; sayWorking(); checkModel(); } });
}

export const cursorHarness = defineHarness({
  name: 'cursor',
  capabilities: {
    deliver: UNSUPPORTED,
    inspectInbound: UNSUPPORTED,
    working: SUPPORTED,
    endOfTurn: SUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  engine,
  observe,
  sessionIdentity,
});

export default cursorHarness;
