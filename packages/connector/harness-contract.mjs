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
export const WORKING_POLL = 'poll';
export const WORKING_EVENT = 'event';
const DECLARED_STATES = new Set([SUPPORTED, UNSUPPORTED]);
const WORKING_SOURCES = new Set([WORKING_POLL, WORKING_EVENT]);

export function capabilityState(harness, capability) {
  const state = harness?.capabilities?.[capability];
  return DECLARED_STATES.has(state) ? state : UNKNOWN;
}

export function advertisedCapabilities(harness) {
  return Object.fromEntries(CAPABILITIES.map(capability => [capability, capabilityState(harness, capability)]));
}

export function defineHarness(definition) {
  if (!definition?.name) throw new Error('A harness needs a name');
  for (const capability of CAPABILITIES) {
    const state = definition.capabilities?.[capability];
    if (!DECLARED_STATES.has(state)) throw new Error(`${definition.name} must declare ${capability}`);
    if (state === SUPPORTED && typeof definition[capability] !== 'function') {
      throw new Error(`${definition.name} declares ${capability} supported but does not implement it`);
    }
  }
  if (definition.capabilities.working === SUPPORTED && !WORKING_SOURCES.has(definition.workingSource)) {
    throw new Error(`${definition.name} declares working supported but does not declare a working source`);
  }
  return Object.freeze({ ...definition, capabilities: Object.freeze({ ...definition.capabilities }) });
}

/** The header the voice skill expects before the user's literal words. */
export function envelope(event) {
  const header = {
    channel: event.channel === 'room-control' ? 'room-control' : 'voice',
    session_id: event.session_id,
    revision: event.revision,
    message_id: event.message_id,
  };
  return JSON.stringify(header) + '\n\n' + event.text;
}
