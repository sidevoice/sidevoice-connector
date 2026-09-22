#!/usr/bin/env node
/** `sidevoice <install|uninstall|mcp|pair|connector|skill> …` — one bin, six entry points.
 *
 *  Each entry is named by a literal so that the bundler can see it: published, this file and
 *  everything it reaches are one file in `dist/`, and an import it could not resolve at build
 *  time would be a path that does not exist at run time. */
const [, , command] = process.argv;
const entries = new Set(['install', 'uninstall', 'mcp', 'pair', 'connector', 'skill']);
if (!entries.has(command)) { console.error('usage: sidevoice <install|uninstall|mcp|pair|connector|skill> [args]'); process.exit(2); }
process.argv.splice(2, 1);
if (command === 'skill') process.env.SIDEVOICE_SKILL_MAIN = '1';
if (command === 'pair') process.env.SIDEVOICE_PAIR_MAIN = '1';
if (command === 'install') process.env.SIDEVOICE_INSTALL_MAIN = '1';
if (command === 'uninstall') process.env.SIDEVOICE_UNINSTALL_MAIN = '1';
switch (command) {
  case 'install': case 'uninstall': await import('./install.mjs'); break;
  case 'mcp': await import('./mcp.mjs'); break;
  case 'pair': await import('./pair.mjs'); break;
  case 'connector': await import('./connector.mjs'); break;
  case 'skill': await import('./skill.mjs'); break;
}
