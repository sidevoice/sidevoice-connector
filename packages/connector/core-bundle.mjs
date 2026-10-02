import { createWriteStream, openSync, closeSync, fsyncSync, writeFileSync, rmSync } from 'node:fs';
import { execFile as execFileCallback } from 'node:child_process';
import { promisify } from 'node:util';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import path from 'node:path';
import { refusal, verifyCoreArtifact } from './core-attestation.mjs';
import { unpackCoreArchive } from './core-archive.mjs';
import { keyed } from './i18n.mjs';
import { runningAsSea } from './sea-runtime.mjs';

const URL_PREFIX = '/sidevoice/sidevoice-core/releases/download/';
const MAX_SIDECAR_BYTES = 4 * 1024 * 1024;
const MAX_WHEEL_BYTES = 512 * 1024 * 1024;
const execFile = promisify(execFileCallback);

function throwIfAborted(signal) {
  if (signal?.aborted) throw keyed('install.cancelled');
}

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

async function responseFor(url, label, signal) {
  let response;
  const timeout = AbortSignal.timeout(20 * 60_000);
  const requestSignal = signal ? AbortSignal.any([signal, timeout]) : timeout;
  try { response = await fetch(url, { redirect: 'follow', signal: requestSignal }); }
  catch (error) { if (signal?.aborted) throw keyed('install.cancelled'); throw refusal('download', `${label} download failed (${error?.message ?? error})`); }
  if (!response.ok || !response.body) throw refusal(label === 'Sigstore sidecar' ? 'sigstore-bundle' : 'download', `${label} returned HTTP ${response.status}`);
  const finalUrl = new URL(response.url);
  if (finalUrl.protocol !== 'https:' || !['github.com', 'release-assets.githubusercontent.com'].includes(finalUrl.hostname)) {
    throw refusal('download', `${label} redirected outside GitHub Releases`);
  }
  return response;
}

async function downloadToFile(response, filename, { expectedSize = null, maxBytes, signal, onProgress = () => {} }) {
  let total = 0;
  const contentLength = Number(response.headers?.get('content-length'));
  const reportedTotal = expectedSize ?? (Number.isSafeInteger(contentLength) && contentLength >= 0 ? contentLength : null);
  let lastReport = 0;
  const counter = new Transform({ transform(chunk, encoding, callback) {
    if (signal?.aborted) { callback(keyed('install.cancelled')); return; }
    total += chunk.length;
    if (total > maxBytes || (expectedSize !== null && total > expectedSize)) callback(refusal('download-size', 'download exceeded its embedded size limit'));
    else {
      const now = Date.now();
      if (now - lastReport >= 200) { lastReport = now; onProgress(total, reportedTotal); }
      callback(null, chunk);
    }
  } });
  try { await pipeline(Readable.fromWeb(response.body), counter, createWriteStream(filename, { flags: 'wx', mode: 0o600 }), { signal }); }
  catch (error) { if (signal?.aborted) throw keyed('install.cancelled'); throw error; }
  if (expectedSize !== null && total !== expectedSize) throw refusal('download-size', `download size ${total} did not match ${expectedSize}`);
  onProgress(total, reportedTotal);
  const fd = openSync(filename, 'r');
  try { fsyncSync(fd); } finally { closeSync(fd); }
  return total;
}

async function downloadToBytes(response, maxBytes, signal) {
  let total = 0;
  const chunks = [];
  const reader = response.body.getReader();
  try {
    for (;;) {
      throwIfAborted(signal);
      const { value, done } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maxBytes) throw refusal('sigstore-bundle', 'Sigstore sidecar exceeds its size limit');
      chunks.push(Buffer.from(value));
    }
  } catch (error) {
    if (signal?.aborted) throw keyed('install.cancelled');
    throw error;
  } finally { reader.releaseLock(); }
  return Buffer.concat(chunks, total);
}

async function verifyDownloaded({ artifactPath, bundleBytes, expectedSha256, channel, tufCachePath, label, signal, verifierEntry }) {
  throwIfAborted(signal);
  if (!signal) {
    const { readFileSync } = await import('node:fs');
    return verifyCoreArtifact({ bytes: readFileSync(artifactPath), expectedSha256, bundleBytes, channel, tufCachePath, label });
  }

  // Sigstore's verifier/TUF network operations do not expose AbortSignal. Isolate them in a same-executable helper so
  // cancellation can terminate a verifier still waiting on TUF before the install reaches its commit point.
  const sidecarPath = `${artifactPath}.sigstore.json`;
  writeFileSync(sidecarPath, bundleBytes, { flag: 'wx', mode: 0o600 });
  try {
    const cliEntry = verifierEntry || (path.basename(process.argv[1] || '') === 'cli.mjs' ? process.argv[1] : null);
    if (!runningAsSea() && !cliEntry) throw refusal('sigstore-bundle', 'abortable verifier entry is unavailable');
    const args = runningAsSea()
      ? ['--sidevoice-verify-core', artifactPath, sidecarPath, channel, tufCachePath, '0', expectedSha256]
      : [cliEntry, '--sidevoice-verify-core', artifactPath, sidecarPath, channel, tufCachePath, '0', expectedSha256];
    const env = { ...process.env };
    for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
    let result;
    try { result = await execFile(process.execPath, args, { env, timeout: 2 * 60_000, maxBuffer: 64 * 1024, signal }); }
    catch (error) {
      if (signal.aborted || error.name === 'AbortError') throw keyed('install.cancelled');
      throw refusal('sigstore-bundle', `Sigstore verifier helper failed (${error?.code ?? error?.message ?? error})`);
    }
    let report;
    try { report = JSON.parse(result.stdout); } catch { throw refusal('sigstore-bundle', 'Sigstore verifier helper returned invalid output'); }
    if (!report.ok) throw refusal(report.check || report.key || 'sigstore-bundle', 'core artifact failed Sigstore verification');
  } finally { rmSync(sidecarPath, { force: true }); }
}

async function fetchAndVerify(entry, directory, { channel, tufCachePath, label, expectedSize = null, maxBytes, signal,
  progressEvent = () => {}, verifierEntry }) {
  throwIfAborted(signal);
  const url = releaseUrl(entry.url, label);
  const artifactPath = path.join(directory, path.posix.basename(url.pathname));
  const bundleUrl = new URL(url.href + '.sigstore.json');
  const [artifactResponse, bundleResponse] = await Promise.all([responseFor(url, label, signal), responseFor(bundleUrl, 'Sigstore sidecar', signal)]);
  const sidecarBytes = await downloadToBytes(bundleResponse, MAX_SIDECAR_BYTES, signal);
  progressEvent({ step: 'download', done: 0, total: expectedSize ?? null });
  const size = await downloadToFile(artifactResponse, artifactPath, { expectedSize, maxBytes, signal,
    onProgress: (done, total) => progressEvent({ step: 'download', done, total }) });
  throwIfAborted(signal);
  progressEvent({ step: 'verify', done: null, total: null });
  await verifyDownloaded({ artifactPath, bundleBytes: sidecarBytes, expectedSha256: entry.sha256, channel, tufCachePath, label, signal, verifierEntry });
  throwIfAborted(signal);
  return { path: artifactPath, size, sha256: entry.sha256 };
}

export async function prepareVerifiedCoreBundle({ manifest, coreVersion, target, directory, channel, tufCachePath, signal,
  progressEvent = () => {}, verifierEntry }) {
  validateCoreManifest(manifest, coreVersion, channel);
  const entry = manifest.bundles.find(item => item.os === target.os && item.arch === target.arch);
  if (!entry) throw refusal('platform', `the signed manifest has no bundle for ${target.os}/${target.arch}`);
  const archive = await fetchAndVerify(entry, directory, { channel, tufCachePath, label: `core bundle ${target.os}/${target.arch}`,
    expectedSize: entry.size, maxBytes: entry.size, signal, progressEvent, verifierEntry });
  const payload = path.join(directory, 'payload');
  await unpackCoreArchive(archive.path, payload, { signal });
  return { ...archive, payload };
}

export async function fetchVerifiedCoreWheel({ manifest, coreVersion, directory, channel, tufCachePath, signal,
  progressEvent = () => {}, verifierEntry }) {
  validateCoreManifest(manifest, coreVersion, channel);
  const entry = manifest.wheel;
  return fetchAndVerify(entry, directory, { channel, tufCachePath, label: 'core wheel', maxBytes: MAX_WHEEL_BYTES, signal, progressEvent, verifierEntry });
}
