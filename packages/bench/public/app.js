// The bench UI. Plain DOM, no build step. Every text a person reads is an i18n key (messages/<lang>.json, English
// the fallback); data from the connector is shown with textContent, never parsed as HTML.

const SUPPORTED = ['en', 'es'];
const HANDSHAKE = new Set(['connector.hello', 'connector.welcome', 'node.rendezvous']);
const FRAME_LIMIT = 1000;
let messages = {};
let fallback = {};
let state = null;
let selectedThread = localStorage.getItem('bench.thread') || '';
const frames = [];
const spoken = new Set();
const methodsById = new Map(); // request id → method, to label responses

// ---- i18n ----------------------------------------------------------------------------------------------------

function pickLanguage() {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const lang = String(tag).toLowerCase().split('-')[0];
    if (SUPPORTED.includes(lang)) return lang;
  }
  return 'en';
}

async function loadMessages() {
  const lang = pickLanguage();
  fallback = await (await fetch('/messages/en.json')).json();
  messages = lang === 'en' ? fallback : await (await fetch(`/messages/${lang}.json`)).json();
  document.documentElement.lang = lang;
}

function tr(key, params = {}) {
  const text = messages[key] ?? fallback[key] ?? key;
  return text.replace(/\{(\w+)\}/g, (whole, name) => (name in params ? String(params[name]) : whole));
}

function translateStatic() {
  for (const el of document.querySelectorAll('[data-i18n]')) el.textContent = tr(el.dataset.i18n);
  for (const el of document.querySelectorAll('[data-i18n-placeholder]')) el.placeholder = tr(el.dataset.i18nPlaceholder);
  document.title = tr('app.title');
}

// ---- helpers -------------------------------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') node.className = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (key === 'text') node.textContent = value;
    else node.setAttribute(key, value);
  }
  for (const child of children.flat()) if (child != null) node.append(child);
  return node;
}

function pill(text, tone = '') {
  return el('span', { class: `pill ${tone}`, text });
}

const time = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });

async function post(action, body = {}) {
  const res = await fetch(`/api/${action}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-sidevoice-bench': '1' },
    body: JSON.stringify(body),
  });
  const value = await res.json();
  if (!res.ok) throw Object.assign(new Error(value.error ?? res.statusText), { value });
  return value;
}

function showResult(value) {
  $('command-result').textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

async function run(action, body) {
  try {
    const value = await post(action, body);
    showResult(value);
    return value;
  } catch (error) {
    const value = error.value ?? {};
    const reason = value.key ? tr(`error.${value.key}`, value.params) : value.rpc ? JSON.stringify(value.rpc) : error.message;
    showResult(tr('error.request', { error: reason }));
    return null;
  }
}

// ---- rendering -----------------------------------------------------------------------------------------------

const RECEIPT_TONE = { pending: 'warn', delivered: 'ok', read: 'ok', not_sent: 'bad', unconfirmed: 'warn' };
const CAP_TONE = { supported: 'ok', unsupported: 'bad', unknown: 'warn' };

function renderStatus() {
  const box = $('status');
  box.replaceChildren();
  const c = state.connector;
  box.append(
    c
      ? pill(tr('status.linked', { host: c.host ?? '?', version: c.version ?? '?', platform: c.platform ?? '?' }), 'ok')
      : pill(tr('status.notLinked'), 'bad'),
  );
  const p = state.process;
  if (!p.binary) box.append(pill(tr('process.none'), 'warn'));
  else box.append(pill(p.running ? tr('process.running', { pid: p.pid }) : tr('process.stopped'), p.running ? 'ok' : 'warn'));
  if (p.restarts) box.append(pill(tr('process.restarts', { count: p.restarts })));
  box.append(pill(tr('session.label', { id: state.session_id.slice(0, 8) })));
  box.append(pill(tr('status.profile', { root: state.profile })));
  const rv = state.rendezvous;
  $('rendezvous-state').textContent = rv.refused
    ? tr('rendezvous.isRefused', { reason: rv.refused })
    : rv.connected
      ? tr('rendezvous.isConnected', { room: rv.room })
      : tr('rendezvous.isDisconnected');
}

function field(label, value) {
  return [el('dt', { text: tr(label) }), el('dd', {}, value)];
}

function renderBindings() {
  const box = $('bindings');
  box.replaceChildren();
  const showGone = $('show-gone').checked;
  const list = state.bindings.filter((b) => showGone || b.live);
  if (!list.length) box.append(el('p', { class: 'muted', text: tr('bindings.none') }));
  for (const b of list) {
    const working = b.working == null ? pill(tr('working.unknown')) : b.working ? pill(tr('working.yes'), 'warn') : pill(tr('working.no'), 'ok');
    const caps = el(
      'div',
      { class: 'caps' },
      Object.entries(b.capabilities).map(([name, value]) =>
        pill(`${name}${b.experimental.includes(name) ? '*' : ''}`, CAP_TONE[value]),
      ),
    );
    const card = el(
      'div',
      {
        class: `binding${b.thread === selectedThread ? ' selected' : ''}${b.live ? '' : ' gone'}`,
        onclick: () => selectThread(b.thread),
      },
      el(
        'div',
        { class: 'row' },
        el('strong', { text: b.title || b.thread }),
        pill(b.harness),
        pill(b.input_mode === 'pull' ? tr('mode.pull') : tr('mode.push'), b.input_mode === 'pull' ? 'warn' : 'ok'),
        b.live ? pill(tr('bindings.live'), 'ok') : pill(tr('bindings.gone'), 'bad'),
        working,
      ),
      el(
        'dl',
        {},
        field('bindings.thread', el('code', { text: b.thread })),
        field('bindings.bindingId', el('code', { text: b.binding_id })),
        field('bindings.capabilities', caps),
        field('bindings.delivery', b.delivery ? b.delivery.kind ?? JSON.stringify(b.delivery) : '—'),
        field('bindings.route', b.route ?? '—'),
        field('bindings.engine', b.engine ? [b.engine.model, b.engine.effort, b.engine.thinking].filter(Boolean).join(' · ') : '—'),
        field('bindings.turn', b.turn ? `${b.turn.turn_id ?? '—'} ${b.turn.turn_phase ?? ''} @ ${time(b.turn.at)}` : '—'),
        field('bindings.inbound', b.inbound ? JSON.stringify(b.inbound) : '—'),
        field('bindings.registered', time(b.registered_at)),
      ),
      b.live
        ? el('button', {
            text: tr('bindings.close'),
            onclick: (event) => {
              event.stopPropagation();
              run('close', { binding_id: b.binding_id });
            },
          })
        : null,
    );
    box.append(card);
  }
}

function speak(text, language) {
  if (!('speechSynthesis' in window)) return;
  const utterance = new SpeechSynthesisUtterance(text);
  if (language) utterance.lang = language;
  speechSynthesis.speak(utterance);
}

function renderTimeline() {
  $('thread-label').textContent = selectedThread || tr('conversation.noThread');
  $('thread-input').value ||= selectedThread;
  const box = $('timeline');
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  box.replaceChildren();
  const items = [
    ...state.messages.filter((m) => m.thread === selectedThread).map((m) => ({ kind: 'user', at: m.created_at, m })),
    ...state.speech.filter((s) => s.thread === selectedThread).map((s) => ({ kind: 'agent', at: s.at, s })),
  ].sort((a, b) => a.at - b.at);
  if (!items.length) box.append(el('p', { class: 'muted', text: tr('conversation.empty') }));
  for (const item of items) {
    if (item.kind === 'user') {
      const { m } = item;
      const history = m.receipts.map((r) => `${tr(`receipt.${r.status}`)} · ${r.via}${r.detail ? ` (${r.detail})` : ''} · ${time(r.at)}`).join('\n');
      box.append(
        el(
          'div',
          { class: 'bubble user', title: history },
          el('div', { text: m.text }),
          el(
            'div',
            { class: 'meta' },
            pill(tr(`receipt.${m.status}`), RECEIPT_TONE[m.status]),
            m.in_flight ? pill(tr('receipt.inFlight'), 'warn') : null,
            el('span', { text: tr('conversation.revision', { revision: m.revision, attempts: m.attempts }) }),
            el('span', { text: time(m.created_at) }),
          ),
        ),
      );
    } else {
      const { s } = item;
      box.append(
        el(
          'div',
          { class: 'bubble agent' },
          el('div', { text: s.text }),
          el(
            'div',
            { class: 'meta' },
            pill(s.status),
            s.language ? pill(s.language) : null,
            el('span', { text: tr('conversation.answers', { revision: s.revision }) }),
            el('span', { text: time(s.at) }),
            el('button', { text: tr('conversation.play'), onclick: () => speak(s.text, s.language) }),
          ),
        ),
      );
    }
  }
  if (atBottom) box.scrollTop = box.scrollHeight;
}

function renderPolicies() {
  const box = $('policies');
  if (box.dataset.ready && document.activeElement?.closest('#policies')) return;
  box.replaceChildren();
  box.dataset.ready = '1';
  for (const [method, choices] of Object.entries(state.policy_choices)) {
    const select = el('select', { onchange: () => run('policy', { method, mode: select.value }) });
    for (const choice of choices) {
      const option = el('option', { value: choice, text: tr(`policy.${choice}`) });
      if (state.policies[method] === choice) option.selected = true;
      select.append(option);
    }
    box.append(el('div', { class: 'policy' }, el('code', { text: method }), select));
  }
}

function renderSetup() {
  const box = $('setup');
  if (box.dataset.ready) return;
  box.dataset.ready = '1';
  const s = state.setup;
  const item = (label, command) =>
    command
      ? el(
          'div',
          { class: 'setup-item' },
          el('div', { class: 'row' }, el('strong', { text: tr(label) }), el('button', { text: tr('setup.copy'), onclick: () => navigator.clipboard?.writeText(command) })),
          el('pre', { text: command }),
        )
      : null;
  box.append(
    item('setup.claude', s.claude.run),
    item('setup.codexLogin', s.codex.login),
    item('setup.codexRegister', s.codex.register),
    item('setup.codexRun', s.codex.run),
    item('setup.codexCleanup', s.codex.cleanup),
    item('setup.cursor', s.cursor.run),
    item('setup.http', s.http.run),
    item('setup.reset', s.reset),
  );
}

function frameMethod(frame) {
  if (!frame || typeof frame !== 'object') return '';
  if (frame.method) {
    if (frame.id != null) methodsById.set(`${frame.id}`, frame.method);
    return frame.method;
  }
  return frame.id != null ? `↳ ${methodsById.get(`${frame.id}`) ?? frame.id}` : '';
}

function frameRow(entry) {
  const arrow = entry.direction === 'in' ? '→' : entry.direction === 'out' ? '←' : '•';
  const method = entry.frame ? frameMethod(entry.frame) : entry.note;
  const json = entry.frame == null ? entry.note : JSON.stringify(entry.frame, null, 2);
  const row = el(
    'div',
    { class: `frame ${entry.direction}`, title: tr(`frames.${entry.direction}`) },
    el('span', { class: 'muted', text: time(entry.at) }),
    el('span', { text: arrow }),
    el('details', {}, el('summary', { text: `${method} ${entry.frame ? JSON.stringify(entry.frame.params ?? entry.frame.result ?? entry.frame.error ?? '') : ''}` }), el('pre', { text: json })),
  );
  row.dataset.method = method ?? '';
  return row;
}

function frameVisible(row) {
  const filter = $('frame-filter').value.trim().toLowerCase();
  const method = row.dataset.method.replace('↳ ', '');
  if ($('hide-handshake').checked && HANDSHAKE.has(method)) return false;
  return !filter || row.textContent.toLowerCase().includes(filter);
}

function addFrame(entry) {
  frames.push(entry);
  if (frames.length > FRAME_LIMIT) frames.shift();
  if ($('pause-frames').checked) return;
  const box = $('frames');
  const row = frameRow(entry);
  row.hidden = !frameVisible(row);
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  box.append(row);
  while (box.children.length > FRAME_LIMIT) box.firstChild.remove();
  if (atBottom) box.scrollTop = box.scrollHeight;
}

function refilterFrames() {
  const box = $('frames');
  box.replaceChildren(...frames.map(frameRow));
  for (const row of box.children) row.hidden = !frameVisible(row);
  box.scrollTop = box.scrollHeight;
}

function addLog(entry) {
  const box = $('connector-log');
  box.textContent += `${time(entry.at)} [${entry.stream}] ${entry.line}\n`;
  box.scrollTop = box.scrollHeight;
}

function render() {
  if (!state) return;
  if (!selectedThread) {
    const first = state.bindings.find((b) => b.live);
    if (first) selectedThread = first.thread;
  }
  renderStatus();
  renderBindings();
  renderTimeline();
  renderPolicies();
  renderSetup();
}

function selectThread(thread) {
  selectedThread = thread;
  localStorage.setItem('bench.thread', thread);
  $('thread-input').value = thread;
  render();
}

// ---- wiring --------------------------------------------------------------------------------------------------

function wire() {
  $('composer').addEventListener('submit', async (event) => {
    event.preventDefault();
    const thread = $('thread-input').value.trim();
    const text = $('text-input').value;
    if (!thread || !text.trim()) return;
    if (!(await run('say', { thread, text }))) return;
    $('text-input').value = '';
    if (thread !== selectedThread) selectThread(thread);
  });
  $('text-input').addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      $('composer').requestSubmit();
    }
  });
  $('connector-start').addEventListener('click', () => run('connector', { action: 'start' }));
  $('connector-stop').addEventListener('click', () => run('connector', { action: 'stop' }));
  $('new-session').addEventListener('click', () => run('session'));
  $('drop-link').addEventListener('click', () => run('drop'));
  $('show-gone').addEventListener('change', renderBindings);
  $('auto-speak').checked = localStorage.getItem('bench.autoSpeak') === '1';
  $('auto-speak').addEventListener('change', () => localStorage.setItem('bench.autoSpeak', $('auto-speak').checked ? '1' : '0'));
  for (const button of document.querySelectorAll('[data-command]')) {
    button.addEventListener('click', () => {
      const method = button.dataset.command;
      let params = {};
      if (method === 'agents.list') params = { rescan: true };
      else if (method.startsWith('agents.')) params = { id: $('agent-id').value };
      else if (method === 'pair.request') params = { room: $('pair-room').value, code: $('pair-code').value };
      run('command', { method, params, timeout_ms: method === 'pair.request' ? 25_000 : 20_000 });
    });
  }
  $('rv-connected').addEventListener('click', () => run('rendezvous', { connected: true, refused: null, error: null }));
  $('rv-disconnected').addEventListener('click', () => run('rendezvous', { connected: false, refused: null, error: 'bench' }));
  $('rv-refused').addEventListener('click', () => run('rendezvous', { connected: false, refused: 'connector_revoked' }));
  const raw = () => {
    try {
      return JSON.parse($('raw-input').value);
    } catch (error) {
      showResult(tr('raw.invalid', { error: error.message }));
      return null;
    }
  };
  $('raw-request').addEventListener('click', () => {
    const value = raw();
    if (value) run('command', { method: value.method, params: value.params ?? {} });
  });
  $('raw-notify').addEventListener('click', () => {
    const value = raw();
    if (value) run('command', { method: value.method, params: value.params ?? {}, notify: true });
  });
  $('raw-frame').addEventListener('click', () => {
    const value = raw();
    if (value) run('raw', { frame: value });
  });
  $('frame-filter').addEventListener('input', refilterFrames);
  $('hide-handshake').addEventListener('change', refilterFrames);
  $('pause-frames').addEventListener('change', () => !$('pause-frames').checked && refilterFrames());
  $('clear-frames').addEventListener('click', () => {
    frames.length = 0;
    $('frames').replaceChildren();
  });
}

async function connect() {
  for (const entry of await (await fetch('/api/frames')).json()) addFrame(entry);
  for (const entry of await (await fetch('/api/connector-log')).json()) addLog(entry);
  const events = new EventSource('/api/events');
  events.addEventListener('state', (event) => {
    state = JSON.parse(event.data);
    for (const s of state.speech) spoken.add(s.utterance_id); // only speech that arrives from now on is spoken
    render();
  });
  events.addEventListener('frame', (event) => addFrame(JSON.parse(event.data)));
  events.addEventListener('log', (event) => addLog(JSON.parse(event.data)));
  events.addEventListener('speech', (event) => {
    const item = JSON.parse(event.data);
    if (spoken.has(item.utterance_id)) return;
    spoken.add(item.utterance_id);
    if ($('auto-speak').checked) speak(item.text, item.language);
  });
}

await loadMessages();
translateStatic();
wire();
await connect();
