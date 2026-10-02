#!/usr/bin/env node
/** `sidevoice <command> …` — one bin, one entry per command.
 *
 *  Every command module exports what runs it (`run(argv)`, or a named one where a module holds two) and does
 *  nothing on import: this file imports them all and calls the one asked for. Named statically so the bundler
 *  sees them: published, this file and everything it reaches are one file in `dist/`, and an import it could not
 *  resolve at build time would be a path that does not exist at run time. No top-level await — the single
 *  executable (R4) is a CommonJS build, where there is none. */
import { run as connector } from './connector.mjs';
import { runInstall, runRollback, runUninstall } from './install.mjs';
import { run as mcp } from './mcp.mjs';
import { runLinkRoom, runPair } from './pair.mjs';
import { run as pairDevice } from './pair-device.mjs';
import { run as service } from './service.mjs';
import { run as skill } from './skill.mjs';
import { t } from './i18n.mjs';
import { VERSION } from './identity.mjs';
import { realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { cursorDatabaseMatches } from './harness-cursor-desktop.mjs';
import { verifyCoreArtifact } from './core-attestation.mjs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import { readFile } from 'node:fs/promises';
import { stableCommand } from './release.mjs';
import { connectorMetadata, versionMetadata } from './metadata.mjs';
import { pause } from './testpoint.mjs';

const COMMANDS = { install: runInstall, uninstall: runUninstall, rollback: runRollback, mcp, pair: runPair, 'link-room': runLinkRoom,
  'pair-device': pairDevice, connector, service, skill };

/** With `--json`, every failure is one object on stdout — `{ok: false, error: {key, message}}` — and exit 1. */
const failJson = (key, message) => { console.log(JSON.stringify({ ok: false, error: { key, message } })); return 1; };

export async function main([command, ...argv] = process.argv.slice(2)) {
  // What an installer asks a staged release (`release.mjs`): this package's version, and nothing else.
  if (command === '--version') {
    if (argv.includes('--json')) console.log(JSON.stringify(versionMetadata()));
    else console.log(VERSION);
    return 0;
  }
  if (command === 'metadata') {
    console.log(JSON.stringify(connectorMetadata()));
    return 0;
  }
  if (command === '--sidevoice-cursor-db-query') {
    try { process.stdout.write(JSON.stringify(await cursorDatabaseMatches(...argv))); return 0; }
    catch (error) { console.error(error?.message || error); return 1; }
  }
  if (command === '--sidevoice-selected-command') {
    try { console.log(JSON.stringify(stableCommand(process.env))); return 0; }
    catch (error) { console.log(JSON.stringify({ error: error.key ?? 'install.failed' })); return 1; }
  }
  // Exercise the same bundled Sigstore path production core installation uses. This private command intentionally
  // returns only machine-readable verification outcomes; the public verification test uses a genuine unrelated bundle.
  if (command === '--sidevoice-verify-core') {
    const [artifactPath, bundlePath, channel, tufCachePath, offline, expectedSha256] = argv;
    if (!artifactPath || !bundlePath || !channel || !tufCachePath) return 2;
    if (offline === '1') {
      const deny = () => { const error = new Error('network disabled for the TUF cache test'); error.name = 'NetworkDisabledError'; throw error; };
      globalThis.fetch = deny;
      http.request = deny; http.get = deny; https.request = deny; https.get = deny; net.connect = deny; tls.connect = deny;
    }
    try {
      await pause('verify-core');
      await verifyCoreArtifact({ bytes: await readFile(artifactPath), bundleBytes: await readFile(bundlePath),
        expectedSha256: expectedSha256 || undefined, channel, tufCachePath, tufForceCache: offline === '1', label: 'SEA Sigstore fixture' });
      console.log(JSON.stringify({ ok: true }));
    } catch (error) {
      console.log(JSON.stringify({ ok: false, key: error.key ?? null, check: error.check ?? null }));
    }
    return 0;
  }
  const json = argv.includes('--json');
  const entry = COMMANDS[command];
  if (!entry) {
    if (json) return failJson('command.unknown', t('command.unknown', { command: command ?? '' }));
    console.error(`usage: sidevoice <${Object.keys(COMMANDS).join('|')}> [args]`); return 2;
  }
  try { return await entry(argv, process.env); }
  catch (error) { if (json) return failJson(error?.key || `${command}.failed`, error?.message || String(error)); throw error; }
}

function invokedAsCli() {
  if (!process.argv[1]) return false;
  try { return import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href; }
  catch { return false; }
}

if (invokedAsCli()) main().then(
  code => { if (typeof code === 'number') process.exitCode = code; },
  error => { console.error(error?.message || error); process.exitCode = 1; });
