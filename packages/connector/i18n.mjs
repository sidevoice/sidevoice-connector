/** What this package says to people, by key: one bundle per language, English the fallback (AGENTS.md).
 *
 *  Text that reaches an agent — tool results, MCP instructions — is English and is not looked up here;
 *  this is for a terminal, and for the `{key, message}` errors the app shows. The app translates by
 *  `key` with its own bundles; `message` is this package's English (or the person's language, once a
 *  bundle for it exists), never something to parse.
 *
 *  Bundles are modules, not files read beside this one: the single executable (R4) has nothing beside it. */
import { en } from './messages/en.mjs';

const BUNDLES = { en };

/** The person's language, from the locale the terminal runs in, when there is a bundle for it; else English. */
export function language(env = process.env) {
  const tag = String(env.LC_ALL || env.LC_MESSAGES || env.LANG || '').toLowerCase().split(/[._@-]/)[0];
  return BUNDLES[tag] ? tag : 'en';
}

/** The text for `key` with `{name}` placeholders filled; a key no bundle has is said as itself, so a
 *  missing string shows up as a key rather than as silence. */
export function t(key, params = {}, env = process.env) {
  const text = BUNDLES[language(env)][key] ?? en[key] ?? key;
  return text.replace(/\{(\w+)\}/g, (whole, name) => (name in params ? String(params[name]) : whole));
}

/** An error the app can act on: a stable `key`, the words for it, and anything else it should carry. */
export function keyed(key, params = {}, extra = {}) {
  return Object.assign(new Error(t(key, params)), { key, ...extra });
}
