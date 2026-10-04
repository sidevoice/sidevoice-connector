import { createRequire } from 'node:module';

let api = null;
try { api = createRequire(import.meta.url)('node:sea'); } catch {}

export const runningAsSea = () => !!api?.isSea();

/** Read the one build-time Core payload. The caller still verifies its pinned digest before staging. */
export function getSeaAsset(key) {
  if (!runningAsSea()) return null;
  return Buffer.from(api.getAsset(key));
}
