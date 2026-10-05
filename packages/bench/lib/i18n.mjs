/** What the bench says to people, by key: one JSON bundle per language in ../messages, English the fallback
 *  (AGENTS.md). The browser UI loads the same bundles. A key no bundle has is said as itself. */
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const MESSAGES_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../messages');
export const LANGUAGES = ['en', 'es'];
const bundles = new Map();

export function bundle(lang) {
  if (!LANGUAGES.includes(lang)) return {};
  if (!bundles.has(lang)) bundles.set(lang, JSON.parse(readFileSync(path.join(MESSAGES_DIR, `${lang}.json`), 'utf8')));
  return bundles.get(lang);
}

/** The person's language from the terminal's locale when there is a bundle for it; else English. */
export function language(env = process.env) {
  const tag = String(env.LC_ALL || env.LC_MESSAGES || env.LANG || '').toLowerCase().split(/[._@-]/)[0];
  return LANGUAGES.includes(tag) ? tag : 'en';
}

export function t(key, params = {}, lang = language()) {
  const text = bundle(lang)[key] ?? bundle('en')[key] ?? key;
  return text.replace(/\{(\w+)\}/g, (whole, name) => (name in params ? String(params[name]) : whole));
}
