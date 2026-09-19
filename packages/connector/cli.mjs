#!/usr/bin/env node
/** `sidevoice <mcp|pair|connector|hook> …` — one bin, four entry points. */
const [, , command, ...rest] = process.argv;
const entries = { mcp: './mcp.mjs', pair: './pair.mjs', connector: './connector.mjs', hook: './hook.mjs' };
if (!entries[command]) { console.error('usage: sidevoice <mcp|pair|connector|hook> [args]'); process.exit(2); }
process.argv.splice(2, 1);
if (command === 'hook') process.env.SIDEVOICE_HOOK_MAIN = '1';
await import(entries[command]);
