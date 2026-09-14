const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');

test('connector and MCP expose the four explicit voice operations', () => {
  const source = fs.readFileSync(__dirname + '/mcp.mjs', 'utf8');
  for (const name of ['voice_connect', 'voice_say', 'voice_disconnect', 'voice_status']) assert.match(source, new RegExp(name));
});
test('connector multiplexes bindings and reconnects by re-announcing them', () => {
  const source = fs.readFileSync(__dirname + '/connector.mjs', 'utf8');
  assert.match(source, /const bindings = new Map/);
  assert.match(source, /for \(const binding of bindings\.values\(\)\) send\(\{ type: 'binding\.register'/);
  assert.match(source, /ws\.addEventListener\('close', reconnect\)/);
  assert.match(source, /input\.ack/);
});
