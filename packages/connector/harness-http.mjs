/** Generic HTTP harness used by explicitly configured external receivers. */
import { defineHarness, SUPPORTED, UNSUPPORTED } from './harness-contract.mjs';

async function deliver(delivery, event) {
  if (delivery?.kind !== 'http') throw new Error(`Unsupported HTTP delivery kind: ${delivery?.kind}`);
  const response = await fetch(delivery.url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ thread_id: delivery.thread, text: event.text, message_id: event.message_id,
      session_id: event.session_id, revision: event.revision, channel: event.channel || 'voice' }),
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) throw new Error(`Harness delivery failed (${response.status})`);
  return { status: 'accepted', detail: `receiver answered ${response.status}` };
}

function sessionIdentity({ env = process.env } = {}) {
  if (!env.SIDEVOICE_THREAD || !env.SIDEVOICE_DELIVERY_URL) return null;
  return {
    harness: env.SIDEVOICE_HARNESS || 'http',
    thread: env.SIDEVOICE_THREAD,
    delivery: { kind: 'http', url: env.SIDEVOICE_DELIVERY_URL, thread: env.SIDEVOICE_THREAD },
  };
}

export const httpHarness = defineHarness({
  name: 'http',
  capabilities: {
    deliver: SUPPORTED,
    inspectInbound: UNSUPPORTED,
    working: UNSUPPORTED,
    endOfTurn: UNSUPPORTED,
    sessionIdentity: SUPPORTED,
  },
  deliver,
  sessionIdentity,
});

export default httpHarness;
