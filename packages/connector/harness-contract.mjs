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
  return Object.freeze({ ...definition, capabilities: Object.freeze({ ...definition.capabilities }) });
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

/** What the conversation is asked at the moment it reads a voice message. */
export function nudge(header) {
  return `[Sidevoice] A voice message from the room (session_id "${header.session_id}", revision ${header.revision}). `
    + 'Before any other tool, publish a short spoken acknowledgement with voice_say that says what you understood and what you will do next, '
    + 'using that session_id and revision; then continue the work and publish the result by voice as well.';
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
 *  lines for news. The file may not exist yet — `locate` is asked again until it does. */
export function tailJsonl(locate, onLine, { intervalMs = 400, catchUp = false, caughtUp = () => {} } = {}) {
  let file = null, offset = null, remainder = '', replaying = false;
  const poll = async () => {
    const { statSync, openSync, readSync, closeSync } = await import('node:fs');
    if (!file) { file = locate(); if (!file) return; }
    let size;
    try { size = statSync(file).size; } catch { file = null; offset = null; return; }
    if (offset === null) {
      if (!catchUp) { offset = size; return; }                // start at the end: only what happens from now on
      offset = 0; replaying = true;
    }
    if (size < offset) { offset = 0; remainder = ''; }        // rewritten: read it again from the top
    if (size === offset) return;
    const fd = openSync(file, 'r');
    try {
      const buffer = Buffer.alloc(size - offset);
      readSync(fd, buffer, 0, buffer.length, offset);
      offset = size;
      remainder += buffer.toString('utf8');
    } finally { closeSync(fd); }
    let index;
    while ((index = remainder.indexOf('\n')) >= 0) {
      const line = remainder.slice(0, index); remainder = remainder.slice(index + 1);
      if (!line.trim()) continue;
      let parsed; try { parsed = JSON.parse(line); } catch { continue; }
      try { onLine(parsed, replaying); } catch {}
    }
    if (replaying) { replaying = false; try { caughtUp(); } catch {} }
  };
  const timer = setInterval(() => { poll().catch(() => {}); }, intervalMs);
  timer.unref?.();
  poll().catch(() => {});
  return () => clearInterval(timer);
}
