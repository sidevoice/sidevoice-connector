#!/usr/bin/env node
/** `sidevoice <mcp|pair|connector|hook|skill> …` — one bin, five entry points. */
const [, , command] = process.argv;
const entries = { mcp: './mcp.mjs', pair: './pair.mjs', connector: './connector.mjs', hook: './hook.mjs', skill: './skill.mjs' };
if (!entries[command]) { console.error('usage: sidevoice <mcp|pair|connector|hook|skill> [args]'); process.exit(2); }
process.argv.splice(2, 1);
if (command === 'hook') process.env.SIDEVOICE_HOOK_MAIN = '1';
if (command === 'skill') process.env.SIDEVOICE_SKILL_MAIN = '1';
await import(entries[command]);
