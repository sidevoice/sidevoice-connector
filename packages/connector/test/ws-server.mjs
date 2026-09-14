/** Minimal RFC 6455 text-frame WebSocket server for tests: handshake, masked client frames, unmasked server frames. */
import http from 'node:http';
import { createHash } from 'node:crypto';

export function createWsServer(onConnection) {
  const server = http.createServer((req, res) => { res.writeHead(404); res.end(); });
  server.on('upgrade', (req, socket) => {
    const key = req.headers['sec-websocket-key'];
    const accept = createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
    const conn = { socket, send(text) { const payload = Buffer.from(text); const len = payload.length; let header; if (len < 126) header = Buffer.from([0x81, len]); else if (len < 65536) { header = Buffer.alloc(4); header[0] = 0x81; header[1] = 126; header.writeUInt16BE(len, 2); } else { header = Buffer.alloc(10); header[0] = 0x81; header[1] = 127; header.writeBigUInt64BE(BigInt(len), 2); } socket.write(Buffer.concat([header, payload])); }, close() { socket.write(Buffer.from([0x88, 0])); socket.end(); }, onMessage: null };
    let buffer = Buffer.alloc(0);
    socket.on('data', chunk => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        if (buffer.length < 2) return;
        const opcode = buffer[0] & 0x0f; const masked = buffer[1] & 0x80; let len = buffer[1] & 0x7f; let offset = 2;
        if (len === 126) { if (buffer.length < 4) return; len = buffer.readUInt16BE(2); offset = 4; }
        else if (len === 127) { if (buffer.length < 10) return; len = Number(buffer.readBigUInt64BE(2)); offset = 10; }
        const maskKey = masked ? buffer.subarray(offset, offset + 4) : null; if (masked) offset += 4;
        if (buffer.length < offset + len) return;
        let payload = buffer.subarray(offset, offset + len); buffer = buffer.subarray(offset + len);
        if (masked) { const out = Buffer.alloc(len); for (let i = 0; i < len; i++) out[i] = payload[i] ^ maskKey[i % 4]; payload = out; }
        if (opcode === 0x8) { socket.end(); return; }
        if (opcode === 0x9) { socket.write(Buffer.concat([Buffer.from([0x8a, payload.length]), payload])); continue; }
        if (opcode === 0x1 && conn.onMessage) conn.onMessage(payload.toString());
      }
    });
    socket.on('error', () => {});
    onConnection(conn);
  });
  return server;
}
