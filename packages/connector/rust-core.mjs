/** The URL-free native Core manifest and the one closed archive format emitted by Core T7. */
import { createHash } from 'node:crypto';
import { createWriteStream, lstatSync, mkdirSync, rmSync } from 'node:fs';
import { Readable, Transform } from 'node:stream';
import { finished, pipeline } from 'node:stream/promises';
import path from 'node:path';
import { createZstdDecompress } from 'node:zlib';
import tar from 'tar-stream';
import { refusal, sha256 } from './core-attestation.mjs';
import { keyed } from './i18n.mjs';

export const RUST_CORE_KIND = 'rust-native-v1';
export const RUST_CORE_ENTRYPOINT = 'bin/sidevoice-core-rust';
export const RUST_CORE_TARGETS = Object.freeze(['macos-aarch64', 'linux-x86_64', 'linux-aarch64']);
export const MAX_RUST_CORE_ARCHIVE_BYTES = 250_000_000;
const MAX_RUST_CORE_UNPACKED_BYTES = 1_000_000_000;
const MAX_RUST_CORE_ENTRIES = 2_000;
const MAX_RUST_CORE_FILE_BYTES = 500_000_000;
const MAX_RUST_CORE_PATH_BYTES = 240;
const MAX_RUST_CORE_INVENTORY_BYTES = 4_000_000;
const REQUIRED_FILES = [RUST_CORE_ENTRYPOINT, 'checks/detector-16k.wav', 'models/silero.onnx',
  'models/silero_vad_16k.bin', 'models/smart_turn_weights.bin.gz'];

const exactKeys = (value, keys) => value && typeof value === 'object' && !Array.isArray(value)
  && Object.keys(value).sort().join(',') === [...keys].sort().join(',');
const sourceSha = value => typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
const digest = value => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

/** Parse the producer's canonical JSON and reject duplicate keys, whitespace changes, BOMs and trailing bytes. */
export function parseCanonicalRustCoreJson(bytes, label = 'native Core JSON') {
  let value;
  try {
    const text = Buffer.from(bytes).toString('utf8');
    value = JSON.parse(text);
    if (!Buffer.from(`${canonical(value)}\n`, 'utf8').equals(Buffer.from(bytes))) throw new Error('not canonical');
  } catch {
    throw refusal('manifest', `${label} is not canonical UTF-8 JSON`);
  }
  return value;
}

/** Validate the distinct Rust-native schema; the Python `{bundles,wheel}` validator stays separate. */
export function validateRustCoreManifest(manifest, { expectedSourceSha = null } = {}) {
  if (!exactKeys(manifest, ['schema', 'kind', 'source_sha', 'cargo_lock_sha256', 'entrypoint', 'bundles'])
      || manifest.schema !== 1 || manifest.kind !== RUST_CORE_KIND || !sourceSha(manifest.source_sha)
      || !digest(manifest.cargo_lock_sha256) || manifest.entrypoint !== RUST_CORE_ENTRYPOINT
      || !exactKeys(manifest.bundles, RUST_CORE_TARGETS)) {
    throw refusal('manifest', 'native Core manifest does not match the rust-native-v1 producer schema');
  }
  if (expectedSourceSha !== null && (!sourceSha(expectedSourceSha) || manifest.source_sha !== expectedSourceSha)) {
    throw refusal('manifest', 'native Core source commit differs from the protected production pin');
  }
  for (const target of RUST_CORE_TARGETS) {
    const record = manifest.bundles[target];
    const expectedName = `sidevoice-core-rust-${manifest.source_sha}-${target}.tar.zst`;
    if (!exactKeys(record, ['name', 'size', 'sha256']) || record.name !== expectedName
        || !Number.isSafeInteger(record.size) || record.size < 1 || record.size > MAX_RUST_CORE_ARCHIVE_BYTES
        || !digest(record.sha256)) {
      throw refusal('manifest', `native Core ${target} bundle record is invalid`);
    }
  }
  return manifest;
}

export function rustCoreTarget(platform = process.platform, arch = process.arch) {
  if (platform === 'darwin' && arch === 'arm64') return 'macos-aarch64';
  if (platform === 'linux' && arch === 'x64') return 'linux-x86_64';
  if (platform === 'linux' && arch === 'arm64') return 'linux-aarch64';
  return null;
}

function safeInventoryName(name) {
  if (typeof name !== 'string' || !name || Buffer.byteLength(name, 'utf8') > MAX_RUST_CORE_PATH_BYTES
      || name.includes('\0') || name.includes('\\') || name.startsWith('/') || !/^[A-Za-z0-9._+@/-]+$/.test(name)) return false;
  const parts = name.split('/');
  return parts.every(part => part && part !== '.' && part !== '..')
    && ['bin', 'lib', 'models', 'checks', 'notices'].includes(parts[0]);
}

function validateInventory(inventory, { target, sourceSha: expectedSourceSha }) {
  if (!exactKeys(inventory, ['schema', 'kind', 'target', 'source_sha', 'entrypoint', 'files'])
      || inventory.schema !== 1 || inventory.kind !== RUST_CORE_KIND || inventory.target !== target
      || inventory.source_sha !== expectedSourceSha || inventory.entrypoint !== RUST_CORE_ENTRYPOINT
      || !Array.isArray(inventory.files) || inventory.files.length > MAX_RUST_CORE_ENTRIES - 1) {
    throw refusal('archive-inventory', 'native Core inventory does not match the selected target');
  }
  let previous = '';
  const byName = new Map();
  for (const record of inventory.files) {
    if (!exactKeys(record, ['name', 'size', 'sha256']) || !safeInventoryName(record.name)
        || record.name === 'native-core.json' || !Number.isSafeInteger(record.size) || record.size < 0
        || record.size > MAX_RUST_CORE_FILE_BYTES || !digest(record.sha256) || record.name <= previous) {
      throw refusal('archive-inventory', 'native Core inventory file record is invalid or unsorted');
    }
    previous = record.name;
    byName.set(record.name, record);
  }
  for (const name of REQUIRED_FILES) if (!byName.has(name)) {
    throw refusal('archive-inventory', `native Core inventory omits ${name}`);
  }
  if (![...byName.keys()].some(name => name.startsWith('notices/'))) {
    throw refusal('archive-inventory', 'native Core inventory has no notices');
  }
  return byName;
}

function expectedDirectories(files) {
  const directories = new Set(['sidevoice-core-rust']);
  for (const name of files.keys()) {
    const parts = `sidevoice-core-rust/${name}`.split('/');
    for (let index = 1; index < parts.length; index++) directories.add(parts.slice(0, index).join('/'));
  }
  return directories;
}

function archivePath(name, type) {
  if (typeof name !== 'string' || name.includes('\0') || name.includes('\\') || name.startsWith('/')
      || Buffer.byteLength(name, 'utf8') > MAX_RUST_CORE_PATH_BYTES + 'sidevoice-core-rust/'.length) {
    throw refusal('archive-path', 'native Core archive contains an unsafe path');
  }
  const normalized = type === 'directory' && name.endsWith('/') ? name.slice(0, -1) : name;
  const parts = normalized.split('/');
  if (!normalized || parts.some(part => !part || part === '.' || part === '..') || parts[0] !== 'sidevoice-core-rust') {
    throw refusal('archive-path', 'native Core archive must have one fixed root');
  }
  const relative = parts.slice(1).join('/');
  if (parts.length > 1 && !(type === 'file' && relative === 'native-core.json') && !safeInventoryName(relative)) {
    throw refusal('archive-path', `native Core archive path is outside the fixed inventory roots: ${normalized}`);
  }
  return normalized;
}

function ensureParents(root, name) {
  const parts = name.split('/').slice(0, -1);
  let current = root;
  for (const part of parts) {
    current = path.join(current, part);
    try {
      const info = lstatSync(current);
      if (!info.isDirectory() || info.isSymbolicLink()) throw refusal('archive-path', 'native Core archive parent is not a directory');
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
      mkdirSync(current, { mode: 0o700 });
    }
  }
}

/** Extract only the T7 archive format into a fresh sibling stage, verifying the complete inventory before return. */
export async function unpackRustCoreArchive(archiveBytes, destination, { manifest, target, signal } = {}) {
  validateRustCoreManifest(manifest);
  const entry = manifest.bundles?.[target];
  const bytes = Buffer.from(archiveBytes);
  if (!entry || bytes.length !== entry.size || bytes.length > MAX_RUST_CORE_ARCHIVE_BYTES || sha256(bytes) !== entry.sha256) {
    throw refusal('sha256', 'native Core archive differs from its signed target record');
  }
  const root = path.resolve(destination);
  mkdirSync(root, { recursive: false, mode: 0o700 });
  const seen = new Set(), filesSeen = new Map(), dirsSeen = new Set();
  let count = 0, unpackedBytes = 0, inventoryBytes = null, entryError = null;
  const extractor = tar.extract();
  extractor.on('entry', (header, stream, next) => {
    (async () => {
      if (signal?.aborted) throw keyed('install.cancelled');
      if (++count > MAX_RUST_CORE_ENTRIES) throw refusal('archive-size', 'native Core archive has too many entries');
      const name = archivePath(header.name, header.type);
      if (seen.has(name)) throw refusal('archive-path', `duplicate native Core archive path ${name}`);
      seen.add(name);
      if (header.type === 'directory') {
        const rel = name.slice('sidevoice-core-rust/'.length);
        if (rel && !safeInventoryName(rel)) throw refusal('archive-path', `invalid native Core directory ${name}`);
        try {
          const info = lstatSync(path.join(root, rel));
          if (!info.isDirectory() || info.isSymbolicLink()) throw refusal('archive-path', `native Core directory collision ${name}`);
        } catch (error) {
          if (error.code !== 'ENOENT') throw error;
          mkdirSync(path.join(root, rel), { recursive: true, mode: 0o700 });
        }
        dirsSeen.add(name);
        stream.resume();
        await finished(stream, signal ? { signal } : undefined);
        return;
      }
      if (header.type !== 'file' || !Number.isSafeInteger(header.size) || header.size < 0 || header.size > MAX_RUST_CORE_FILE_BYTES) {
        throw refusal('archive-type', `native Core archive contains a non-regular entry at ${name}`);
      }
      unpackedBytes += header.size;
      if (unpackedBytes > MAX_RUST_CORE_UNPACKED_BYTES) throw refusal('archive-size', 'native Core archive exceeds the unpacked size limit');
      const rel = name.slice('sidevoice-core-rust/'.length);
      if (rel === 'native-core.json') {
        if (header.size > MAX_RUST_CORE_INVENTORY_BYTES) throw refusal('archive-size', 'native Core inventory exceeds its size limit');
        const chunks = [];
        let length = 0;
        for await (const chunk of stream) {
          length += chunk.length;
          if (length > MAX_RUST_CORE_INVENTORY_BYTES) throw refusal('archive-size', 'native Core inventory exceeds its size limit');
          chunks.push(chunk);
        }
        if (length !== header.size) throw refusal('archive-size', 'native Core inventory is truncated');
        inventoryBytes = Buffer.concat(chunks);
        return;
      }
      if (!safeInventoryName(rel)) throw refusal('archive-path', `native Core file path is invalid: ${rel}`);
      ensureParents(root, rel);
      const file = path.join(root, rel);
      const hash = createHash('sha256');
      const hasher = new Transform({ transform(chunk, _encoding, callback) { hash.update(chunk); callback(null, chunk); } });
      const mode = rel === RUST_CORE_ENTRYPOINT ? 0o755 : 0o644;
      await pipeline(stream, hasher, createWriteStream(file, { flags: 'wx', mode }), ...(signal ? [{ signal }] : []));
      const info = lstatSync(file);
      if (!info.isFile() || info.isSymbolicLink() || info.size !== header.size) throw refusal('archive-size', `native Core file size differs at ${rel}`);
      filesSeen.set(rel, { size: info.size, sha256: hash.digest('hex') });
    })().then(next, error => { entryError = error; next(error); });
  });
  try {
    const source = Readable.from([bytes]);
    await pipeline(source, createZstdDecompress(), extractor, ...(signal ? [{ signal }] : []));
    if (signal?.aborted) throw keyed('install.cancelled');
    if (entryError) throw entryError;
    if (!seen.has('sidevoice-core-rust') || !dirsSeen.has('sidevoice-core-rust') || !inventoryBytes) {
      throw refusal('archive-inventory', 'native Core archive is missing its fixed root or inventory');
    }
    const inventory = parseCanonicalRustCoreJson(inventoryBytes, 'native-core.json');
    const records = validateInventory(inventory, { target, sourceSha: manifest.source_sha });
    const expectedDirs = expectedDirectories(records);
    if (dirsSeen.size !== expectedDirs.size || [...expectedDirs].some(name => !dirsSeen.has(name))) {
      throw refusal('archive-inventory', 'native Core archive directories do not match its inventory');
    }
    if (filesSeen.size !== records.size || [...records].some(([name, record]) => {
      const actual = filesSeen.get(name);
      return !actual || actual.size !== record.size || actual.sha256 !== record.sha256;
    })) throw refusal('archive-inventory', 'native Core files do not match the signed inventory');
    return { root, inventory, target, sourceSha: manifest.source_sha, archiveSha256: entry.sha256 };
  } catch (error) {
    rmSync(root, { recursive: true, force: true });
    if (signal?.aborted) throw keyed('install.cancelled');
    throw entryError ?? error;
  }
}
