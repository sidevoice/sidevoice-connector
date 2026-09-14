/** The last mile, one function per harness. Each takes a binding's delivery target and a room event. */
import net from 'node:net';
import { execFile } from 'node:child_process';

/** The header the skill expects before the user's literal words. */
export function envelope(event) {
  const header = { channel: event.channel === 'room-control' ? 'room-control' : 'voice',
    session_id: event.session_id, revision: event.revision, message_id: event.message_id };
  return JSON.stringify(header) + '\n\n' + event.text;
}

/** Claude Code: the session's own inbox socket, inherited by the façade that registered the binding.
 *  The inbox sends no acknowledgement, so this can never report more than what the wire showed:
 *  the peer hanging up right after the frames is the one observable sign of a refusal. */
function deliverClaude(delivery, event) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const socket = net.createConnection(delivery.socket);
    let settled = false, wrote = 0, replied = '';
    const finish = (error, status, detail) => {
      if (settled) return;
      settled = true; clearTimeout(timer); socket.destroy();
      if (error) return reject(error);
      resolve({ status, detail: `${detail} after ${Date.now() - started}ms${replied ? ', peer said ' + replied.slice(0, 120) : ''}` });
    };
    const timer = setTimeout(() => finish(null, 'unknown', 'connection still open, no acknowledgement'), 1500);
    socket.on('error', error => finish(error));
    socket.on('data', chunk => { replied += chunk; });
    socket.on('connect', () => {
      socket.write(JSON.stringify({ type: 'auth', token: delivery.token }) + '\n');
      socket.write(JSON.stringify({ type: 'user', message: { role: 'user', content: envelope(event) } }) + '\n');
      wrote = Date.now();
    });
    // A hang-up right after the frames is how a refused auth or a closed inbox looks from here.
    socket.on('close', () => finish(null, 'rejected', wrote ? 'peer closed the connection' : 'peer closed before the frames were written'));
  });
}

/** Codex: `codex queue` enqueues the next user turn on the local app-server daemon. */
function deliverCodex(delivery, event) {
  return new Promise((resolve, reject) => {
    const binary = process.env.SIDEVOICE_CODEX_BIN || 'codex';
    const args = ['queue', '--thread', delivery.thread, '--message', envelope(event)];
    execFile(binary, args, { timeout: 30_000, maxBuffer: 1 << 20 }, (error, stdout, stderr) => {
      if (error) return reject(new Error((stderr || stdout || error.message).toString().trim().slice(0, 400)));
      resolve({ status: 'accepted', detail: 'codex queue confirmed the thread' });
    });
  });
}

/** Fallback: an HTTP receiver next to the harness (the slimmed Codex Desktop bridge speaks this). */
async function deliverHttp(delivery, event) {
  const response = await fetch(delivery.url, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ thread_id: delivery.thread, text: event.text, message_id: event.message_id,
      session_id: event.session_id, revision: event.revision, channel: event.channel || 'voice' }),
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) throw new Error(`Harness delivery failed (${response.status})`);
  return { status: 'accepted', detail: `receiver answered ${response.status}` };
}

export const adapters = { 'claude-uds': deliverClaude, 'codex-queue': deliverCodex, http: deliverHttp };

export function deliver(delivery, event) {
  const adapter = adapters[delivery?.kind];
  if (!adapter) throw new Error(`Unknown delivery kind: ${delivery?.kind}`);
  return adapter(delivery, event);
}
