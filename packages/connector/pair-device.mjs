/** A one-time code for pairing a device — the desktop app, a browser — with this machine
 *  (sidevoice-core's `server/devices.py`). The node issues it and keeps the devices; this asks for one through the
 *  connector (`pair_device`, which starts the core if it is not running) and says it the way a person
 *  can use it: the code to paste, the same code as a QR, how long it is valid, and where to paste it.
 *
 *  The same words reach the person from the conversation (`voice_pair_device`) and from a terminal
 *  (`sidevoice pair-device`). Pairing is the person's act: nothing here runs unless they asked. */
import QRCode from 'qrcode';
import { connectorClient } from './ipc.mjs';
import { t } from './i18n.mjs';

/** Where a device holding this code can reach the machine: through its room (`room`), at an address that
 *  is not this computer's (`direct`), or only from this computer (`local-only`). */
export function reach(payload) {
  if (payload?.rv) return 'room';
  const loopback = url => { try { const host = new URL(url).hostname.replace(/^\[|\]$/g, ''); return host === 'localhost' || host === '::1' || /^127\./.test(host); } catch { return true; } };
  return (payload?.urls || []).some(url => !loopback(url)) ? 'direct' : 'local-only';
}

/** The code as text blocks a terminal shows as they are: two modules per character (UTF-8 half blocks). */
export function qrText(code) {
  return QRCode.toString(code, { type: 'utf8', errorCorrectionLevel: 'L', margin: 2 });
}

function validity(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return 'only a short while';
  return seconds < 120 ? `${Math.round(seconds)} seconds` : `${Math.round(seconds / 60)} minutes`;
}

/** What the person reads: the code, its QR, its validity and one line saying where it goes. */
export async function pairDeviceText({ code, payload, expires_in: expiresIn }) {
  const host = payload?.host ? ` (${payload.host})` : '';
  return [
    `One-time code to pair a device with this machine${host}, valid for ${validity(expiresIn)}:`,
    '',
    code,
    '',
    (await qrText(code)).replace(/\n+$/, ''),
    '',
    'Paste it in the Sidevoice app under Máquinas → Emparejar.',
  ].join('\n');
}

/** Ask this machine's core for a code, through the connector. Returns the answer and its text. */
export async function pairDevice(rpc) {
  const answer = await rpc('pair_device', {});
  if (typeof answer?.code !== 'string' || !answer.code) throw new Error('This machine\'s core answered without a pairing code.');
  return { ...answer, text: await pairDeviceText(answer) };
}

/** `sidevoice pair-device [--json]`: through the launcher, like any façade. */
export async function run(argv = [], env = process.env) {
  const json = argv.includes('--json');
  const connector = connectorClient(env);
  try {
    const answer = await pairDevice(connector.rpc);
    const where = reach(answer.payload);
    if (json) console.log(JSON.stringify({ ok: true, code: answer.code, expires_in: answer.expires_in, reach: where, payload: answer.payload }));
    else {
      console.log(answer.text);
      if (where === 'local-only') console.log('\n' + t('pair-device.local-only'));
    }
    return 0;
  } catch (error) {
    if (json) console.log(JSON.stringify({ ok: false, error: { key: error.key || 'pair-device.failed', message: error.message } }));
    else console.error(error.message);
    return 1;
  } finally { connector.end(); }
}
