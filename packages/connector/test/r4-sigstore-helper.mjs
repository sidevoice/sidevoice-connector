import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import { readFile } from 'node:fs/promises';
import { verifyCoreArtifact } from '../core-attestation.mjs';

const [bundlePath, artifactPath, cachePath, offline] = process.argv.slice(2);
if (offline === '1') {
  const deny = () => { const error = new Error('network disabled for the TUF cache test'); error.name = 'NetworkDisabledError'; throw error; };
  globalThis.fetch = deny;
  http.request = deny; http.get = deny; https.request = deny; https.get = deny; net.connect = deny; tls.connect = deny;
}

try {
  await verifyCoreArtifact({ bytes: await readFile(artifactPath), bundleBytes: await readFile(bundlePath),
    channel: 'release', tufCachePath: cachePath, tufForceCache: offline === '1', label: 'cache fixture' });
  process.stdout.write(JSON.stringify({ ok: true }) + '\n');
} catch (error) {
  process.stdout.write(JSON.stringify({ ok: false, key: error.key ?? null, check: error.check ?? null,
    name: error.name, message: error.message }) + '\n');
}
