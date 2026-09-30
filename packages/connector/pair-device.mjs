#!/usr/bin/env node
/** A one-time code for pairing a device — the desktop app, a browser — with this machine
 *  (`docs/DEVICE_PAIRING.md`). The node issues it and keeps the devices; this asks for one through the
 *  connector (`pair_device`, which starts the core if it is not running) and says it the way a person
 *  can use it: the code to paste, the same code as a QR, how long it is valid, and where to paste it.
 *
 *  The same words reach the person from the conversation (`voice_pair_device`) and from a terminal
 *  (`sidevoice pair-device`). Pairing is the person's act: nothing here runs unless they asked. */
import QRCode from 'qrcode';
import { connectorClient } from './ipc.mjs';

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

if (process.env.SIDEVOICE_PAIR_DEVICE_MAIN === '1') {
  const connector = connectorClient();
  try {
    console.log((await pairDevice(connector.rpc)).text);
    connector.end();
  } catch (error) { console.error(error.message); connector.end(); process.exit(1); }
}
