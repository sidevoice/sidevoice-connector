/** Experimental: a voice message into a chat of the Cursor editor, through an MCP App.
 *
 *  The editor gives no way to put a message into a chat, and tells an MCP server nothing about which chat
 *  is calling. It does render MCP Apps (the `io.modelcontextprotocol/ui` extension): a tool that names a
 *  `ui://` resource gets that resource's HTML drawn in the chat, next to the call, and a view that sends
 *  `ui/message` has its text submitted to that same chat as the person's message — as if they had typed it
 *  and pressed Enter, including Cursor's own queue-or-interrupt setting when the chat is busy. Read from
 *  Cursor 3.22.12 (`handleUiMessage` → `submitChatMaybeAbortCurrent(composerId, text)`, no approval asked).
 *
 *  So `voice_connect` itself carries the view. The view learns from the call's result the conversation id,
 *  the loopback port of this machine's connector and a key of its own, and long-polls the connector for
 *  what the room says to it. The key is never sent: the view proves it holds it (an HMAC of the conversation
 *  id), and every message comes signed with it, so neither a local process on the port nor one asking the
 *  port can pass for the other side. The chat is identified by construction — the view lives in it — and
 *  nothing is installed in Cursor.
 *
 *  Cursor answers a `ui/message` only when the turn it starts is over, so the view does not wait for that
 *  answer: it says it dispatched the message, and the delivery is `unknown`, like a keystroke.
 *
 *  What it cannot do: deliver while the view is not mounted (the chat closed, or the card scrolled out of
 *  a list that unmounts it — not verifiable without the app). */
import { createHmac, randomBytes, timingSafeEqual } from 'node:crypto';
import http from 'node:http';

export const RESOURCE_URI = 'ui://sidevoice/voice-link';
export const MIME = 'text/html;profile=mcp-app';
export const THREAD_PREFIX = 'cursor-editor-';

/** A fresh key for one conversation's view. */
export const viewKey = () => randomBytes(32).toString('hex');
const hmac = (key, text) => createHmac('sha256', Buffer.from(key, 'hex')).update(text).digest('hex');
const same = (a, b) => typeof a === 'string' && typeof b === 'string' && a.length === b.length && timingSafeEqual(Buffer.from(a), Buffer.from(b));

/** Whether the client that spawned this server draws MCP Apps views — what it declared in `initialize`. */
export function drawsViews(client) {
  const ui = client?.capabilities?.extensions?.['io.modelcontextprotocol/ui'];
  return !!ui && (!Array.isArray(ui.mimeTypes) || ui.mimeTypes.some(type => String(type).toLowerCase().startsWith('text/html')));
}

/** The resource as `resources/read` returns it: the view, allowed to reach loopback on any port — the port
 *  is the connector's, and comes with the call's result. */
export function resource() {
  return { uri: RESOURCE_URI, mimeType: MIME, text: viewHtml(),
    _meta: { ui: { csp: { connectDomains: ['http://127.0.0.1:*'] }, prefersBorder: true } } };
}

/** The view. It speaks the MCP Apps protocol to the host (JSON-RPC over postMessage), reads its conversation,
 *  port and key from the tool result, and loops: take the next message, check its signature, dispatch it
 *  with `ui/message`, say so. Cursor's answer, which comes when the turn ends, is reported when it comes. */
export function viewHtml() {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><style>
body{margin:0;font:12px/1.4 system-ui,sans-serif;color:var(--vscode-foreground,#888)}
p{margin:6px 10px}.dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:#888;margin-right:6px}
.on .dot{background:#8a6fd1}.err .dot{background:#d16f6f}
</style></head><body><p id="s"><span class="dot"></span><span id="t">Sidevoice: conectando…</span></p><script>
(() => {
  const status = (text, kind) => { document.getElementById('t').textContent = text; document.getElementById('s').className = kind || ''; };
  let serial = 0, link = null, toolCall = null, running = false, sign = null;
  const waiting = new Map();
  const post = message => window.parent.postMessage(message, '*');
  const request = (method, params) => new Promise((resolve, reject) => { const id = ++serial; waiting.set(id, { resolve, reject }); post({ jsonrpc: '2.0', id, method, params }); });
  const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  const hex = buffer => Array.from(new Uint8Array(buffer), b => b.toString(16).padStart(2, '0')).join('');
  /** The link voice_connect returned: its conversation, the connector's port and this view's key. */
  const find = value => {
    let found = null;
    const visit = v => { if (found || v == null) return;
      if (typeof v === 'string') { try { visit(JSON.parse(v)); } catch {} return; }
      if (typeof v !== 'object') return;
      if (v.view_link && typeof v.view_link === 'object') { found = v.view_link; return; }
      for (const k of Object.keys(v)) visit(v[k]); };
    visit(value);
    return found && /^cursor-editor-[0-9a-f-]{36}$/.test(found.conversation) && Number.isInteger(found.port) && /^[0-9a-f]{64}$/.test(found.key) ? found : null;
  };
  async function loop() {
    if (running || !link) return; running = true;
    const key = await crypto.subtle.importKey('raw', new Uint8Array(link.key.match(/../g).map(h => parseInt(h, 16))), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
    sign = async text => hex(await crypto.subtle.sign('HMAC', key, new TextEncoder().encode(text)));
    const base = 'http://127.0.0.1:' + link.port + '/cursor-app/';
    const query = async () => 'thread=' + encodeURIComponent(link.conversation) + '&auth=' + (await sign('poll:' + link.conversation)) + (toolCall ? '&tool_call=' + encodeURIComponent(toolCall) : '');
    const tell = async outcome => { try { await fetch(base + 'done?' + await query(), { method: 'POST', headers: { 'content-type': 'text/plain' }, body: JSON.stringify(outcome) }); } catch {} };
    for (;;) {
      let message = null;
      try {
        const answer = await fetch(base + 'next?' + await query());
        if (answer.status === 200) message = await answer.json();
        else if (answer.status !== 204) { status('Sidevoice: el conector ya no conoce esta conversación; pide unirte otra vez', 'err'); await wait(5000); continue; }
        status('Sidevoice: esta conversación recibe la voz de la sala (experimental)', 'on');
      } catch { status('Sidevoice: esperando al conector de este equipo…', 'err'); await wait(2000); continue; }
      if (!message) continue;
      if (!message.sig || message.sig !== await sign('msg:' + message.message_id + '\\n' + message.text)) { status('Sidevoice: descartado un mensaje sin firma', 'err'); continue; }
      // Cursor answers when the turn it starts has ended: dispatched is all that can be said now.
      request('ui/message', { role: 'user', content: [{ type: 'text', text: message.text }] })
        .then(() => tell({ message_id: message.message_id, stage: 'answered', ok: true }),
              error => tell({ message_id: message.message_id, stage: 'answered', ok: false, error: String(error && error.message || error) }));
      await tell({ message_id: message.message_id, stage: 'dispatched', ok: true });
    }
  }
  window.addEventListener('message', event => {
    const m = event.data; if (!m || m.jsonrpc !== '2.0') return;
    if (m.id !== undefined && waiting.has(m.id) && !m.method) { const w = waiting.get(m.id); waiting.delete(m.id); m.error ? w.reject(new Error(m.error.message)) : w.resolve(m.result); return; }
    if (m.method === 'ui/notifications/tool-result') { link = link || find(m.params); if (link) loop(); else status('Sidevoice: esta llamada no unió la conversación', 'err'); }
  });
  request('ui/initialize', { protocolVersion: '2026-01-26', appInfo: { name: 'sidevoice', version: '1' }, appCapabilities: {} })
    .then(result => { toolCall = result && result.hostContext && result.hostContext.toolInfo && result.hostContext.toolInfo.id || null; post({ jsonrpc: '2.0', method: 'ui/notifications/initialized', params: {} }); })
    .catch(() => status('Sidevoice: Cursor no inició la vista', 'err'));
})();
</script></body></html>`;
}

/** The connector's side: one loopback HTTP server per connector, on a port the system picks, and one mailbox
 *  per registered editor conversation. A view holds a long poll open; a delivery waits until a view takes the
 *  message and says it dispatched it. Only a registered conversation is served, only to a caller that proves
 *  it holds that conversation's key, and only from a webview Origin. */
const POLL_MS = 25_000;
const boxes = new Map();       // thread -> { key, queue, polls, seen, toolCall, waiting: Map(message_id -> {resolve, reject, timer}), log }
let server = null, listening = null;

/** `port`: the one a card already knows — a connector that took over from another listens where that one
 *  did, so the cards it served find this one there. Taken, any port, and those cards cannot reach it. */
export function ensureBridge(port = 0) {
  if (listening) return listening;
  server = http.createServer(handle);
  const listen = (on, fallback) => new Promise((resolve, reject) => {
    server.once('error', error => {
      if (fallback && error.code === 'EADDRINUSE') return resolve(listen(0, false));
      listening = null; server = null; reject(new Error(`The Cursor bridge could not listen on 127.0.0.1: ${error.code || error.message}`));
    });
    server.listen(on, '127.0.0.1', () => resolve(server.address().port));
  });
  listening = listen(port || 0, !!port);
  server.unref?.();
  return listening;
}

/** Make a conversation reachable by its view: called when the connector registers it. Returns the port the
 *  view must use, and a function that forgets the conversation. */
export async function openView(thread, key, { log = () => {}, answered = () => {}, port: wanted = 0 } = {}) {
  if (!thread?.startsWith(THREAD_PREFIX) || !/^[0-9a-f]{64}$/.test(key || '')) throw new Error('An editor conversation needs its id and its view key');
  const port = await ensureBridge(wanted);
  boxes.set(thread, { key, queue: [], polls: [], seen: 0, toolCall: null, waiting: new Map(), log, answered });
  return { port, close: () => { const mailbox = boxes.get(thread); if (!mailbox) return; boxes.delete(thread);
    for (const poll of mailbox.polls.splice(0)) poll(null);
    for (const pending of mailbox.waiting.values()) { clearTimeout(pending.timer); pending.reject(new Error('The conversation left the room')); } } };
}

function handle(req, res) {
  const origin = String(req.headers.origin || '');
  const cors = { 'access-control-allow-origin': origin, vary: 'origin' };
  if (!/^vscode-webview:\/\//.test(origin)) { res.writeHead(403); return res.end(); }
  const url = new URL(req.url, 'http://127.0.0.1');
  const thread = url.searchParams.get('thread') || '';
  const mailbox = boxes.get(thread);
  if (!mailbox) { res.writeHead(404, cors); return res.end(); }
  if (!same(url.searchParams.get('auth'), hmac(mailbox.key, 'poll:' + thread))) { res.writeHead(403, cors); return res.end(); }
  mailbox.seen = Date.now();
  if (url.searchParams.get('tool_call')) mailbox.toolCall = url.searchParams.get('tool_call').slice(0, 200);
  if (req.method === 'GET' && url.pathname === '/cursor-app/next') {
    let timer = null;
    const answer = message => { clearTimeout(timer); if (res.writableEnded || res.destroyed) return false;
      if (!message) { res.writeHead(204, cors); res.end(); return true; }
      res.writeHead(200, { ...cors, 'content-type': 'application/json' });
      res.end(JSON.stringify({ ...message, sig: hmac(mailbox.key, 'msg:' + message.message_id + '\n' + message.text) })); return true; };
    const next = mailbox.queue.shift();
    if (next) return answer(next);
    timer = setTimeout(() => { mailbox.polls = mailbox.polls.filter(poll => poll !== answer); answer(null); }, POLL_MS);
    mailbox.polls.push(answer);
    req.on('close', () => { mailbox.polls = mailbox.polls.filter(poll => poll !== answer); clearTimeout(timer); });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/cursor-app/done') {
    let body = '';
    req.on('data', chunk => { body += chunk; if (body.length > 65536) req.destroy(); });
    req.on('end', () => {
      let outcome = {}; try { outcome = JSON.parse(body); } catch {}
      const pending = mailbox.waiting.get(outcome.message_id);
      if (outcome.stage === 'dispatched' && pending) { mailbox.waiting.delete(outcome.message_id); clearTimeout(pending.timer); pending.resolve(); }
      // Cursor's own answer arrives when the turn it started ends; it is only news for the log.
      if (outcome.stage === 'answered') { try { mailbox.answered(outcome.message_id, !!outcome.ok); } catch {} }
      if (outcome.stage === 'answered') mailbox.log(`${thread}: Cursor ${outcome.ok ? 'took' : 'refused'} ${outcome.message_id}${outcome.ok ? '' : ': ' + String(outcome.error || '').slice(0, 200)}`);
      res.writeHead(204, cors); res.end();
    });
    return;
  }
  res.writeHead(404, cors); res.end();
}

/** Hand one message to the view in that conversation and wait until it has dispatched it. */
export function deliverToView(thread, message, { timeoutMs = Number(process.env.SIDEVOICE_CURSOR_APP_TIMEOUT_MS || 20_000) } = {}) {
  const mailbox = boxes.get(thread);
  if (!mailbox) return Promise.reject(new Error('That Cursor conversation is not open in this connector: ask it to join the room again.'));
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      mailbox.waiting.delete(message.message_id);
      mailbox.queue = mailbox.queue.filter(item => item !== message);
      reject(new Error('No Sidevoice card is open in that Cursor chat to take the message (open the chat, or ask it to join the room again).'));
    }, timeoutMs);
    mailbox.waiting.set(message.message_id, { resolve, reject, timer });
    const poll = mailbox.polls.shift();
    if (!poll || !poll(message)) mailbox.queue.push(message);
  });
}

export function viewState(thread) {
  const mailbox = boxes.get(thread);
  return mailbox ? { seen: mailbox.seen, toolCall: mailbox.toolCall } : null;
}

export function stopBridge() { const closing = server; server = null; listening = null; boxes.clear(); return closing ? new Promise(resolve => closing.close(() => resolve())) : Promise.resolve(); }
