/** The harness boundary. A known harness declares every capability; callers never infer support
 *  from a missing method. Missing or malformed declarations remain unknown, never false. */

export const CAPABILITIES = Object.freeze([
  'deliver',
  'inspectInbound',
  'working',
  'endOfTurn',
  'sessionIdentity',
]);

export const SUPPORTED = 'supported';
export const UNSUPPORTED = 'unsupported';
export const UNKNOWN = 'unknown';
const DECLARED_STATES = new Set([SUPPORTED, UNSUPPORTED]);

export function capabilityState(harness, capability) {
  const state = harness?.capabilities?.[capability];
  return DECLARED_STATES.has(state) ? state : UNKNOWN;
}

export function advertisedCapabilities(harness) {
  return Object.fromEntries(CAPABILITIES.map(capability => [capability, capabilityState(harness, capability)]));
}

/** A module may mark supported capabilities `experimental`: they work, by a route the harness does not offer
 *  as an interface (typing into its terminal, driving its UI), and the person is told so wherever the
 *  capability is shown. Anything not declared supported is never experimental. */
export function experimentalCapabilities(harness, capabilities = advertisedCapabilities(harness), identity = null) {
  // The module's own marks, and those its identity adds for one conversation (an editor chat of Cursor is
  // identified by a route the module does not use for the CLI).
  const declared = [...(Array.isArray(harness?.experimental) ? harness.experimental : []), ...(Array.isArray(identity?.experimental) ? identity.experimental : [])];
  return CAPABILITIES.filter(capability => declared.includes(capability) && capabilities[capability] === SUPPORTED);
}

/** What one conversation can do: the module's declaration, narrowed by what its identity found out about
 *  that conversation (a Cursor chat not running under `persist` cannot take input though the module can).
 *  A conversation can only lose a capability here, never gain one. */
export function conversationCapabilities(harness, identity) {
  const declared = advertisedCapabilities(harness);
  for (const [capability, state] of Object.entries(identity?.capabilities || {})) {
    if (declared[capability] === SUPPORTED && state === UNSUPPORTED) declared[capability] = UNSUPPORTED;
  }
  return declared;
}

/** `working` and `endOfTurn` are both answered by observation: a harness that supports either
 *  implements `observe(thread, handlers)`, which watches what the harness itself writes about that
 *  conversation and calls back — `working(bool, { turn_id })` on every transition it can see, and
 *  `userMessage({ text, turn_id })` for every user message the conversation admits. It returns a
 *  function that stops watching. Nothing is installed in the harness for this to work. */
export function defineHarness(definition) {
  if (!definition?.name) throw new Error('A harness needs a name');
  for (const capability of CAPABILITIES) {
    const state = definition.capabilities?.[capability];
    if (!DECLARED_STATES.has(state)) throw new Error(`${definition.name} must declare ${capability}`);
    const implemented = capability === 'working' || capability === 'endOfTurn' ? definition.observe : definition[capability];
    if (state === SUPPORTED && typeof implemented !== 'function') {
      throw new Error(`${definition.name} declares ${capability} supported but does not implement it`);
    }
  }
  for (const capability of definition.experimental || []) {
    if (definition.capabilities[capability] !== SUPPORTED) throw new Error(`${definition.name} marks ${capability} experimental but does not support it`);
  }
  return Object.freeze({ ...definition, capabilities: Object.freeze({ ...definition.capabilities }),
    ...(definition.experimental ? { experimental: Object.freeze([...definition.experimental]) } : {}) });
}

/** The header before the user's literal words, and — for a voice message — the note after them that
 *  asks the conversation to speak first. The note travels inside the message because it is the only
 *  thing every harness delivers without anything installed; the instructions name it as not the user's. */
export function envelope(event) {
  const header = {
    channel: event.channel === 'room-control' ? 'room-control' : 'voice',
    session_id: event.session_id,
    revision: event.revision,
    message_id: event.message_id,
  };
  const note = header.channel === 'voice' ? '\n\n' + nudge(header) : '';
  return JSON.stringify(header) + '\n\n' + event.text + note;
}

/** What the conversation is asked at the moment it reads a voice message: one line that points at the
 *  server's instructions, which are already in context — not a copy of them. The harness adds its own
 *  wrapper around a cross-session message; ours stays small. */
export function nudge(header) {
  return `[Sidevoice] Voice from the room: acknowledge with voice_say (session_id "${header.session_id}", revision ${header.revision}) before any other tool, then work and reply by voice, as the sidevoice server's instructions say.`;
}

/** The header at the front of a delivered message, or null when the text is not one of ours. Works on
 *  the text as a harness stores it, which may put its own line before the header. */
export function voiceEnvelope(text) {
  if (typeof text !== 'string') return null;
  const start = text.indexOf('{"channel":');
  if (start < 0) return null;
  const end = text.indexOf('}', start);
  if (end < 0) return null;
  let header;
  try { header = JSON.parse(text.slice(start, end + 1)); } catch { return null; }
  if (!header || (header.channel !== 'voice' && header.channel !== 'room-control') || !header.message_id || !header.session_id || !Number.isInteger(header.revision)) return null;
  return { channel: header.channel, message_id: header.message_id, session_id: header.session_id, revision: header.revision };
}

/** Follows a JSON-lines file the harness appends to: each complete new line is handed to `onLine` as
 *  parsed JSON. From where the file is now, unless `catchUp` is set — then what is already there is
 *  read first, with `replayed = true`, so a watcher can learn the current state without mistaking old
 *  lines for news. The file may not exist yet — `locate` is asked again until it does.
 *
 *  A harness that rewrites the file in place instead of only appending sets `rewrites`: the last bytes
 *  read are checked before reading on, and a file that no longer ends where it did is read again from the
 *  top as a replay — `rewound()` first, `caughtUp()` after — rather than from the middle of new text. A
 *  rewrite that truncates and then writes can be seen half done, so that replay lasts until a read ends on
 *  a complete line. */
export function tailJsonl(locate, onLine, { intervalMs = 400, catchUp = false, caughtUp = () => {}, rewrites = false, rewound = () => {} } = {}) {
  let file = null, offset = null, remainder = '', replaying = false, rewinding = false, tail = Buffer.alloc(0);
  const poll = async () => {
    const { statSync, openSync, readSync, closeSync } = await import('node:fs');
    if (!file) { file = locate(); if (!file) return; }
    let size;
    try { size = statSync(file).size; } catch { file = null; offset = null; tail = Buffer.alloc(0); return; }
    if (offset === null) {
      if (!catchUp) { offset = size; return; }                // start at the end: only what happens from now on
      offset = 0;
      // An empty file has no past to replay: what arrives next is news.
      if (size === 0) { try { caughtUp(); } catch {} return; }
      replaying = true;
    }
    if (size === offset) return;
    const fd = openSync(file, 'r');
    try {
      if (rewrites && offset > 0 && (size < offset || !sameTail(fd, offset, tail, readSync))) {
        offset = 0; remainder = ''; tail = Buffer.alloc(0); replaying = true; rewinding = true;
        try { rewound(); } catch {}
      } else if (size < offset) { offset = 0; remainder = ''; }   // rewritten: read it again from the top
      const buffer = Buffer.alloc(size - offset);
      readSync(fd, buffer, 0, buffer.length, offset);
      offset = size;
      if (rewrites) tail = Buffer.concat([tail, buffer]).subarray(-64);
      remainder += buffer.toString('utf8');
    } finally { closeSync(fd); }
    let index;
    while ((index = remainder.indexOf('\n')) >= 0) {
      const line = remainder.slice(0, index); remainder = remainder.slice(index + 1);
      if (!line.trim()) continue;
      let parsed; try { parsed = JSON.parse(line); } catch { continue; }
      try { onLine(parsed, replaying); } catch {}
    }
    if (replaying && !(rewinding && (offset === 0 || remainder !== ''))) { replaying = false; rewinding = false; try { caughtUp(); } catch {} }
  };
  const timer = setInterval(() => { poll().catch(() => {}); }, intervalMs);
  timer.unref?.();
  poll().catch(() => {});
  return () => clearInterval(timer);
}

/** Whether the bytes that ended the file when it was last read are still there. */
function sameTail(fd, offset, tail, readSync) {
  if (!tail.length) return true;
  const now = Buffer.alloc(tail.length);
  readSync(fd, now, 0, now.length, offset - tail.length);
  return now.equals(tail);
}
