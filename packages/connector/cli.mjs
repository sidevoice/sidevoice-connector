#!/usr/bin/env node
/** `sidevoice <command> …` — one bin, one entry per command.
 *
 *  Every command module exports what runs it (`run(argv)`, or a named one where a module holds two) and does
 *  nothing on import: this file imports them all and calls the one asked for. Named statically so the bundler
 *  sees them: published, this file and everything it reaches are one file in `dist/`, and an import it could not
 *  resolve at build time would be a path that does not exist at run time. No top-level await — the single
 *  executable (R4) is a CommonJS build, where there is none. */
import { run as connector } from './connector.mjs';
import { runInstall, runUninstall } from './install.mjs';
import { run as mcp } from './mcp.mjs';
import { runLinkRoom, runPair } from './pair.mjs';
import { run as pairDevice } from './pair-device.mjs';
import { run as service } from './service.mjs';
import { run as skill } from './skill.mjs';

const COMMANDS = { install: runInstall, uninstall: runUninstall, mcp, pair: runPair, 'link-room': runLinkRoom,
  'pair-device': pairDevice, connector, service, skill };

async function main([command, ...argv]) {
  const entry = COMMANDS[command];
  if (!entry) { console.error(`usage: sidevoice <${Object.keys(COMMANDS).join('|')}> [args]`); return 2; }
  return entry(argv, process.env);
}

main(process.argv.slice(2)).then(
  code => { if (typeof code === 'number') process.exitCode = code; },
  error => { console.error(error?.message || error); process.exitCode = 1; });
