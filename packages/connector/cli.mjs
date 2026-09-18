#!/usr/bin/env node
/** `sidevoice <mcp|pair|connector> …` — one bin, three entry points. */
const [, , command, ...rest] = process.argv;
const entries = { mcp: './mcp.mjs', pair: './pair.mjs', connector: './connector.mjs' };
if (!entries[command]) { console.error('usage: sidevoice <mcp|pair|connector> [args]'); process.exit(2); }
process.argv.splice(2, 1);
await import(entries[command]);
