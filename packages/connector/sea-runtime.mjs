import { createRequire } from 'node:module';

let sea = false;
try { sea = createRequire(import.meta.url)('node:sea').isSea(); } catch {}

export const runningAsSea = () => sea;
