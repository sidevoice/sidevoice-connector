import { createWriteStream, mkdirSync, lstatSync, symlinkSync } from 'node:fs';
import { createReadStream } from 'node:fs';
import path from 'node:path';
import { pipeline, finished } from 'node:stream/promises';
import { createZstdDecompress } from 'node:zlib';
import tar from 'tar-stream';
import { refusal } from './core-attestation.mjs';
import { keyed } from './i18n.mjs';

const MAX_ARCHIVE_ENTRIES = 100_000;
const MAX_UNPACKED_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_LINK_HOPS = 4096;
const inside = (root, destination) => destination === root || destination.startsWith(root + path.sep);
const drain = async (stream, signal) => {
  if (signal?.aborted) throw keyed('install.cancelled');
  stream.resume();
  try { await finished(stream, signal ? { signal } : undefined); }
  catch (error) { if (signal?.aborted) throw keyed('install.cancelled'); throw error; }
};

function cleanArchivePath(name, root) {
  if (typeof name !== 'string' || !name || name.includes('\0') || name.includes('\\') || name.startsWith('/')) {
    throw refusal('archive-path', `unsafe archive path ${String(name)}`);
  }
  const normalized = name.replace(/\/$/, '');
  const parts = normalized.split('/');
  if (!normalized || parts.some(part => !part || part === '.' || part === '..')) throw refusal('archive-path', `unsafe archive path ${name}`);
  const full = path.resolve(root, ...parts);
  if (!inside(root, full) || full === root) throw refusal('archive-path', `archive path escapes the staging directory: ${name}`);
  return { normalized: parts.join('/'), full, parts };
}

function ensureRealParents(root, parts) {
  let current = root;
  for (const part of parts) {
    current = path.join(current, part);
    try {
      const info = lstatSync(current);
      if (!info.isDirectory() || info.isSymbolicLink()) throw refusal('archive-path', `archive parent is not a real directory: ${current}`);
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
      mkdirSync(current, { mode: 0o700 });
    }
  }
}

/** Resolve links through other archive links as the OS will, refusing escapes and cycles before creating any link. */
function validateLinkTargets(root, links) {
  const byPath = new Map(links.map(link => [link.normalized, link]));
  for (const link of links) {
    const parts = link.normalized.split('/').slice(0, -1);
    const pending = link.target.split('/');
    const visited = new Set([link.normalized]);
    let hops = 0;
    while (pending.length) {
      const part = pending.shift();
      if (!part || part === '.') continue;
      if (part === '..') {
        if (!parts.length) throw refusal('archive-link', `symlink escapes the staging directory: ${link.normalized}`);
        parts.pop();
        continue;
      }
      parts.push(part);
      const chained = byPath.get(parts.join('/'));
      if (!chained) continue;
      if (++hops > MAX_LINK_HOPS || visited.has(chained.normalized)) {
        throw refusal('archive-link', `symlink chain is cyclic or too deep: ${link.normalized}`);
      }
      visited.add(chained.normalized);
      parts.pop();
      pending.unshift(...chained.target.split('/'));
    }
    const resolved = path.resolve(root, ...parts);
    if (!inside(root, resolved)) throw refusal('archive-link', `symlink escapes the staging directory: ${link.normalized}`);
  }
}

/** Stream a zstd-compressed tar into a fresh private staging directory. Links are deferred until files finish. */
export async function unpackCoreArchive(archivePath, destination, { signal } = {}) {
  const root = path.resolve(destination);
  mkdirSync(root, { recursive: false, mode: 0o700 });
  const seen = new Set(), links = [];
  let entryCount = 0, unpackedBytes = 0;
  const extractor = tar.extract();
  let entryError = null;
  extractor.on('entry', (header, stream, next) => {
    (async () => {
      if (signal?.aborted) throw keyed('install.cancelled');
      if (++entryCount > MAX_ARCHIVE_ENTRIES) throw refusal('archive-size', 'archive has too many entries');
      const { normalized, full, parts } = cleanArchivePath(header.name, root);
      if (seen.has(normalized)) throw refusal('archive-path', `duplicate archive path ${normalized}`);
      seen.add(normalized);
      ensureRealParents(root, parts.slice(0, -1));
      if (header.type === 'directory') {
        try {
          const info = lstatSync(full);
          if (!info.isDirectory() || info.isSymbolicLink()) throw refusal('archive-path', `directory collides with another entry: ${normalized}`);
        } catch (error) {
          if (error.code !== 'ENOENT') throw error;
          mkdirSync(full, { mode: 0o700 });
        }
        await drain(stream, signal);
        return;
      }
      if (header.type === 'symlink') {
        const target = header.linkname;
        if (typeof target !== 'string' || !target || target.includes('\0') || target.includes('\\') || path.posix.isAbsolute(target)) {
          throw refusal('archive-link', `absolute or malformed symlink target for ${normalized}`);
        }
        const resolvedTarget = path.resolve(path.dirname(full), ...target.split('/'));
        if (!inside(root, resolvedTarget)) throw refusal('archive-link', `symlink escapes the staging directory: ${normalized}`);
        await drain(stream, signal);
        links.push({ full, normalized, target });
        return;
      }
      if (header.type !== 'file' || !Number.isSafeInteger(header.size) || header.size < 0) {
        throw refusal('archive-type', `unsupported tar entry type ${String(header.type)} at ${normalized}`);
      }
      unpackedBytes += header.size;
      if (unpackedBytes > MAX_UNPACKED_BYTES) throw refusal('archive-size', 'unpacked core exceeds the size limit');
      const mode = (Number(header.mode) & 0o111) ? 0o755 : 0o644;
      await pipeline(stream, createWriteStream(full, { flags: 'wx', mode }), ...(signal ? [{ signal }] : []));
      if (lstatSync(full).size !== header.size) throw refusal('archive-size', `truncated tar entry ${normalized}`);
    })().then(next, error => { entryError = error; next(error); });
  });
  await pipeline(createReadStream(archivePath), createZstdDecompress(), extractor, ...(signal ? [{ signal }] : [])).catch(error => {
    if (signal?.aborted) throw keyed('install.cancelled');
    throw entryError ?? error;
  });
  if (signal?.aborted) throw keyed('install.cancelled');
  if (entryError) throw entryError;
  validateLinkTargets(root, links);
  for (const link of links) {
    const parts = link.normalized.split('/');
    ensureRealParents(root, parts.slice(0, -1));
    try { lstatSync(link.full); throw refusal('archive-link', `symlink collides with an entry: ${link.normalized}`); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    symlinkSync(link.target, link.full);
  }
}
