/** Values that must travel with a single executable rather than be read beside it. */
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const sourcePackage = () => JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));

export const BUILD_PACKAGE = typeof __SIDEVOICE_PACKAGE_JSON__ === 'string'
  ? JSON.parse(__SIDEVOICE_PACKAGE_JSON__)
  : sourcePackage();
export const BUILD_PACKAGE_DIR = path.dirname(fileURLToPath(import.meta.url));
export const CORE_MANIFEST = typeof __SIDEVOICE_CORE_MANIFEST_JSON__ === 'string'
  ? JSON.parse(__SIDEVOICE_CORE_MANIFEST_JSON__)
  : null;
