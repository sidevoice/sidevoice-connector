/** Cursor harness implementation — the Cursor CLI (`cursor-agent`).
 *
 *  Cursor hands an MCP server nothing about the conversation it serves: the environment is scrubbed
 *  to HOME, PATH, SHELL and a few more, and `tools/call` carries no metadata. What it does say is its
 *  name in `initialize` (`clientInfo.name` "Cursor"), and the process that spawned the server holds
 *  the chat it is running open: `chats/<md5 of cwd>/<chat id>/store.db`, a SQLite store the CLI keeps
 *  open for as long as the chat is its current one. That open file is the identity — nothing is asked
 *  of the model. The Cursor editor keeps its chats elsewhere and serves every chat of a window from one
 *  MCP process, so it has no such file to point at: its chats join through an MCP App view instead
 *  (harness-cursor-app.mjs).
 *
 *  Working state and the messages a chat admits come from the transcript Cursor itself writes for the
 *  chat, `projects/<workspace slug>/agent-transcripts/<chat id>/<chat id>.jsonl`, and the model from the
 *  chat's own store. Nothing is installed in Cursor.
 *
 *  Delivery is experimental, and only for a chat started with `cursor-agent persist`: Cursor offers no way
 *  to put a message into a chat, but `persist` runs the CLI inside tmux (`tmux -L cursor-agent`, socket
 *  under `TMUX_TMPDIR=/tmp`) and tags the session with the chat it runs (`@cursor_chat_id`). The voice
 *  message is pasted into that pane as a bracketed paste — the CLI's input turns bracketed paste on and
 *  keeps a pasted text whole, newlines included — and Enter sends it. It types where the person types:
 *  both at once would mix, which the operator accepted. Any other chat declares delivery unsupported.
 *  See docs/HARNESS_CONTRACT.md, "Cursor". Read from cursor-agent 2026.09.28-64d2043; not yet seen live. */
import { execFile, execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { closeSync, existsSync, openSync, readdirSync, readFileSync, readlinkSync, readSync, statSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { defineHarness, envelope, SUPPORTED, UNSUPPORTED, tailJsonl, voiceEnvelope } from './harness-contract.mjs';
import { deliverToView, drawsViews, openView, THREAD_PREFIX, viewKey, viewState } from './harness-cursor-app.mjs';

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

const EDITOR_NOTE = 'This Cursor did not declare MCP Apps support (the io.modelcontextprotocol/ui extension) when it started this '
  + 'server, so it would draw no Sidevoice card, and a chat of the editor has no other way to receive the room; nor is this a '
  + 'Cursor CLI chat (cursor-agent), whose open chat Sidevoice can see. ~/.sidevoice/mcp.log shows what it declared.';

const EDITOR_WATCH = 'The room learns that this chat took a voice message, and when it is working, from Cursor itself: from its answer to the card, and from this chat\'s transcript once the first voice message it takes shows which one is its own.';

/** A conversation in Cursor, by what Cursor itself shows: its name in `initialize`, and the chat store the
 *  spawning process holds; in the editor, a conversation of our own, bound to the view that `voice_connect`
 *  draws in the chat. Not Cursor: null. Cursor with neither: refused with the reason. */
/** Whether the MCP client says it is Cursor: "Cursor" (CLI) or "cursor-vscode" (editor), in `initialize`. */
export const isCursorClient = client => /^cursor\b/i.test(client?.name || '');

export function sessionIdentity({ client, env = process.env, locate = findChat, session = persistSession } = {}) {
  if (!isCursorClient(client)) return null;
  const found = locate();
  // The editor — the client that draws views; cursor-agent does not — whatever its parent holds open: the
  // chat is the one the view `voice_connect` returns is drawn in, and the id is ours.
  if (drawsViews(client)) { const thread = THREAD_PREFIX + randomUUID(); return { harness: 'cursor', thread, route: 'cursor-editor-view', chatStoreHeld: !!found,
    delivery: { kind: 'cursor-app', thread, key: viewKey() },
    // Working state comes from the chat's transcript once the first voice message it takes says which it is.
    experimental: ['sessionIdentity', 'working', 'endOfTurn'], editor: true, watchNote: EDITOR_WATCH }; }
  if (!found) throw new Error(EDITOR_NOTE);
  const persisted = session(found.chat, env);
  if (persisted) return { harness: 'cursor', thread: found.chat, route: 'cursor-cli-persist', delivery: { kind: 'cursor-tmux', chat: found.chat } };
  return { harness: 'cursor', thread: found.chat, route: 'cursor-cli', delivery: { kind: 'none', chat: found.chat },
    capabilities: { deliver: UNSUPPORTED }, deliverNote: NOT_PERSISTED };
}

const NOT_PERSISTED = 'This chat is not running under cursor-agent persist, so nothing can put a message into it. To let the '
  + 'room talk to a Cursor CLI chat (experimental), start it with  cursor-agent persist  (it needs tmux).';

/** The tmux that `persist` uses, the way the CLI picks it, and the server it runs its sessions on. */
function tmuxCommand(env = process.env) {
  const binary = env.SIDEVOICE_TMUX_BIN?.trim() || env.CURSOR_AGENT_TMUX_PATH?.trim()
    || (env.AGENT_TMUX_ROOT_PATH?.trim() ? path.join(env.AGENT_TMUX_ROOT_PATH.trim(), 'bin', 'tmux') : 'tmux');
  const named = env.CURSOR_AGENT_TMUX_SERVER_NAME?.trim();
  const server = named && /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(named) ? named : 'cursor-agent';
  // The environment persist gives tmux: its socket lives under TMUX_TMPDIR=/tmp, and a TMUX of ours must
  // not make it think it is nested.
  const childEnv = { PATH: env.PATH || '/usr/bin:/bin', HOME: env.HOME || os.homedir(), TMUX_TMPDIR: '/tmp' };
  return { binary, prefix: ['-u', '-L', server, '-f', '/dev/null'], env: childEnv };
}

function tmux(args, { env = process.env, input } = {}) {
  const { binary, prefix, env: childEnv } = tmuxCommand(env);
  return new Promise((resolve, reject) => {
    const child = execFile(binary, [...prefix, ...args], { env: childEnv, timeout: 5000, maxBuffer: 1 << 20 }, (error, stdout, stderr) => {
      if (error) return reject(new Error((String(stderr || '').trim() || error.message).slice(0, 300)));
      resolve(String(stdout));
    });
    if (input !== undefined) child.stdin.end(input); else child.stdin.end();
  });
}

/** The persistent session running this chat, as `cursor-agent persist` lists its own: sessions it manages
 *  (`@cursor_managed` 1) carry the chat they run in `@cursor_chat_id`, which follows /new and /resume. */
export function persistSession(chat, env = process.env) {
  const { binary, prefix, env: childEnv } = tmuxCommand(env);
  let out;
  try {
    out = execFileSync(binary, [...prefix, 'list-sessions', '-F', '#{session_name}\t#{@cursor_managed}\t#{@cursor_chat_id}\t#{session_attached}'],
      { env: childEnv, encoding: 'utf8', timeout: 4000, stdio: ['ignore', 'pipe', 'ignore'] });
  } catch { return null; }
  return parseSessions(out).find(session => session.chat.toLowerCase() === String(chat).toLowerCase()) || null;
}

export function parseSessions(out) {
  return String(out).split('\n').map(line => line.split('\t')).filter(([name, managed, chat]) => name && managed === '1' && chat)
    .map(([name, , chat, attached, pane, inMode]) => ({ name, chat, attached: Number(attached) || 0,
      ...(pane ? { pane } : {}), ...(inMode !== undefined ? { inMode: inMode === '1' } : {}) }));
}

/** The same lookup, without blocking the connector, and with the pane the session shows and whether that
 *  pane is in a tmux mode (copy mode takes keys for itself). */
async function persistPane(chat, env) {
  let out;
  try { out = await tmux(['list-sessions', '-F', '#{session_name}\t#{@cursor_managed}\t#{@cursor_chat_id}\t#{session_attached}\t#{pane_id}\t#{pane_in_mode}'], { env }); }
  catch { return null; }
  return parseSessions(out).find(session => session.chat.toLowerCase() === String(chat).toLowerCase() && /^%\d+$/.test(session.pane || '')) || null;
}

/** Type the voice message into the chat's pane: the session is looked up again now, because the chat a
 *  pane runs changes with /new. Pasted with bracketed paste (tmux wraps it only for a pane that asked
 *  for it, as the CLI's input does) so the newlines stay inside the message, then Enter on its own after a
 *  pause, so it is a key and not the end of the paste. tmux proves the keys reached the pane, not that
 *  Cursor took them: the answer is `unknown`, and the transcript gives the read receipt. */
export async function deliver(delivery, event, env = process.env) {
  if (delivery?.kind === 'cursor-app') {
    // The editor: the view drawn in that chat dispatches it. Cursor answers only when the turn it starts is
    // over, so what is known now is what a keystroke proves.
    await deliverToView(delivery.thread, { message_id: event.message_id, text: envelope(event) });
    return { status: 'unknown', detail: 'dispatched by the chat\'s Sidevoice view (MCP App ui/message); Cursor answers when the turn ends' };
  }
  if (delivery?.kind !== 'cursor-tmux') throw new Error(`Cursor cannot take input here (delivery ${delivery?.kind || 'none'})`);
  const session = await persistPane(delivery.chat, env);
  // Gone from persist (stopped, or /new moved the pane to another chat): nothing will take it, now or later.
  if (!session) return { status: 'unsupported', error: `Chat ${delivery.chat} is no longer running in a cursor-agent persist session` };
  // The exact pane, not a session name tmux would also match by prefix; out of copy mode, or Enter is its.
  const target = session.pane;
  if (session.inMode) await tmux(['send-keys', '-t', target, '-X', 'cancel'], { env }).catch(() => {});
  const buffer = 'sidevoice-' + randomUUID();
  await tmux(['load-buffer', '-b', buffer, '-'], { env, input: envelope(event) });
  await tmux(['paste-buffer', '-p', '-r', '-d', '-b', buffer, '-t', target], { env });
  await new Promise(resolve => setTimeout(resolve, PASTE_SETTLE_MS));
  await tmux(['send-keys', '-t', target, 'Enter'], { env });
  return { status: 'unknown', detail: `typed into persistent session ${session.name}${session.attached ? ' (someone is attached)' : ''}` };
}

const PASTE_SETTLE_MS = Number(process.env.SIDEVOICE_CURSOR_PASTE_SETTLE_MS || 150);

/** What the connector can say about a conversation's route, for voice_status: an editor chat's card. */
export function deliveryState(delivery) {
  if (delivery?.kind !== 'cursor-app') return null;
  const view = viewState(delivery.thread);
  return { card_connected: !!view?.seen, card_last_seen_ms_ago: view?.seen ? Date.now() - view.seen : null };
}

/** Before any delivery, when the connector registers a conversation: an editor chat's view needs the
 *  connector's loopback bridge open for it, and learns the port from what `voice_connect` returns. */
export async function prepare(delivery, { log, admitted } = {}) {
  if (delivery?.kind !== 'cursor-app') return null;
  // Cursor answers the card's ui/message once the turn it started has run: the chat took that message.
  const { port, close } = await openView(delivery.thread, delivery.key, { log, answered: (message_id, ok) => { if (ok) admitted?.(message_id); } });
  // The card delivers, not the façade: the conversation can outlive the MCP process that created it.
  return { info: { port }, release: close, detachable: true };
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
  if (String(chatId).startsWith(THREAD_PREFIX)) return observeEditorChat(chatId, handlers, env);
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

/** An editor chat's conversation id is ours; its transcript is under Cursor's own chat id, which the editor
 *  never says. The first voice message it takes says it: the transcript that holds that message's header is
 *  this chat's. From then on it is watched like a CLI chat — early read receipt, working, end of turn. */
function observeEditorChat(thread, handlers, env) {
  let inner = null;
  const timer = setInterval(() => {
    if (inner) return;
    const wanted = handlers.expecting?.() || [];
    if (!wanted.length) return;
    const found = transcriptHolding(wanted, env);
    if (!found) return;
    inner = observe(found.chat, handlers, env);
    handlers.userMessage({ text: found.text, turn_id: null });
  }, EDITOR_SCAN_MS);
  timer.unref?.();
  return () => { clearInterval(timer); inner?.(); };
}
const EDITOR_SCAN_MS = Number(process.env.SIDEVOICE_CURSOR_SCAN_MS || 1000);

/** The chat transcript, written in the last ten minutes, whose user lines hold one of these messages. */
export function transcriptHolding(messageIds, env = process.env) {
  const wanted = new Set(messageIds);
  const since = Date.now() - 10 * 60_000;
  for (const root of dataDirs(env)) {
    let projects = []; try { projects = readdirSync(path.join(root, 'projects')); } catch { continue; }
    for (const project of projects) {
      const dir = path.join(root, 'projects', project, 'agent-transcripts');
      let chats = []; try { chats = readdirSync(dir); } catch { continue; }
      for (const chat of chats) {
        const file = path.join(dir, chat, chat + '.jsonl');
        let stat; try { stat = statSync(file); } catch { continue; }
        if (stat.mtimeMs < since) continue;
        let text = ''; try { text = readTail(file, stat.size); } catch { continue; }
        for (const line of text.split('\n')) {
          if (!line.includes('message_id')) continue;
          let entry; try { entry = JSON.parse(line); } catch { continue; }
          const seen = interpretTranscript(entry);
          const header = seen?.text ? voiceEnvelope(seen.text) : null;
          if (header && wanted.has(header.message_id)) return { chat, file, text: seen.text };
        }
      }
    }
  }
  return null;
}
function readTail(file, size, bytes = 256 * 1024) {
  const start = Math.max(0, size - bytes), buffer = Buffer.alloc(size - start);
  const fd = openSync(file, 'r'); try { readSync(fd, buffer, 0, buffer.length, start); } finally { closeSync(fd); }
  return buffer.toString('utf8');
}

export const cursorHarness = defineHarness({
  name: 'cursor',
  capabilities: {
    deliver: SUPPORTED,
    inspectInbound: UNSUPPORTED,
    working: SUPPORTED,
    endOfTurn: SUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  engine,
  observe,
  sessionIdentity,
  deliver,
  prepare,
  deliveryState,
  // Delivery types into the chat's terminal through tmux (CLI) or submits through an MCP App view (editor):
  // it works, by routes Cursor does not offer as an interface.
  experimental: ['deliver'],
});

export default cursorHarness;
