/** Experimental: a voice message into a chat of the Cursor editor, through an MCP App.
 *
 *  The editor gives no way to put a message into a chat, and tells an MCP server nothing about which chat
 *  is calling. It does render MCP Apps (the `io.modelcontextprotocol/ui` extension): a tool that names a
 *  `ui://` resource gets that resource's HTML drawn in the chat, next to the call, and a view that sends
 *  `ui/message` has its text submitted to that same chat as the person's message — as if they had typed it
 *  and pressed Enter, including Cursor's own queue-or-interrupt setting when the chat is busy. Read from
 *  Cursor 3.22.12 (`handleUiMessage` → `submitChatMaybeAbortCurrent(composerId, text)`, no approval asked).
 *
 *  So `voice_connect` itself carries the view. The view learns the conversation id from the call's result,
 *  and asks the connector on this machine for what the room says to it, over loopback: a long poll to a
 *  small HTTP bridge the connector runs on 127.0.0.1, which the resource's CSP allows by name. The chat is
 *  identified by construction — the view lives in it — and nothing is installed in Cursor.
 *
 *  What it cannot do: deliver while the view is not mounted (the chat closed, or the card scrolled out of
 *  a list that unmounts it — not verifiable without the app). */
import http from 'node:http';

export const RESOURCE_URI = 'ui://sidevoice/voice-link';
export const MIME = 'text/html;profile=mcp-app';
export const THREAD_PREFIX = 'cursor-editor-';
const THREAD = /^cursor-editor-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function bridgePort(env = process.env) {
  const port = Number(env.SIDEVOICE_CURSOR_APP_PORT);
  return Number.isInteger(port) && port > 0 && port < 65536 ? port : 47631;
}
export const bridgeOrigin = (env = process.env) => `http://127.0.0.1:${bridgePort(env)}`;

/** Whether the client that spawned this server draws MCP Apps views — what it declared in `initialize`. */
export function drawsViews(client) {
  const ui = client?.capabilities?.extensions?.['io.modelcontextprotocol/ui'];
  return !!ui && (!Array.isArray(ui.mimeTypes) || ui.mimeTypes.some(type => String(type).toLowerCase().startsWith('text/html')));
}

/** The resource as `resources/read` returns it: the view, and the one origin it may connect to. */
export function resource(env = process.env) {
  return { uri: RESOURCE_URI, mimeType: MIME, text: viewHtml(bridgeOrigin(env)),
    _meta: { ui: { csp: { connectDomains: [bridgeOrigin(env)] }, prefersBorder: true } } };
}

/** The view. It speaks the MCP Apps protocol to the host (JSON-RPC over postMessage), finds its conversation
 *  in the tool result, and loops: take the next message from the bridge, submit it with `ui/message`, say how
 *  that went. */
export function viewHtml(origin) {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><style>
body{margin:0;font:12px/1.4 system-ui,sans-serif;color:var(--vscode-foreground,#888)}
p{margin:6px 10px}.dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:#888;margin-right:6px}
.on .dot{background:#8a6fd1}.err .dot{background:#d16f6f}
</style></head><body><p id="s"><span class="dot"></span><span id="t">Sidevoice: conectando…</span></p><script>
(() => {
  const BRIDGE = ${JSON.stringify(origin)};
  const status = (text, kind) => { document.getElementById('t').textContent = text; document.getElementById('s').className = kind || ''; };
  let serial = 0, thread = null, toolCall = null, running = false;
  const waiting = new Map();
  const post = message => window.parent.postMessage(message, '*');
  const request = (method, params) => new Promise((resolve, reject) => { const id = ++serial; waiting.set(id, { resolve, reject }); post({ jsonrpc: '2.0', id, method, params }); });
  const find = value => { const match = JSON.stringify(value || '').match(/cursor-editor-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/); return match ? match[0] : null; };
  const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  async function loop() {
    if (running || !thread) return; running = true;
    for (;;) {
      let message = null;
      try {
        const answer = await fetch(BRIDGE + '/cursor-app/next?thread=' + encodeURIComponent(thread) + (toolCall ? '&tool_call=' + encodeURIComponent(toolCall) : ''));
        if (answer.status === 200) message = await answer.json();
        status('Sidevoice: esta conversación recibe la voz de la sala (experimental)', 'on');
      } catch { status('Sidevoice: esperando al conector de este equipo…', 'err'); await wait(2000); continue; }
      if (!message) continue;
      let outcome = { message_id: message.message_id, ok: true };
      try { await request('ui/message', { role: 'user', content: [{ type: 'text', text: message.text }] }); }
      catch (error) { outcome = { message_id: message.message_id, ok: false, error: String(error && error.message || error) }; }
      try { await fetch(BRIDGE + '/cursor-app/done?thread=' + encodeURIComponent(thread), { method: 'POST', headers: { 'content-type': 'text/plain' }, body: JSON.stringify(outcome) }); } catch {}
    }
  }
  window.addEventListener('message', event => {
    const m = event.data; if (!m || m.jsonrpc !== '2.0') return;
    if (m.id !== undefined && waiting.has(m.id) && !m.method) { const w = waiting.get(m.id); waiting.delete(m.id); m.error ? w.reject(new Error(m.error.message)) : w.resolve(m.result); return; }
    if (m.method === 'ui/notifications/tool-result') { thread = thread || find(m.params); if (thread) loop(); else status('Sidevoice: esta llamada no unió la conversación', 'err'); }
  });
  request('ui/initialize', { protocolVersion: '2026-01-26', appInfo: { name: 'sidevoice', version: '1' }, appCapabilities: {} })
    .then(result => { toolCall = result && result.hostContext && result.hostContext.toolInfo && result.hostContext.toolInfo.id || null; post({ jsonrpc: '2.0', method: 'ui/notifications/initialized', params: {} }); })
    .catch(() => status('Sidevoice: Cursor no inició la vista', 'err'));
})();
</script></body></html>`;
}

/** The connector's side: one loopback HTTP server, one mailbox per editor conversation. A view holds a
 *  long poll open; a delivery waits for a view to take the message and say whether Cursor submitted it.
 *  Only a Cursor webview may ask (its Origin is `vscode-webview://…`, which a web page cannot claim), and
 *  only for a conversation id it was shown. */
const POLL_MS = 25_000;
const boxes = new Map();       // thread -> { queue, polls, seen, toolCall, waiting: Map(message_id -> {resolve, reject, timer}) }
let server = null, listening = null;

function box(thread) {
  if (!boxes.has(thread)) boxes.set(thread, { queue: [], polls: [], seen: 0, toolCall: null, waiting: new Map() });
  return boxes.get(thread);
}

export function ensureBridge(env = process.env) {
  if (listening) return listening;
  server = http.createServer(handle);
  listening = new Promise((resolve, reject) => {
    server.once('error', error => { listening = null; server = null; reject(new Error(`The Cursor bridge could not listen on 127.0.0.1:${bridgePort(env)}: ${error.code || error.message}`)); });
    server.listen(bridgePort(env), '127.0.0.1', () => resolve(server));
  });
  server.unref?.();
  return listening;
}

function handle(req, res) {
  const origin = String(req.headers.origin || '');
  const cors = { 'access-control-allow-origin': origin, vary: 'origin' };
  if (!/^vscode-webview:\/\//.test(origin)) { res.writeHead(403); return res.end(); }
  const url = new URL(req.url, 'http://127.0.0.1');
  const thread = url.searchParams.get('thread') || '';
  if (!THREAD.test(thread)) { res.writeHead(404, cors); return res.end(); }
  const mailbox = box(thread);
  mailbox.seen = Date.now();
  if (url.searchParams.get('tool_call')) mailbox.toolCall = url.searchParams.get('tool_call');
  if (req.method === 'GET' && url.pathname === '/cursor-app/next') {
    let timer = null;
    const answer = message => { clearTimeout(timer); if (res.writableEnded || res.destroyed) return false;
      if (!message) { res.writeHead(204, cors); res.end(); return true; }
      res.writeHead(200, { ...cors, 'content-type': 'application/json' }); res.end(JSON.stringify(message)); return true; };
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
      if (pending) { mailbox.waiting.delete(outcome.message_id); clearTimeout(pending.timer);
        outcome.ok ? pending.resolve() : pending.reject(new Error('Cursor did not take it: ' + String(outcome.error || 'unknown error').slice(0, 200))); }
      res.writeHead(204, cors); res.end();
    });
    return;
  }
  res.writeHead(404, cors); res.end();
}

/** Hand one message to the view in that conversation and wait for Cursor's answer to its `ui/message`. */
export async function deliverToView(thread, message, { env = process.env, timeoutMs = Number(env.SIDEVOICE_CURSOR_APP_TIMEOUT_MS || 20_000) } = {}) {
  await ensureBridge(env);
  const mailbox = box(thread);
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
