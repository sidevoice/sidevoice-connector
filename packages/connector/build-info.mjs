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
export const CORE_MANIFEST_SHA256 = typeof __SIDEVOICE_CORE_MANIFEST_SHA256__ === 'string'
  ? __SIDEVOICE_CORE_MANIFEST_SHA256__
  : null;
export const RUST_CORE_MANIFEST_TEXT = typeof __SIDEVOICE_RUST_CORE_MANIFEST_JSON__ === 'string'
  ? __SIDEVOICE_RUST_CORE_MANIFEST_JSON__ : null;
export const RUST_CORE_MANIFEST_SHA256 = typeof __SIDEVOICE_RUST_CORE_MANIFEST_SHA256__ === 'string'
  ? __SIDEVOICE_RUST_CORE_MANIFEST_SHA256__ : null;
export const RUST_CORE_SOURCE_SHA = typeof __SIDEVOICE_RUST_CORE_SOURCE_SHA__ === 'string'
  ? __SIDEVOICE_RUST_CORE_SOURCE_SHA__ : null;
export const RUST_CORE_TARGET = typeof __SIDEVOICE_RUST_CORE_TARGET__ === 'string'
  ? __SIDEVOICE_RUST_CORE_TARGET__ : null;
export const RUST_CORE_ARCHIVE_SHA256 = typeof __SIDEVOICE_RUST_CORE_ARCHIVE_SHA256__ === 'string'
  ? __SIDEVOICE_RUST_CORE_ARCHIVE_SHA256__ : null;
export const RUST_CORE_ARCHIVE_SIZE = typeof __SIDEVOICE_RUST_CORE_ARCHIVE_SIZE__ === 'number'
  && Number.isSafeInteger(__SIDEVOICE_RUST_CORE_ARCHIVE_SIZE__) ? __SIDEVOICE_RUST_CORE_ARCHIVE_SIZE__ : null;
export const RUST_CONNECTOR_TARGET = typeof __SIDEVOICE_RUST_CONNECTOR_TARGET__ === 'string'
  ? __SIDEVOICE_RUST_CONNECTOR_TARGET__ : null;
export const RUST_CONNECTOR_SOURCE_SHA = typeof __SIDEVOICE_RUST_CONNECTOR_SOURCE_SHA__ === 'string'
  ? __SIDEVOICE_RUST_CONNECTOR_SOURCE_SHA__ : null;
export const RUST_CONNECTOR_BINARY_SHA256 = typeof __SIDEVOICE_RUST_CONNECTOR_BINARY_SHA256__ === 'string'
  ? __SIDEVOICE_RUST_CONNECTOR_BINARY_SHA256__ : null;
export const RUST_CONNECTOR_BINARY_SIZE = typeof __SIDEVOICE_RUST_CONNECTOR_BINARY_SIZE__ === 'number'
  && Number.isSafeInteger(__SIDEVOICE_RUST_CONNECTOR_BINARY_SIZE__) ? __SIDEVOICE_RUST_CONNECTOR_BINARY_SIZE__ : null;
