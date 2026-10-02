import { createWriteStream, openSync, closeSync, fsyncSync } from 'node:fs';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import path from 'node:path';
import { refusal, verifyCoreArtifact } from './core-attestation.mjs';
import { unpackCoreArchive } from './core-archive.mjs';

const URL_PREFIX = '/sidevoice/sidevoice-core/releases/download/';
const MAX_SIDECAR_BYTES = 4 * 1024 * 1024;
const MAX_WHEEL_BYTES = 512 * 1024 * 1024;

export function coreTarget(platform = process.platform, arch = process.arch) {
  const os = platform === 'darwin' ? 'macos' : platform === 'linux' ? 'linux' : null;
  const architecture = arch === 'arm64' ? 'aarch64' : arch === 'x64' ? 'x86_64' : null;
  if (!os || !architecture) return null;
  if (os === 'macos' && architecture !== 'aarch64') return null;
  return { os, arch: architecture };
}

function releaseUrl(raw, label) {
  let url;
  try { url = new URL(raw); } catch { throw refusal('manifest', `${label} URL is invalid`); }
  if (url.protocol !== 'https:' || url.hostname !== 'github.com' || !url.pathname.startsWith(URL_PREFIX) ||
      url.username || url.password || url.search || url.hash) {
    throw refusal('manifest', `${label} URL is outside the public core releases`);
  }
  return url;
}

function coreAssetUrl(raw, label, coreVersion, basename, channel) {
  const url = releaseUrl(raw, label);
  const releaseTag = channel === 'release' ? `v${coreVersion}` : channel === 'nightly' ? 'nightly' : null;
  if (!releaseTag) throw refusal('manifest', `unsupported core manifest channel ${channel}`);
  const expectedPath = `${URL_PREFIX}${releaseTag}/${basename}`;
  if (url.pathname !== expectedPath || path.posix.basename(url.pathname) !== basename) {
    throw refusal('manifest', `${label} filename or release channel does not match the pinned ${channel}`);
  }
  return url;
}

const hasExactKeys = (value, keys) => value && typeof value === 'object' &&
  Object.keys(value).sort().join(',') === [...keys].sort().join(',');

export function validateCoreManifest(manifest, coreVersion, channel = 'release') {
  if (!hasExactKeys(manifest, ['bundles', 'wheel']) || !Array.isArray(manifest.bundles) ||
      !hasExactKeys(manifest.wheel, ['url', 'sha256'])) {
    throw refusal('manifest', 'embedded core manifest does not match the producer schema');
  }
  const seen = new Set();
  const allowedTargets = new Set(['macos/aarch64', 'linux/x86_64', 'linux/aarch64']);
  for (const bundle of manifest.bundles) {
    if (!hasExactKeys(bundle, ['os', 'arch', 'url', 'sha256', 'size']) || typeof bundle.os !== 'string' ||
        typeof bundle.arch !== 'string' || !Number.isSafeInteger(bundle.size) || bundle.size < 1 ||
        !/^[0-9a-f]{64}$/.test(bundle.sha256)) throw refusal('manifest', 'a core bundle record is incomplete');
    const key = `${bundle.os}/${bundle.arch}`;
    if (!allowedTargets.has(key)) throw refusal('manifest', `unsupported core bundle target ${key}`);
    if (seen.has(key)) throw refusal('manifest', `duplicate core bundle target ${key}`);
    seen.add(key);
    const basename = `sidevoice-core-${coreVersion}-${bundle.os}-${bundle.arch}.tar.zst`;
    coreAssetUrl(bundle.url, `core bundle ${key}`, coreVersion, basename, channel);
  }
  if (!/^[0-9a-f]{64}$/.test(manifest.wheel.sha256)) throw refusal('manifest', 'the core wheel digest is not lowercase SHA-256');
  const wheelBasename = `sidevoice_core-${coreVersion}-py3-none-any.whl`;
  coreAssetUrl(manifest.wheel.url, 'core wheel', coreVersion, wheelBasename, channel);
  return manifest;
}

/** Pick a platform bundle only when the signed manifest actually contains it; otherwise use the signed wheel. */
export function coreInstallSource(manifest, target) {
  return target && manifest?.bundles?.some(item => item.os === target.os && item.arch === target.arch) ? 'bundle' : 'wheel';
}

async function responseFor(url, label) {
  let response;
  try { response = await fetch(url, { redirect: 'follow', signal: AbortSignal.timeout(20 * 60_000) }); }
  catch (error) { throw refusal('download', `${label} download failed (${error?.message ?? error})`); }
  if (!response.ok || !response.body) throw refusal(label === 'Sigstore sidecar' ? 'sigstore-bundle' : 'download', `${label} returned HTTP ${response.status}`);
  const finalUrl = new URL(response.url);
  if (finalUrl.protocol !== 'https:' || !['github.com', 'release-assets.githubusercontent.com'].includes(finalUrl.hostname)) {
    throw refusal('download', `${label} redirected outside GitHub Releases`);
  }
  return response;
}

async function downloadToFile(response, filename, { expectedSize = null, maxBytes }) {
  let total = 0;
  const counter = new Transform({ transform(chunk, encoding, callback) {
    total += chunk.length;
    if (total > maxBytes || (expectedSize !== null && total > expectedSize)) callback(refusal('download-size', 'download exceeded its embedded size limit'));
    else callback(null, chunk);
  } });
  await pipeline(Readable.fromWeb(response.body), counter, createWriteStream(filename, { flags: 'wx', mode: 0o600 }));
  if (expectedSize !== null && total !== expectedSize) throw refusal('download-size', `download size ${total} did not match ${expectedSize}`);
  const fd = openSync(filename, 'r');
  try { fsyncSync(fd); } finally { closeSync(fd); }
  return total;
}

async function downloadToBytes(response, maxBytes) {
  let total = 0;
  const chunks = [];
  const reader = response.body.getReader();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maxBytes) throw refusal('sigstore-bundle', 'Sigstore sidecar exceeds its size limit');
      chunks.push(Buffer.from(value));
    }
  } finally { reader.releaseLock(); }
  return Buffer.concat(chunks, total);
}

async function fetchAndVerify(entry, directory, { channel, tufCachePath, label, expectedSize = null, maxBytes }) {
  const url = releaseUrl(entry.url, label);
  const artifactPath = path.join(directory, path.posix.basename(url.pathname));
  const bundleUrl = new URL(url.href + '.sigstore.json');
  const [artifactResponse, bundleResponse] = await Promise.all([responseFor(url, label), responseFor(bundleUrl, 'Sigstore sidecar')]);
  const sidecarBytes = await downloadToBytes(bundleResponse, MAX_SIDECAR_BYTES);
  const size = await downloadToFile(artifactResponse, artifactPath, { expectedSize, maxBytes });
  const { readFileSync } = await import('node:fs');
  const bytes = readFileSync(artifactPath);
  await verifyCoreArtifact({ bytes, expectedSha256: entry.sha256, bundleBytes: sidecarBytes, channel, tufCachePath, label });
  return { path: artifactPath, size, sha256: entry.sha256 };
}

export async function prepareVerifiedCoreBundle({ manifest, coreVersion, target, directory, channel, tufCachePath }) {
  validateCoreManifest(manifest, coreVersion, channel);
  const entry = manifest.bundles.find(item => item.os === target.os && item.arch === target.arch);
  if (!entry) throw refusal('platform', `the signed manifest has no bundle for ${target.os}/${target.arch}`);
  const archive = await fetchAndVerify(entry, directory, { channel, tufCachePath, label: `core bundle ${target.os}/${target.arch}`,
    expectedSize: entry.size, maxBytes: entry.size });
  const payload = path.join(directory, 'payload');
  await unpackCoreArchive(archive.path, payload);
  return { ...archive, payload };
}

export async function fetchVerifiedCoreWheel({ manifest, coreVersion, directory, channel, tufCachePath }) {
  validateCoreManifest(manifest, coreVersion, channel);
  const entry = manifest.wheel;
  return fetchAndVerify(entry, directory, { channel, tufCachePath, label: 'core wheel', maxBytes: MAX_WHEEL_BYTES });
}
