#!/usr/bin/env node
/** A stand-in for `uv`: records how it was called, and "installs" a `sidevoice-core` that runs the fake core. */
import { appendFileSync, chmodSync, mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
appendFileSync(process.env.FAKE_UV_LOG, JSON.stringify(args) + '\n');
if (process.env.FAKE_UV_FAIL) { console.error(process.env.FAKE_UV_FAIL_OUTPUT || 'fake uv: failing as asked'); process.exit(2); }
// What uv says while it works, and how long it takes: the first install is slow.
if (args[0] === 'pip') { console.error('Resolved 93 packages in 701ms'); console.error(' + aiortc==1.15.0'); console.error(' + uvicorn==0.53.0'); }
if (args[0] === 'pip' && process.env.FAKE_UV_DELAY_MS) await new Promise(r => setTimeout(r, Number(process.env.FAKE_UV_DELAY_MS)));
if (args[0] === 'venv') mkdirSync(path.join(args[args.length - 1], 'bin'), { recursive: true });
if (args[0] === 'pip') {
  const python = args[args.indexOf('--python') + 1];
  const core = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fake-sidevoice-core.mjs');
  const bin = path.join(path.dirname(python), 'sidevoice-core');
  // exec keeps the pid, as a console-script shebang does: the ready file's pid is the child's.
  writeFileSync(bin, `#!/bin/sh\nexec "${process.execPath}" "${core}" "$@"\n`);
  chmodSync(bin, 0o755);
}
