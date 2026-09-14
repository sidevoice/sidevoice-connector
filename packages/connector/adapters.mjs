/** The last mile, one function per harness. Each takes a binding's delivery target and a room event. */
import net from 'node:net';
import { execFile } from 'node:child_process';

/** The header the skill expects before the user's literal words. */
export function envelope(event) {
  const header = { channel: event.channel === 'room-control' ? 'room-control' : 'voice',
    session_id: event.session_id, revision: event.revision, message_id: event.message_id };
  return JSON.stringify(header) + '\n\n' + event.text;
}

/** Claude Code: the session's own inbox socket, inherited by the façade that registered the binding. */
function deliverClaude(delivery, event) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(delivery.socket);
    let settled = false;
    const finish = error => { if (settled) return; settled = true; clearTimeout(timer); socket.destroy(); error ? reject(error) : resolve(); };
    const timer = setTimeout(() => finish(), 1500); // The inbox writes nothing back; silence after the frames is success.
    socket.on('error', finish);
    socket.on('connect', () => {
      socket.write(JSON.stringify({ type: 'auth', token: delivery.token }) + '\n');
      socket.write(JSON.stringify({ type: 'user', message: { role: 'user', content: envelope(event) } }) + '\n');
    });
    socket.on('close', () => finish());
  });
}

/** Codex: `codex queue` enqueues the next user turn on the local app-server daemon. */
function deliverCodex(delivery, event) {
  return new Promise((resolve, reject) => {
    const binary = process.env.SIDEVOICE_CODEX_BIN || 'codex';
    const args = ['queue', '--thread', delivery.thread, '--message', envelope(event)];
    execFile(binary, args, { timeout: 30_000, maxBuffer: 1 << 20 }, (error, stdout, stderr) => {
      if (error) return reject(new Error((stderr || stdout || error.message).toString().trim().slice(0, 400)));
      resolve();
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
}

export const adapters = { 'claude-uds': deliverClaude, 'codex-queue': deliverCodex, http: deliverHttp };

export function deliver(delivery, event) {
  const adapter = adapters[delivery?.kind];
  if (!adapter) throw new Error(`Unknown delivery kind: ${delivery?.kind}`);
  return adapter(delivery, event);
}
