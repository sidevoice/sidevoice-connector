import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, readFile, readdir, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createReadStream, createWriteStream } from 'node:fs';
import { createZstdCompress } from 'node:zlib';
import { pipeline } from 'node:stream/promises';
import tar from 'tar-stream';
import test, { after, before } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build as buildWithEsbuild } from 'esbuild';
import { enforceCoreProvenance, CORE_ISSUER, CORE_REPOSITORY, CORE_REPOSITORY_ID, CORE_SIGNER, SLSA_PREDICATE, sha256, verifyCoreArtifact } from '../core-attestation.mjs';
import { unpackCoreArchive } from '../core-archive.mjs';
import { coreInstallSource, coreTarget, fetchVerifiedCoreWheel, validateCoreManifest } from '../core-bundle.mjs';
import { classifyInstallFailure, normalizeInstallFailure } from '../install-errors.mjs';
import { readLock } from '../lockfile.mjs';
import { connectorSocketOf, dataDirOf, nodeFiles, writePrivate } from '../node-files.mjs';
import { VERSION } from '../identity.mjs';
import { CORE_VERSION, explainUvFailure, hasEmbeddedCoreBundle, installCoreRuntime, installRuntime, runtimeIdentity, runtimePaths, runtimeRoot, verifiedWheelCachePath } from '../core.mjs';
import { decide, discardRuntimeIfUnselected, point, releaseLayout, stableCommand } from '../release.mjs';
import { definitionTexts, recordInstallation } from '../service.mjs';
import { oursInCursor } from '../registrations.mjs';
import { connectorMetadata, versionMetadata } from '../metadata.mjs';
import { createDesktopPinRecord, CORE_INPUTS_ARTIFACT_NAME, PIN_ARTIFACT_NAME, PROVENANCE_ARTIFACT_NAME,
  SEA_ARTIFACT_NAME, verifyArtifactEntries, verifyArtifactRoundTrip } from '../sea-artifact.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixture = path.join(here, 'fixtures', 'r4', 'sigstore-5.0.0.sigstore.json');
const fixtureArtifact = path.join(here, 'fixtures', 'r4', 'sigstore-5.0.0.tgz');
const helper = path.join(here, 'r4-sigstore-helper.mjs');
const connectorPackage = path.dirname(here);
const coreProducerManifestFixture = path.join(here, 'fixtures', 'r4', 'core-manifest-core34.json');
const targetName = process.platform === 'darwin' && process.arch === 'arm64' ? 'macos-aarch64'
  : process.platform === 'linux' && process.arch === 'x64' ? 'linux-x86_64'
    : process.platform === 'linux' && process.arch === 'arm64' ? 'linux-aarch64' : null;
const sea = targetName ? path.join(connectorPackage, 'dist-sea', targetName, 'sidevoice') : null;
let scratch;

before(async () => { scratch = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-r4-')); });
after(async () => { if (scratch) await rm(scratch, { recursive: true, force: true }); });

const OIDS = {
  issuer: '1.3.6.1.4.1.57264.1.1', buildSigner: '1.3.6.1.4.1.57264.1.9',
  runner: '1.3.6.1.4.1.57264.1.11', source: '1.3.6.1.4.1.57264.1.12',
  repositoryId: '1.3.6.1.4.1.57264.1.15', buildConfig: '1.3.6.1.4.1.57264.1.18',
};
const buildConfig = `${CORE_REPOSITORY}/.github/workflows/release-please.yml@refs/heads/main`;
const textOid = value => {
  const bytes = Buffer.from(value, 'utf8');
  return Buffer.concat([Buffer.from([0x0c, bytes.length]), bytes]);
};
function signer(overrides = {}) {
  const values = { issuer: CORE_ISSUER, buildSigner: CORE_SIGNER, runner: 'github-hosted', source: CORE_REPOSITORY,
    repositoryId: CORE_REPOSITORY_ID, buildConfig, ...overrides };
  return { identity: { extensions: { issuer: values.issuer }, subjectAlternativeName: values.buildSigner,
    oids: Object.entries(values).map(([key, value]) => ({ oid: { id: OIDS[key].split('.').map(Number) },
      value: key === 'issuer' ? Buffer.from(value, 'utf8') : textOid(value) })) } };
}
function statement({ digest = 'a'.repeat(64), channel = 'release', predicateType = SLSA_PREDICATE,
  workflowPath = channel === 'release' ? '.github/workflows/release-please.yml' : '.github/workflows/test.yml',
  repositoryId = CORE_REPOSITORY_ID, subjects = [{ name: 'sidevoice-core-linux-x86_64', digest: { sha256: digest } }] } = {}) {
  return { _type: 'https://in-toto.io/Statement/v1', subject: subjects, predicateType,
    predicate: { buildDefinition: { externalParameters: { workflow: { repository: CORE_REPOSITORY, ref: 'refs/heads/main', path: workflowPath } },
      internalParameters: { github: { repository_id: repositoryId } } },
      runDetails: { builder: { id: 'https://github.com/actions/runner/github-hosted' } } } };
}
const expectRefusal = (run, check) => assert.throws(run, error => error.key === 'install.authenticity' && error.check === check);

async function buildCoreRuntimeAndCliWithManifest(manifest, name = 'manifest-fallback-build') {
  // Test-only bundle injection exercises runtime source selection. This intentionally bypasses build-time signature
  // verification with synthetic fixture data; it is not evidence of a production or genuine R4-a installation.
  const output = path.join(scratch, name);
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true, mode: 0o700 });
  const shipped = JSON.parse(await readFile(path.join(connectorPackage, 'package.json'), 'utf8'));
  shipped.sidevoice = { channel: 'release', build_seq: 1 };
  await buildWithEsbuild({
    entryPoints: {
      coreRuntime: path.join(connectorPackage, 'core.mjs'),
      cli: path.join(connectorPackage, 'cli.mjs'),
    },
    outdir: output,
    entryNames: '[name]',
    outExtension: { '.js': '.mjs' },
    bundle: true,
    platform: 'node',
    format: 'esm',
    target: 'node22',
    external: ['node:*'],
    banner: { js: 'import { createRequire as __sidevoiceCreateRequire } from "node:module"; const require = __sidevoiceCreateRequire(import.meta.url);' },
    legalComments: 'none',
    define: {
      __SIDEVOICE_PACKAGE_JSON__: JSON.stringify(JSON.stringify(shipped)),
      __SIDEVOICE_CORE_MANIFEST_JSON__: JSON.stringify(JSON.stringify(manifest)),
      __SIDEVOICE_CORE_MANIFEST_SHA256__: JSON.stringify(sha256(Buffer.from(JSON.stringify(manifest)))),
    },
  });
  return { coreRuntime: path.join(output, 'coreRuntime.mjs'), cli: path.join(output, 'cli.mjs') };
}

async function waitForPath(filename, timeout = 15_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (existsSync(filename)) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error(`timed out waiting for ${filename}`);
}

function testInstallEnv(home, extra = {}) {
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.startsWith('SIDEVOICE_')) delete env[key];
  for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
  return Object.assign(env, { HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, 'config'),
    SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: 'none' }, extra);
}

test('Fulcio and in-toto pins accept only the core repository, expected workflows, hosted runner and SHA-256 subject', () => {
  const digest = 'b'.repeat(64);
  const result = enforceCoreProvenance({ signer: signer(), statement: statement({ digest }), digest, channel: 'release' });
  assert.deepEqual(result, { issuer: CORE_ISSUER, signer: CORE_SIGNER, source: CORE_REPOSITORY,
    repositoryId: CORE_REPOSITORY_ID, runner: 'github-hosted', buildConfig, predicateType: SLSA_PREDICATE, sha256: digest });

  expectRefusal(() => enforceCoreProvenance({ signer: signer({ issuer: 'https://example.invalid' }), statement: statement({ digest }), digest, channel: 'release' }), 'issuer');
  expectRefusal(() => enforceCoreProvenance({ signer: signer({ buildSigner: `${CORE_REPOSITORY}/.github/workflows/other.yml@refs/heads/main` }), statement: statement({ digest }), digest, channel: 'release' }), 'workflow');
  expectRefusal(() => enforceCoreProvenance({ signer: signer({ source: 'https://github.com/sidevoice/sidevoice-core-fork' }), statement: statement({ digest }), digest, channel: 'release' }), 'source');
  expectRefusal(() => enforceCoreProvenance({ signer: signer({ repositoryId: '1' }), statement: statement({ digest }), digest, channel: 'release' }), 'repository-id');
  expectRefusal(() => enforceCoreProvenance({ signer: signer({ runner: 'self-hosted' }), statement: statement({ digest }), digest, channel: 'release' }), 'runner');
  expectRefusal(() => enforceCoreProvenance({ signer: signer({ buildConfig: `${CORE_REPOSITORY}/.github/workflows/test.yml@refs/heads/main` }), statement: statement({ digest }), digest, channel: 'release' }), 'build-config');
  expectRefusal(() => enforceCoreProvenance({ signer: signer(), statement: statement({ digest, workflowPath: '.github/workflows/attacker.yml' }), digest, channel: 'release' }), 'build-config');
  expectRefusal(() => enforceCoreProvenance({ signer: signer(), statement: statement({ digest, predicateType: 'https://example.invalid/predicate' }), digest, channel: 'release' }), 'predicate');
  expectRefusal(() => enforceCoreProvenance({ signer: signer(), statement: statement({ digest, subjects: [{ digest: { sha512: 'c'.repeat(128) } }] }), digest, channel: 'release' }), 'subject');
  expectRefusal(() => enforceCoreProvenance({ signer: signer(), statement: statement({ digest, subjects: [{ digest: { sha256: digest } }, { digest: { sha256: digest } }] }), digest, channel: 'release' }), 'subject');
});

test('nightly build config is pinned independently from release provenance', () => {
  const digest = 'c'.repeat(64);
  const expected = `${CORE_REPOSITORY}/.github/workflows/test.yml@refs/heads/main`;
  const passed = enforceCoreProvenance({ signer: signer({ buildConfig: expected }),
    statement: statement({ digest, channel: 'nightly' }), digest, channel: 'nightly' });
  assert.equal(passed.buildConfig, expected);
  expectRefusal(() => enforceCoreProvenance({ signer: signer(), statement: statement({ digest }), digest, channel: 'nightly' }), 'build-config');
});

test('tampered bytes and a missing Sigstore sidecar have named refusals', async () => {
  const bytes = Buffer.from('verified core candidate');
  await assert.rejects(() => verifyCoreArtifact({ bytes, expectedSha256: '0'.repeat(64), bundleBytes: '{}', channel: 'release', tufCachePath: path.join(scratch, 'unused') }),
    error => error.key === 'install.authenticity' && error.check === 'sha256');
  await assert.rejects(() => verifyCoreArtifact({ bytes, bundleBytes: '', channel: 'release', tufCachePath: path.join(scratch, 'unused') }),
    error => error.key === 'install.authenticity' && error.check === 'sigstore-bundle');
  assert.equal(sha256(bytes), '78905b65670587ef7d4d03527aef0a6586fba0333a31c1cf046e8cae6688b380');
});

test('a missing release .sigstore.json sidecar is refused before installation', async () => {
  const manifest = { bundles: [], wheel: {
    url: 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice_core-0.1.0-py3-none-any.whl',
    sha256: 'f'.repeat(64) } };
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async url => {
    if (String(url).endsWith('.sigstore.json')) return new Response(null, { status: 404 });
    const response = new Response(Buffer.from('wheel bytes'), { status: 200 });
    Object.defineProperty(response, 'url', { value: String(url) });
    return response;
  };
  try {
    await assert.rejects(() => fetchVerifiedCoreWheel({ manifest, coreVersion: '0.1.0', directory: scratch,
      channel: 'release', tufCachePath: path.join(scratch, 'missing-sidecar-tuf') }),
    error => error.key === 'install.authenticity' && error.check === 'sigstore-bundle');
  } finally { globalThis.fetch = originalFetch; }
});

test('R4-a manifest schema and platform mapping fail closed', () => {
  const manifest = { bundles: [
    { os: 'macos', arch: 'aarch64', url: 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice-core-0.1.0-macos-aarch64.tar.zst', sha256: 'd'.repeat(64), size: 123 },
    { os: 'linux', arch: 'x86_64', url: 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice-core-0.1.0-linux-x86_64.tar.zst', sha256: 'e'.repeat(64), size: 124 },
  ], wheel: { url: 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice_core-0.1.0-py3-none-any.whl', sha256: 'f'.repeat(64) } };
  assert.equal(validateCoreManifest(manifest, '0.1.0'), manifest);
  assert.equal(hasEmbeddedCoreBundle(manifest, { os: 'linux', arch: 'x86_64' }), true);
  assert.equal(hasEmbeddedCoreBundle(manifest, { os: 'windows', arch: 'x64' }), false);
  assert.deepEqual(coreTarget('darwin', 'arm64'), { os: 'macos', arch: 'aarch64' });
  assert.deepEqual(coreTarget('linux', 'x64'), { os: 'linux', arch: 'x86_64' });
  assert.equal(coreTarget('win32', 'x64'), null);
  assert.throws(() => validateCoreManifest({ ...manifest, bundles: [{ ...manifest.bundles[0], url: 'https://evil.invalid/core.tar.zst' }] }, '0.1.0'), /manifest/);
});

test('cross-repo core PR #34 manifest matches its exact two-key producer schema and remains bound to CORE_VERSION', async () => {
  const manifest = JSON.parse(await readFile(coreProducerManifestFixture, 'utf8'));
  assert.deepEqual(Object.keys(manifest).sort(), ['bundles', 'wheel']);
  assert.equal(validateCoreManifest(manifest, CORE_VERSION, 'release'), manifest);
  assert.equal(hasEmbeddedCoreBundle(manifest, { os: 'linux', arch: 'x86_64' }), true);
  assert.equal(coreInstallSource(manifest, { os: 'macos', arch: 'aarch64' }), 'bundle');
  assert.equal(coreInstallSource(manifest, { os: 'windows', arch: 'x86_64' }), 'wheel');
  assert.equal(coreInstallSource(manifest, null), 'wheel', 'unmapped platforms use the wheel path');
  assert.equal(coreInstallSource({ ...manifest, bundles: [] }, { os: 'linux', arch: 'x86_64' }), 'wheel',
    'a missing platform bundle uses the wheel path');
  assert.equal(hasEmbeddedCoreBundle({ ...manifest, bundles: [] }, { os: 'linux', arch: 'x86_64' }), false);

  expectRefusal(() => validateCoreManifest({ ...manifest, version: CORE_VERSION }, CORE_VERSION, 'release'), 'manifest');
  expectRefusal(() => validateCoreManifest(manifest, '0.1.1', 'release'), 'manifest');
  const wrongBundleVersion = { ...manifest, bundles: manifest.bundles.map(bundle => ({ ...bundle,
    url: bundle.url.replace('/v0.1.0/', '/v0.1.1/') })) };
  expectRefusal(() => validateCoreManifest(wrongBundleVersion, CORE_VERSION, 'release'), 'manifest');
  expectRefusal(() => validateCoreManifest({ ...manifest, wheel: { ...manifest.wheel,
    url: manifest.wheel.url.replace('/v0.1.0/', '/v0.1.1/') } }, CORE_VERSION, 'release'), 'manifest');
});

test('metadata reports desktop pin fields and hashes the producer manifest bytes without rewriting its schema', async () => {
  const raw = await readFile(coreProducerManifestFixture);
  const manifest = JSON.parse(raw);
  const target = coreTarget();
  const packageJson = JSON.parse(await readFile(path.join(connectorPackage, 'package.json'), 'utf8'));
  const buildPackage = { ...packageJson, sidevoice: { channel: 'release', build_seq: 29, connector_sha: 'c'.repeat(40) } };
  const metadata = connectorMetadata({ buildPackage, manifest, manifestSha256: sha256(raw), target, sea: true });
  const version = versionMetadata({ buildPackage, target, sea: true });
  assert.deepEqual(version, { ok: true, version: VERSION, target: `${target.os}-${target.arch}`, channel: 'release',
    connector_sha: 'c'.repeat(40), build_seq: 29, format: 'sea', sea: true });
  assert.deepEqual(Object.keys(metadata), ['ok', 'connector', 'embedded_core', 'protocols']);
  assert.deepEqual(metadata.connector, { version: VERSION, sha: 'c'.repeat(40), channel: 'release', build_seq: 29,
    target: `${target.os}-${target.arch}`, format: 'sea', sea: true, link_min: 2, link_max: 2 });
  assert.equal(metadata.embedded_core.version, CORE_VERSION);
  assert.equal(metadata.embedded_core.manifest_sha256, sha256(raw));
  assert.equal(metadata.embedded_core.api, 1);
  assert.equal(metadata.embedded_core.link, 2);
  const signedAssets = manifest.bundles.map(asset => ({ name: path.posix.basename(new URL(asset.url).pathname),
    url: asset.url, sha256: asset.sha256, size: asset.size }));
  assert.deepEqual(metadata.embedded_core.assets, signedAssets,
    'metadata carries every signed bundle so desktop pin validation can compare the complete manifest');
  const otherTarget = target.os === 'macos' ? { os: 'linux', arch: 'x86_64' } : { os: 'macos', arch: 'aarch64' };
  assert.deepEqual(connectorMetadata({ buildPackage, manifest, manifestSha256: sha256(raw), target: otherTarget, sea: true })
    .embedded_core.assets, signedAssets, 'the signed asset identity is independent of the connector target');
  assert.deepEqual(metadata.protocols, { metadata: 'sidevoice-metadata-v1', progress: 'sidevoice-progress-jsonl-v1' });

  const nightly = { bundles: manifest.bundles.map(asset => ({ ...asset, url: asset.url.replace('/v0.1.0/', '/nightly/') })),
    wheel: { ...manifest.wheel, url: manifest.wheel.url.replace('/v0.1.0/', '/nightly/') } };
  assert.equal(connectorMetadata({ buildPackage: { ...buildPackage, sidevoice: { ...buildPackage.sidevoice, channel: 'nightly' } },
    manifest: nightly, manifestSha256: 'd'.repeat(64), target, sea: true }).connector.channel, 'nightly');
});

test('verified asset downloads emit stable byte progress before a named digest refusal', async () => {
  const manifest = JSON.parse(await readFile(coreProducerManifestFixture, 'utf8'));
  const bytes = Buffer.from('tampered wheel fixture');
  const events = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (rawUrl, options = {}) => {
    const url = String(rawUrl);
    const body = url.endsWith('.sigstore.json') ? Buffer.from('{}') : bytes;
    const response = new Response(body, { status: 200, headers: { 'content-length': String(body.length) } });
    Object.defineProperty(response, 'url', { value: url });
    if (options.signal) options.signal.addEventListener('abort', () => response.body?.cancel().catch(() => {}), { once: true });
    return response;
  };
  try {
    await assert.rejects(() => fetchVerifiedCoreWheel({ manifest, coreVersion: CORE_VERSION, directory: scratch,
      channel: 'release', tufCachePath: path.join(scratch, 'progress-tuf'), progressEvent: event => events.push(event) }),
    error => error.key === 'install.authenticity' && error.check === 'sha256');
  } finally { globalThis.fetch = originalFetch; }
  assert.ok(events.some(event => event.step === 'download' && event.done === 0));
  assert.deepEqual(events.filter(event => event.step === 'download').at(-1), { step: 'download', done: bytes.length, total: bytes.length });
  assert.ok(events.some(event => event.step === 'verify' && event.done === null && event.total === null));
});

test('wheel byte progress keeps known bytes numeric when Content-Length is absent', async () => {
  const bytes = Buffer.from('wheel without a length header');
  const progressDirectory = path.join(scratch, 'progress-without-content-length');
  await mkdir(progressDirectory, { recursive: true });
  const manifest = { bundles: [], wheel: {
    url: 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice_core-0.1.0-py3-none-any.whl',
    sha256: sha256(bytes),
  } };
  const originalFetch = globalThis.fetch, events = [];
  globalThis.fetch = async rawUrl => {
    const url = String(rawUrl), body = url.endsWith('.sigstore.json') ? Buffer.from('{}') : bytes;
    const response = new Response(body, { status: 200 });
    Object.defineProperty(response, 'url', { value: url });
    return response;
  };
  try {
    await assert.rejects(() => fetchVerifiedCoreWheel({ manifest, coreVersion: CORE_VERSION, directory: progressDirectory,
      channel: 'release', tufCachePath: path.join(scratch, 'progress-unknown-total-tuf'), progressEvent: event => events.push(event) }),
    error => error.key === 'install.authenticity' && error.check === 'sigstore-bundle');
  } finally { globalThis.fetch = originalFetch; }
  assert.ok(events.some(event => event.step === 'download' && event.done === 0 && event.total === null));
  assert.ok(events.some(event => event.step === 'download' && event.done === bytes.length && event.total === null));
});

test('download and storage failures keep stable network, proxy, and disk keys separate from authenticity', async () => {
  const manifest = JSON.parse(await readFile(coreProducerManifestFixture, 'utf8'));
  const originalFetch = globalThis.fetch;
  const directory = path.join(scratch, 'download-failure-keys');
  await mkdir(directory, { recursive: true });
  const download = () => fetchVerifiedCoreWheel({ manifest, coreVersion: CORE_VERSION, directory,
    channel: 'release', tufCachePath: path.join(scratch, 'download-failure-tuf') });
  try {
    globalThis.fetch = async () => { const cause = Object.assign(new Error('no route'), { code: 'ENETUNREACH' });
      throw Object.assign(new TypeError('fetch failed'), { cause }); };
    await assert.rejects(download(), error => error.key === 'install.network');

    globalThis.fetch = async () => { const cause = Object.assign(new Error('certificate verify failed'), { code: 'UNABLE_TO_VERIFY_LEAF_SIGNATURE' });
      throw Object.assign(new TypeError('fetch failed'), { cause }); };
    await assert.rejects(download(), error => error.key === 'install.proxy');

    globalThis.fetch = async () => new Response('', { status: 407 });
    await assert.rejects(download(), error => error.key === 'install.proxy');

    globalThis.fetch = async rawUrl => {
      const url = String(rawUrl), response = new Response('', { status: url.endsWith('.sigstore.json') ? 404 : 200 });
      Object.defineProperty(response, 'url', { value: url });
      return response;
    };
    await assert.rejects(download(), error => error.key === 'install.authenticity' && error.check === 'sigstore-bundle');
  } finally { globalThis.fetch = originalFetch; }

  const noSpace = Object.assign(new Error('write failed'), { code: 'ENOSPC' });
  assert.equal(classifyInstallFailure(noSpace), 'disk');
  assert.equal(classifyInstallFailure(new Error('certificate verify failed for the signing identity'), null, { proxyText: false }), null,
    'a rejected Sigstore signing certificate remains an authenticity refusal, not a proxy error');
  assert.equal(normalizeInstallFailure(noSpace).key, 'install.disk');
  assert.equal(normalizeInstallFailure(Object.assign(new Error('digest mismatch'), { key: 'install.authenticity' })).key,
    'install.authenticity', 'a signed-artifact refusal is never remapped to a transport or disk key');
  assert.equal(explainUvFailure('pip install', 1, ['No space left on device'], '/tmp/sidevoice.log').key, 'install.disk');
  assert.equal(explainUvFailure('pip install', 1, ['UnknownIssuer while verifying certificate'], '/tmp/sidevoice.log').key, 'install.proxy');
  assert.equal(explainUvFailure('pip install', 1, ['network is unreachable'], '/tmp/sidevoice.log').key, 'install.network');
});

test('first-install uv cancellation removes its partial runtime before acknowledging cancellation', async () => {
  const home = path.join(scratch, 'uv-cancel-home'), dataDir = path.join(home, '.sidevoice');
  const uvDir = path.join(home, 'bin'), wheel = path.join(home, 'verified-wheel.whl');
  await mkdir(uvDir, { recursive: true });
  await writeFile(wheel, 'local wheel placeholder');
  const uv = path.join(uvDir, 'uv');
  await writeFile(uv, `#!${process.execPath}\nconst fs = require('node:fs');\nconst target = process.argv.at(-1);\nfs.mkdirSync(target, { recursive: true });\nfs.writeFileSync(require('node:path').join(target, 'partial'), 'incomplete');\nsetInterval(() => {}, 1000);\n`, { mode: 0o755 });
  const env = { HOME: home, PATH: uvDir, SIDEVOICE_CORE_SPEC: wheel };
  const { id } = runtimeIdentity(env);
  const paths = runtimePaths(dataDir, id), controller = new AbortController();
  const installing = installRuntime({ dataDir, env, signal: controller.signal });
  await waitForPath(path.join(paths.venv, 'partial'));
  controller.abort();
  await assert.rejects(installing, error => error.key === 'install.cancelled');
  assert.equal(existsSync(paths.home), false, 'the failed or cancelled first runtime tree is removed');
});

test('pre-commit cleanup removes only an unselected completed core runtime', async () => {
  const env = testInstallEnv(path.join(scratch, 'runtime-selection-home'));
  const dataDir = dataDirOf(env), id = '0.1.0-wheel-' + 'a'.repeat(64);
  const runtime = path.join(runtimeRoot(dataDir), id), layout = releaseLayout(env), selected = 'selected-release';
  await mkdir(runtime, { recursive: true, mode: 0o700 });
  await mkdir(path.join(layout.releases, selected), { recursive: true, mode: 0o700 });
  writePrivate(path.join(layout.releases, selected, 'release.json'), { id: selected, core_build: id });
  point(env, 'current', selected);
  assert.equal(discardRuntimeIfUnselected(env, dataDir, id), false, 'a selected runtime survives cancellation cleanup');
  await rm(layout.current, { force: true });
  assert.equal(discardRuntimeIfUnselected(env, dataDir, id), true, 'an unselected first-install runtime is discarded');
  assert.equal(existsSync(runtime), false);
});

test('SIGINT aborts the isolated Sigstore/TUF verifier before the install commit point', async () => {
  const hooks = path.join(scratch, 'cancel-verifier-hooks');
  const directory = path.join(scratch, 'cancel-verifier-download');
  await mkdir(hooks, { recursive: true, mode: 0o700 });
  await mkdir(directory, { recursive: true, mode: 0o700 });
  await writeFile(path.join(hooks, 'pause-verify-core'), '');
  const bytes = Buffer.from('test verifier cancellation payload');
  const artifactUrl = 'https://github.com/sidevoice/sidevoice-core/releases/download/v0.1.0/sidevoice_core-0.1.0-py3-none-any.whl';
  const manifest = { bundles: [], wheel: { url: artifactUrl, sha256: sha256(bytes) } };
  const originalFetch = globalThis.fetch, originalHooks = process.env.SIDEVOICE_TEST_HOOKS;
  const controller = new AbortController();
  globalThis.fetch = async rawUrl => {
    const url = String(rawUrl), body = url.endsWith('.sigstore.json') ? Buffer.from('{}') : bytes;
    const response = new Response(body, { status: 200, headers: { 'content-length': String(body.length) } });
    Object.defineProperty(response, 'url', { value: url });
    return response;
  };
  process.env.SIDEVOICE_TEST_HOOKS = hooks;
  try {
    const verifying = fetchVerifiedCoreWheel({ manifest, coreVersion: CORE_VERSION, directory, channel: 'release',
      tufCachePath: path.join(scratch, 'cancel-verifier-tuf'), signal: controller.signal, verifierEntry: path.join(connectorPackage, 'cli.mjs') });
    await waitForPath(path.join(hooks, 'paused-verify-core'));
    controller.abort();
    await assert.rejects(verifying, error => error.key === 'install.cancelled');
  } finally {
    globalThis.fetch = originalFetch;
    if (originalHooks === undefined) delete process.env.SIDEVOICE_TEST_HOOKS;
    else process.env.SIDEVOICE_TEST_HOOKS = originalHooks;
  }
});

test('missing platform bundle falls back to the verified wheel and reports keyed no-uv or tampered-wheel refusals', async () => {
  const producerManifest = JSON.parse(await readFile(coreProducerManifestFixture, 'utf8'));
  const manifest = { ...producerManifest, bundles: [] };
  const built = await buildCoreRuntimeAndCliWithManifest(manifest);
  const core = await import(pathToFileURL(built.coreRuntime).href);

  const metadataManifestBuild = await buildCoreRuntimeAndCliWithManifest(producerManifest, 'metadata-producer-build');
  const metadataResult = spawnSync(process.execPath, [metadataManifestBuild.cli, 'metadata', '--json'], { encoding: 'utf8', timeout: 15_000 });
  assert.equal(metadataResult.status, 0, metadataResult.stderr);
  assert.deepEqual(JSON.parse(metadataResult.stdout).embedded_core.assets, producerManifest.bundles.map(asset => ({
    name: path.posix.basename(new URL(asset.url).pathname), url: asset.url, sha256: asset.sha256, size: asset.size,
  })), 'the built CLI exposes the complete synthetic producer bundle list, independent of its host target');

  const cleanEnv = home => {
    const env = { ...process.env };
    for (const key of Object.keys(env)) if (key.startsWith('SIDEVOICE_')) delete env[key];
    for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
    Object.assign(env, { HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, 'config'),
      PATH: '', SIDEVOICE_INSTALL_FROM_SOURCE: '0', SIDEVOICE_SERVICE_MANAGER: 'none' });
    return env;
  };

  const missingSidecarHome = path.join(scratch, 'missing-sidecar-json-cli');
  const sidecarHook = path.join(scratch, 'sidecar-404-fetch.cjs');
  await mkdir(missingSidecarHome, { recursive: true, mode: 0o700 });
  await writeFile(sidecarHook, `globalThis.fetch = async raw => { const url = String(raw);\n` +
    `const response = url.endsWith('.sigstore.json') ? new Response('', { status: 404 }) : new Response(Buffer.from('asset'), { status: 200 });\n` +
    `Object.defineProperty(response, 'url', { value: url }); return response; };\n`);
  const sidecarEnv = { ...cleanEnv(missingSidecarHome), NODE_OPTIONS: `--require=${sidecarHook}` };
  const sidecarResult = spawnSync(process.execPath, [metadataManifestBuild.cli, 'install', '--no-agents', '--json'], {
    env: sidecarEnv, encoding: 'utf8', timeout: 60_000,
  });
  assert.equal(sidecarResult.status, 1, sidecarResult.stderr || sidecarResult.stdout);
  const sidecarFailure = JSON.parse(sidecarResult.stdout.trim());
  assert.equal(sidecarFailure.error.key, 'install.authenticity');
  assert.equal(sidecarFailure.error.params.check, 'sigstore-bundle');
  assert.equal(existsSync(releaseLayout(sidecarEnv).current), false, 'a missing signature sidecar does not select a release');

  const cliHome = path.join(scratch, 'missing-bundle-no-uv-cli');
  await mkdir(cliHome, { recursive: true, mode: 0o700 });
  const cliEnv = { ...cleanEnv(cliHome), SIDEVOICE_UV: path.join(scratch, 'no-such-uv') };
  const cliResult = spawnSync(process.execPath, [built.cli, 'install', '--no-agents', '--json'], {
    env: cliEnv, encoding: 'utf8', timeout: 60_000,
  });
  assert.equal(cliResult.status, 1, cliResult.stderr || cliResult.stdout);
  assert.equal(JSON.parse(cliResult.stdout.trim()).error.key, 'install.no-bundle', cliResult.stdout);

  const originalFetch = globalThis.fetch;
  const fetches = [];
  globalThis.fetch = async raw => {
    const url = String(raw);
    fetches.push(url);
    const response = new Response(Buffer.from(url.endsWith('.sigstore.json') ? '{}' : 'tampered wheel bytes'), { status: 200 });
    Object.defineProperty(response, 'url', { value: url });
    return response;
  };
  try {
    const uvHome = path.join(scratch, 'missing-bundle-uv-present');
    const dataDir = path.join(uvHome, '.sidevoice');
    await mkdir(uvHome, { recursive: true, mode: 0o700 });
    await assert.rejects(() => core.installCoreRuntime({ dataDir, env: { ...cleanEnv(uvHome), SIDEVOICE_UV: process.execPath } }),
      error => error.key === 'install.authenticity' && error.check === 'sha256');
    assert.equal(fetches.length, 2, 'with uv present, the fallback downloads both wheel and sidecar before rejecting tampered bytes');
    assert.ok(fetches.includes(producerManifest.wheel.url));
    assert.ok(fetches.includes(producerManifest.wheel.url + '.sigstore.json'));
  } finally { globalThis.fetch = originalFetch; }
});

test('R4-a manifests accept the exact nightly release path and refuse other tags or asset names', () => {
  const bundleName = 'sidevoice-core-0.1.0-linux-x86_64.tar.zst';
  const wheelName = 'sidevoice_core-0.1.0-py3-none-any.whl';
  const base = 'https://github.com/sidevoice/sidevoice-core/releases/download';
  const nightly = { bundles: [{ os: 'linux', arch: 'x86_64',
    url: `${base}/nightly/${bundleName}`, sha256: 'a'.repeat(64), size: 123 }],
  wheel: { url: `${base}/nightly/${wheelName}`, sha256: 'b'.repeat(64) } };
  assert.equal(validateCoreManifest(nightly, '0.1.0', 'nightly'), nightly);

  const release = { ...nightly, bundles: nightly.bundles.map(bundle => ({ ...bundle, url: bundle.url.replace('/nightly/', '/v0.1.0/') })),
    wheel: { ...nightly.wheel, url: nightly.wheel.url.replace('/nightly/', '/v0.1.0/') } };
  assert.equal(validateCoreManifest(release, '0.1.0', 'release'), release);
  expectRefusal(() => validateCoreManifest(nightly, '0.1.0', 'release'), 'manifest');
  expectRefusal(() => validateCoreManifest(release, '0.1.0', 'nightly'), 'manifest');
  expectRefusal(() => validateCoreManifest({ ...nightly,
    wheel: { ...nightly.wheel, url: `${base}/v0.1.1/${wheelName}` } }, '0.1.0', 'nightly'), 'manifest');
  expectRefusal(() => validateCoreManifest({ ...nightly,
    bundles: [{ ...nightly.bundles[0], url: `${base}/nightly/other-core.tar.zst` }] }, '0.1.0', 'nightly'), 'manifest');
  expectRefusal(() => validateCoreManifest({ ...nightly,
    wheel: { ...nightly.wheel, url: `${base}/nightly/${wheelName}?download=1` } }, '0.1.0', 'nightly'), 'manifest');
});

test('verified wheel cache location and runtime identity are stable for the same embedded digest', () => {
  const digest = 'a'.repeat(64), dataDir = path.join(scratch, 'wheel-runtime');
  const cachedWheel = verifiedWheelCachePath(dataDir, digest);
  assert.equal(cachedWheel, verifiedWheelCachePath(dataDir, digest));
  assert.ok(!cachedWheel.startsWith(runtimeRoot(dataDir) + path.sep), 'release pruning cannot delete the verified wheel cache');
  const first = runtimeIdentity({ SIDEVOICE_CORE_SPEC: path.join(dataDir, 'download-a.whl'), SIDEVOICE_INSTALL_FROM_SOURCE: '0' }, digest).id;
  const second = runtimeIdentity({ SIDEVOICE_CORE_SPEC: path.join(dataDir, 'download-b.whl'), SIDEVOICE_INSTALL_FROM_SOURCE: '0' }, digest).id;
  assert.equal(first, `${CORE_VERSION}-wheel-${digest}`);
  assert.equal(second, first, 'the complete verified SHA-256, independent of path/stat, names the runtime');
  assert.ok(cachedWheel.endsWith(path.join(digest, `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`)));
});

test('selected release format controls the recorded and service command across format changes and rollback', async () => {
  const env = { HOME: scratch, XDG_DATA_HOME: path.join(scratch, 'xdg'), SIDEVOICE_DATA_DIR: path.join(scratch, 'install-data') };
  const layout = releaseLayout(env);
  await mkdir(layout.releases, { recursive: true, mode: 0o700 });
  const makeRelease = async (id, format, files) => {
    const dir = path.join(layout.releases, id);
    await mkdir(path.join(dir, 'dist'), { recursive: true, mode: 0o700 });
    for (const file of files) await writeFile(path.join(dir, 'dist', file), 'fixture');
    await writeFile(path.join(dir, 'release.json'), JSON.stringify({ id, connector: VERSION, core: '0.1.0', core_build: 'external',
      channel: 'release', build_seq: 0, format }) + '\n');
  };
  await makeRelease(`${VERSION}-sea`, 'sea', ['sidevoice']);
  await makeRelease(VERSION, 'esm', ['cli.mjs']);
  await symlink(path.join('releases', `${VERSION}-sea`), layout.current);
  writePrivate(nodeFiles(dataDirOf(env)).install, { command: [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')],
    releases: layout.root, definitions: [] });
  assert.deepEqual(stableCommand(env), [path.join(layout.current, 'dist', 'sidevoice')]);
  recordInstallation(env);
  const seaInstall = JSON.parse(await readFile(nodeFiles(dataDirOf(env)).install, 'utf8'));
  assert.deepEqual(seaInstall.command, stableCommand(env));
  assert.equal(seaInstall.nodeExecutable, process.execPath, 'the R1 Node interpreter survives the SEA switch for rollback');
  assert.equal(oursInCursor({ command: path.join(layout.current, 'dist', 'sidevoice'), args: ['mcp'] }, env), true);
  assert.match(definitionTexts('systemd', env).connector, new RegExp(path.join(layout.current, 'dist', 'sidevoice').replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));

  assert.equal(decide({ connector: VERSION, channel: 'release', format: 'sea' },
    { connector: VERSION, channel: 'release', format: 'esm' }), 'noop');
  assert.equal(decide({ connector: VERSION, channel: 'release', format: 'esm' },
    { connector: VERSION, channel: 'release', format: 'sea' }), 'upgrade');
  assert.equal(decide({ connector: '9.0.0', channel: 'release', format: 'sea' },
    { connector: '1.0.0', channel: 'release', format: 'esm' }), 'noop');

  await rm(layout.current);
  await symlink(path.join('releases', VERSION), layout.current);
  recordInstallation(env);
  assert.deepEqual(stableCommand(env), [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')]);
  assert.deepEqual(JSON.parse(await readFile(nodeFiles(dataDirOf(env)).install, 'utf8')).command, stableCommand(env));
  assert.equal(oursInCursor({ command: path.join(layout.current, 'dist', 'sidevoice'), args: ['mcp'] }, env), true,
    'rollback retains ownership of an old SEA registration after install.json records ESM');
  assert.match(definitionTexts('systemd', env).connector, new RegExp(path.join(layout.current, 'dist', 'cli.mjs').replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));

  if (sea && existsSync(sea)) {
    writePrivate(nodeFiles(dataDirOf(env)).install, { command: [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')], releases: layout.root, definitions: [] });
    const legacy = spawnSync(sea, ['--sidevoice-selected-command'], { env: { ...process.env, ...env }, encoding: 'utf8', timeout: 10_000 });
    assert.equal(legacy.status, 0, legacy.stderr || legacy.stdout);
    assert.deepEqual(JSON.parse(legacy.stdout.trim()), [process.execPath, path.join(layout.current, 'dist', 'cli.mjs')],
      'a SEA rolling back to a legacy R1 release recovers Node from the old two-element command record');
    recordInstallation(env);
  }

  await rm(layout.current);
  await symlink(path.join('releases', `${VERSION}-sea`), layout.current);
  recordInstallation(env);
  assert.deepEqual(JSON.parse(await readFile(nodeFiles(dataDirOf(env)).install, 'utf8')).command, [path.join(layout.current, 'dist', 'sidevoice')]);
});

test('a build without R4-a assets and a network developer override both fail closed', async () => {
  await assert.rejects(() => installCoreRuntime({ dataDir: path.join(scratch, 'no-manifest'),
    env: { SIDEVOICE_INSTALL_FROM_SOURCE: '0' } }),
  error => error.key === 'install.authenticity' && error.check === 'manifest');
  await assert.rejects(() => installRuntime({ dataDir: path.join(scratch, 'network-override'),
    env: { SIDEVOICE_CORE_SPEC: 'sidevoice-core==0.1.0', SIDEVOICE_INSTALL_FROM_SOURCE: '0' } }),
  error => error.key === 'install.authenticity' && error.check === 'developer-override');
});

test('release and nightly package builds refuse to omit the signed core manifest', () => {
  for (const channel of ['release', 'nightly']) {
    const env = { ...process.env, SIDEVOICE_CHANNEL: channel, SIDEVOICE_BUILD_SEQ: '7' };
    for (const key of ['SIDEVOICE_CORE_MANIFEST', 'SIDEVOICE_CORE_MANIFEST_SIGSTORE', 'SIDEVOICE_REQUIRE_CORE_MANIFEST', 'SIDEVOICE_BUILD_SEA']) delete env[key];
    env.SIDEVOICE_REQUIRE_CORE_MANIFEST = '1';
    const result = spawnSync(process.execPath, [path.join(connectorPackage, 'build.mjs')], {
      cwd: connectorPackage, env, encoding: 'utf8', timeout: 30_000,
    });
    assert.equal(result.status, 1, result.stdout || result.stderr);
    assert.match(result.stderr, /SIDEVOICE_CORE_MANIFEST is required for release and nightly builds/);
  }
});

test('production npm and SEA workflows require the signed core manifest; only the tested production macOS SEA is uploaded', async () => {
  const repoRoot = path.resolve(connectorPackage, '..', '..');
  const ci = await readFile(path.join(repoRoot, '.github', 'workflows', 'ci.yml'), 'utf8');
  const seaWorkflow = await readFile(path.join(repoRoot, '.github', 'workflows', 'r4-sea.yml'), 'utf8');
  assert.match(ci, /MANIFEST=core-manifest\.json/);
  assert.match(ci, /MANIFEST_SIGSTORE=core-manifest\.json\.sigstore\.json/);
  assert.match(ci, /gh release download[\s\S]*?-p "\$MANIFEST" -p "\$MANIFEST_SIGSTORE"/);
  assert.match(ci, /SIDEVOICE_CORE_MANIFEST:\s*\$\{\{\s*steps\.core\.outputs\.manifest\s*\}\}/);
  assert.match(ci, /SIDEVOICE_CORE_MANIFEST_SIGSTORE:\s*\$\{\{\s*steps\.core\.outputs\.manifest_sigstore\s*\}\}/);
  assert.match(ci, /SIDEVOICE_REQUIRE_CORE_MANIFEST:\s*'1'/);
  assert.match(seaWorkflow, /^  workflow_dispatch:/m);
  assert.match(seaWorkflow, /if: github\.event_name != 'pull_request'/);
  assert.match(seaWorkflow, /gh release download nightly[\s\S]*?-p core-manifest\.json -p core-manifest\.json\.sigstore\.json/);
  assert.match(seaWorkflow, /SIDEVOICE_CORE_MANIFEST:\s*\$\{\{\s*steps\.core\.outputs\.manifest\s*\}\}/);
  assert.match(seaWorkflow, /SIDEVOICE_CORE_MANIFEST_SIGSTORE:\s*\$\{\{\s*steps\.core\.outputs\.manifest_sigstore\s*\}\}/);
  assert.match(seaWorkflow, /SIDEVOICE_REQUIRE_CORE_MANIFEST:\s*\$\{\{\s*steps\.core\.outputs\.require_manifest\s*\}\}/);
  assert.match(seaWorkflow, /echo "require_manifest=1"/);
  assert.match(seaWorkflow, new RegExp(`name: ${SEA_ARTIFACT_NAME}`));
  assert.match(seaWorkflow, /path: packages\/connector\/dist-sea\/macos-aarch64\/sidevoice/);
  assert.match(seaWorkflow, /retention-days: 90/);
  assert.match(seaWorkflow, /actions\/download-artifact@v4/);
  assert.match(seaWorkflow, /sea-artifact\.mjs verify-copy/);
  assert.match(seaWorkflow, new RegExp(`name: ${PIN_ARTIFACT_NAME}`));
  assert.match(seaWorkflow, new RegExp(`name: ${CORE_INPUTS_ARTIFACT_NAME}`));
  assert.match(seaWorkflow, new RegExp(`name: ${PROVENANCE_ARTIFACT_NAME}`));
  assert.match(seaWorkflow, /uses: actions\/attest@v4/);
  assert.match(seaWorkflow, /gh attestation verify[\s\S]*--bundle[\s\S]*--signer-workflow github\.com\/sidevoice\/sidevoice-connector\/\.github\/workflows\/r4-sea\.yml[\s\S]*--source-ref refs\/heads\/main[\s\S]*--source-digest[\s\S]*--deny-self-hosted-runners/);
  assert.match(seaWorkflow, /artifact-metadata: write/);
  assert.match(seaWorkflow, /repos\/\$GITHUB_REPOSITORY\/actions\/runs\/\$GITHUB_RUN_ID\/artifacts\?per_page=100/);
  assert.match(seaWorkflow, /github\.ref == 'refs\/heads\/main'/);
  assert.match(seaWorkflow, /actions\/upload-artifact@v4/);
  const productionSteps = seaWorkflow.match(/if: matrix\.target == 'macos-aarch64' && github\.event_name != 'pull_request'/g) || [];
  assert.equal(productionSteps.length, 5, 'only production inputs and executable upload steps run on main after native tests');
});

test('production desktop pin metadata binds the exact SEA, signed manifest bytes, and immutable run artifact', async () => {
  const original = await readFile(coreProducerManifestFixture);
  const coreManifestBytes = Buffer.from(original.toString('utf8').replaceAll('/v0.1.0/', '/nightly/'));
  const manifest = JSON.parse(coreManifestBytes.toString('utf8'));
  const buildPackage = { version: VERSION, sidevoice: { channel: 'nightly', build_seq: 29, connector_sha: 'c'.repeat(40) } };
  const target = { os: 'macos', arch: 'aarch64' };
  const metadata = connectorMetadata({ buildPackage, manifest, manifestSha256: sha256(coreManifestBytes), target, sea: true });
  const version = versionMetadata({ buildPackage, target, sea: true });
  const executableBytes = Buffer.from('test-only Mach-O bytes');
  // Synthetic JSON exercises pin-schema hashing only; it is not a core signature or production evidence.
  const coreManifestSidecarBytes = Buffer.from('{}');
  const artifactRecord = (name, id, size, digestChar) => ({ id, name, size_in_bytes: size,
    archive_download_url: `https://api.github.com/repos/sidevoice/sidevoice-connector/actions/artifacts/${id}/zip`,
    digest: `sha256:${digestChar.repeat(64)}`, expired: false,
    workflow_run: { id: 123, repository_id: 12345, head_repository_id: 12345,
      head_branch: 'main', head_sha: buildPackage.sidevoice.connector_sha } });
  // This fixture covers API metadata mapping only. It is not a connector attestation or production evidence.
  const artifactRecords = [artifactRecord(SEA_ARTIFACT_NAME, 456, 1024, 'a'),
    artifactRecord(PROVENANCE_ARTIFACT_NAME, 457, 512, 'b')];
  const artifactListing = records => ({ total_count: records.length, artifacts: records });
  const pin = createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123, artifactRecords: artifactListing(artifactRecords) });

  assert.equal(pin.status, 'ready');
  assert.equal(pin.target, 'macos-aarch64');
  assert.equal(pin.core_manifest_sha256, sha256(coreManifestBytes));
  assert.equal(pin.core_manifest_size, coreManifestBytes.length);
  assert.equal(pin.core_manifest_bytes_base64, coreManifestBytes.toString('base64'));
  assert.equal(pin.executable_sha256, sha256(executableBytes));
  assert.equal(pin.executable_size, executableBytes.length);
  assert.equal(pin.asset_url, 'https://api.github.com/repos/sidevoice/sidevoice-connector/actions/artifacts/456/zip');
  assert.equal(pin.provenance.artifact_name, SEA_ARTIFACT_NAME);
  assert.deepEqual(pin.provenance.sidecars, [{ name: 'sidevoice-provenance.zip',
    url: 'https://api.github.com/repos/sidevoice/sidevoice-connector/actions/artifacts/457/zip',
    sha256: 'b'.repeat(64), size: 512 }]);
  assert.equal(pin.core_manifest_sidecars[0].sha256, sha256(coreManifestSidecarBytes));
  assert.equal(pin.core_manifest_sidecars[0].url,
    'https://github.com/sidevoice/sidevoice-core/releases/download/nightly/core-manifest.json.sigstore.json');
  assert.deepEqual(pin.core_assets, metadata.embedded_core.assets);

  const source = path.join(scratch, 'sea-roundtrip-source');
  const downloaded = path.join(scratch, 'sea-roundtrip-artifact');
  await mkdir(downloaded, { recursive: true });
  await writeFile(source, executableBytes);
  await writeFile(path.join(downloaded, 'sidevoice'), executableBytes);
  assert.deepEqual(await verifyArtifactRoundTrip(source, downloaded), {
    executable_size: executableBytes.length, executable_sha256: sha256(executableBytes),
  });
  await writeFile(path.join(downloaded, 'unexpected.txt'), 'not part of the root executable artifact');
  await assert.rejects(() => verifyArtifactRoundTrip(source, downloaded), /only root sidevoice/);

  const provenanceDir = path.join(scratch, 'provenance-sidecar-artifact');
  await mkdir(provenanceDir, { recursive: true });
  // Layout placeholder only. The actual Sigstore bundle is generated and cryptographically checked in main CI.
  await writeFile(path.join(provenanceDir, 'sidevoice.sigstore.json'), 'test-only root entry');
  await verifyArtifactEntries(provenanceDir, ['sidevoice.sigstore.json']);
  await writeFile(path.join(provenanceDir, 'extra.json'), '{}');
  await assert.rejects(() => verifyArtifactEntries(provenanceDir, ['sidevoice.sigstore.json']), /exactly the root entries/);

  const coreInputsDir = path.join(scratch, 'core-input-artifact');
  await mkdir(coreInputsDir, { recursive: true });
  await writeFile(path.join(coreInputsDir, 'core-manifest.json'), '{}');
  await writeFile(path.join(coreInputsDir, 'core-manifest.json.sigstore.json'), '{}');
  const verifyEntries = spawnSync(process.execPath, [path.join(connectorPackage, 'sea-artifact.mjs'), 'verify-entries',
    coreInputsDir, 'core-manifest.json', 'core-manifest.json.sigstore.json'], { encoding: 'utf8' });
  assert.equal(verifyEntries.status, 0, verifyEntries.stderr);
  assert.deepEqual(JSON.parse(verifyEntries.stdout), { ok: true,
    entries: ['core-manifest.json', 'core-manifest.json.sigstore.json'] });

  const wrongRouteRecords = artifactRecords.map(record => ({ ...record }));
  wrongRouteRecords[0].archive_download_url = 'https://api.github.com/repos/sidevoice/sidevoice-connector/actions/runs/123/artifacts/456/zip';
  assert.throws(() => createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123, artifactRecords: artifactListing(wrongRouteRecords) }),
  /canonical artifact ZIP route/);

  const wrongProvenanceRun = artifactRecords.map(record => ({ ...record }));
  wrongProvenanceRun[1].workflow_run = { ...wrongProvenanceRun[1].workflow_run, head_sha: 'd'.repeat(40) };
  assert.throws(() => createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123, artifactRecords: artifactListing(wrongProvenanceRun) }),
  /repository, workflow run, and connector commit/);

  const expiredProvenance = artifactRecords.map(record => ({ ...record }));
  expiredProvenance[1].expired = true;
  assert.throws(() => createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123, artifactRecords: artifactListing(expiredProvenance) }),
  /r4b-provenance is expired/);

  const incompleteListing = artifactListing(artifactRecords);
  incompleteListing.total_count += 1;
  assert.throws(() => createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123, artifactRecords: incompleteListing }),
  /listing is invalid or incomplete/);

  assert.throws(() => createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: buildPackage.sidevoice.connector_sha, expectedBuildSeq: buildPackage.sidevoice.build_seq,
    repository: 'sidevoice/sidevoice-connector', repositoryId: '12345',
    workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123,
    artifactRecords: artifactListing(artifactRecords.filter(record => record.name !== PROVENANCE_ARTIFACT_NAME)) }),
  /exactly one sidevoice-connector-macos-aarch64-r4b-provenance artifact/);

  assert.throws(() => createDesktopPinRecord({ executableBytes, version,
    metadata: { ...metadata, embedded_core: { ...metadata.embedded_core, manifest_sha256: null, assets: [] } },
    coreManifestBytes, coreManifestSidecarBytes, expectedConnectorSha: buildPackage.sidevoice.connector_sha,
    expectedBuildSeq: buildPackage.sidevoice.build_seq, repository: 'sidevoice/sidevoice-connector',
    repositoryId: '12345', workflow: '.github/workflows/r4-sea.yml@refs/heads/main', runId: 123,
    artifactRecords: artifactListing(artifactRecords) }),
  /embedded manifest SHA does not match/);
});

async function makeArchive(filename, entries) {
  const pack = tar.pack();
  const done = pipeline(pack, createZstdCompress(), createWriteStream(filename));
  for (const entry of entries) {
    const body = entry.body ?? Buffer.alloc(0);
    await new Promise((resolve, reject) => pack.entry({ name: entry.name, type: entry.type || 'file', linkname: entry.linkname,
      mode: entry.mode ?? 0o644, size: body.length }, body, error => error ? reject(error) : resolve()));
  }
  pack.finalize();
  await done;
}

test('safe archive extraction preserves executable files and internal relative links', async () => {
  const archive = path.join(scratch, 'safe.tar.zst'), dest = path.join(scratch, 'safe-stage');
  await makeArchive(archive, [
    { name: 'python/bin/python3.12', body: Buffer.from('python'), mode: 0o755 },
    { name: 'python/bin/python3', type: 'symlink', linkname: 'python3.12' },
  ]);
  await unpackCoreArchive(archive, dest);
  assert.equal((await readFile(path.join(dest, 'python/bin/python3.12'))).toString(), 'python');
  assert.equal((await stat(path.join(dest, 'python/bin/python3'))).mode & 0o111, 0o111);
});

test('archive traversal, absolute paths, composed escaping links, cycles, and device entries are refused before commit', async () => {
  const cases = [
    { name: '../outside', type: 'file', body: Buffer.from('escape'), check: 'archive-path' },
    { name: '/absolute', type: 'file', body: Buffer.from('escape'), check: 'archive-path' },
    { name: 'python/link', type: 'symlink', linkname: '../../../../outside', check: 'archive-link' },
    { name: 'escape', type: 'symlink', linkname: 'pivot/../outside', check: 'archive-link',
      extra: [{ name: 'pivot', type: 'symlink', linkname: '.' }] },
    { name: 'python/first', type: 'symlink', linkname: 'second', check: 'archive-link',
      extra: [{ name: 'python/second', type: 'symlink', linkname: 'first' }] },
    { name: 'python/device', type: 'character-device', check: 'archive-type' },
  ];
  for (const [index, item] of cases.entries()) {
    const archive = path.join(scratch, `unsafe-${index}.tar.zst`), dest = path.join(scratch, `unsafe-${index}-stage`);
    await makeArchive(archive, [...(item.extra || []), item]);
    await assert.rejects(() => unpackCoreArchive(archive, dest), error => error.key === 'install.authenticity' && error.check === item.check);
    await rm(dest, { recursive: true, force: true });
  }
});

function runVerifier(cachePath, offline) {
  const env = { ...process.env };
  for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
  if (sea && existsSync(sea)) {
    const result = spawnSync(sea, ['--sidevoice-verify-core', fixtureArtifact, fixture, 'release', cachePath, offline ? '1' : '0'],
      { env, encoding: 'utf8', timeout: 120_000 });
    assert.equal(result.status, 0, result.stderr || result.stdout);
    return JSON.parse(result.stdout.trim());
  }
  const result = spawnSync(process.execPath, [helper, fixture, fixtureArtifact, cachePath, offline ? '1' : '0'], { env, encoding: 'utf8', timeout: 120_000 });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  return JSON.parse(result.stdout.trim());
}
async function findFile(dir, basename) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const child = path.join(dir, entry.name);
    if (entry.isFile() && entry.name === basename) return child;
    if (entry.isDirectory()) { const found = await findFile(child, basename); if (found) return found; }
  }
  return null;
}
async function copyTree(from, to) {
  await mkdir(to, { recursive: true });
  for (const entry of await readdir(from, { withFileTypes: true })) {
    const source = path.join(from, entry.name), target = path.join(to, entry.name);
    if (entry.isDirectory()) await copyTree(source, target);
    else await writeFile(target, await readFile(source));
  }
}

test('public-good TUF cold and warm caches verify a genuine Sigstore bundle; wrong workflow is refused afterward', async () => {
  const cache = path.join(scratch, 'tuf-warm');
  const result = runVerifier(cache, false);
  assert.equal(result.key, 'install.authenticity');
  assert.equal(result.check, 'workflow', 'the unrelated, genuine npm bundle must reach and fail the core signer pin');
  assert.ok(await findFile(cache, 'root.json'), 'a cold online cache receives the public-good TUF root');
  assert.ok(await findFile(cache, 'timestamp.json'), 'a cold online cache receives timestamp metadata');
  const warm = runVerifier(cache, true);
  assert.equal(warm.check, 'workflow', 'the warm TUF cache permits offline cryptographic verification');
});

test('cold, expired, and corrupt offline TUF caches fail closed with the Sigstore check named', async () => {
  const cold = runVerifier(path.join(scratch, 'tuf-cold-offline'), true);
  assert.equal(cold.check, 'sigstore');

  const warm = path.join(scratch, 'tuf-source');
  const initialized = runVerifier(warm, false);
  assert.equal(initialized.check, 'workflow');

  const expired = path.join(scratch, 'tuf-expired');
  await copyTree(warm, expired);
  const timestamp = await findFile(expired, 'timestamp.json');
  assert.ok(timestamp, 'timestamp metadata is present in the warm cache');
  const metadata = JSON.parse(await readFile(timestamp, 'utf8'));
  metadata.signed.expires = '2000-01-01T00:00:00Z';
  await writeFile(timestamp, JSON.stringify(metadata));
  assert.equal(runVerifier(expired, true).check, 'sigstore', 'expired metadata is not accepted offline');

  const corrupt = path.join(scratch, 'tuf-corrupt');
  await copyTree(warm, corrupt);
  const corruptTimestamp = await findFile(corrupt, 'timestamp.json');
  await writeFile(corruptTimestamp, '{');
  assert.equal(runVerifier(corrupt, true).check, 'sigstore', 'corrupt metadata is not accepted offline');
});

test('native SEA runs directly, answers MCP stdio and self-spawns its connector without Node on PATH', async t => {
  if (!sea) { t.skip(`no SEA target is configured for ${process.platform}/${process.arch}`); return; }
  try { await stat(sea); } catch { t.skip('target SEA is built by the native target CI job'); return; }
  const version = spawnSync(sea, ['--version'], { encoding: 'utf8', timeout: 10_000 });
  assert.equal(version.status, 0, version.stderr);
  assert.equal(version.stdout.trim(), VERSION);
  const versionJson = spawnSync(sea, ['--version', '--json'], { encoding: 'utf8', timeout: 10_000 });
  assert.equal(versionJson.status, 0, versionJson.stderr);
  const versionInfo = JSON.parse(versionJson.stdout.trim());
  assert.deepEqual(Object.keys(versionInfo), ['ok', 'version', 'target', 'channel', 'connector_sha', 'build_seq', 'format', 'sea']);
  assert.equal(versionInfo.ok, true);
  assert.equal(versionInfo.version, VERSION);
  assert.equal(versionInfo.target, targetName);
  assert.equal(versionInfo.sea, true);
  assert.equal(versionInfo.format, 'sea');
  assert.match(versionInfo.connector_sha, /^[0-9a-f]{40}$/);
  const metadataResult = spawnSync(sea, ['metadata', '--json'], { encoding: 'utf8', timeout: 10_000 });
  assert.equal(metadataResult.status, 0, metadataResult.stderr);
  const metadata = JSON.parse(metadataResult.stdout.trim());
  assert.equal(metadata.ok, true);
  assert.equal(metadata.connector.sha, versionInfo.connector_sha);
  assert.equal(metadata.connector.target, targetName);
  assert.equal(metadata.connector.sea, true);
  assert.equal(metadata.connector.version, versionInfo.version);
  assert.equal(metadata.embedded_core.version, CORE_VERSION);
  assert.deepEqual(metadata.protocols, { metadata: 'sidevoice-metadata-v1', progress: 'sidevoice-progress-jsonl-v1' });

  const dataDir = path.join(scratch, 'mcp-machine'), emptyPath = path.join(scratch, 'empty-path');
  await mkdir(emptyPath, { recursive: true });
  const fakeCore = path.join(connectorPackage, 'test', 'fake-sidevoice-core.mjs');
  const coreWrapper = path.join(scratch, 'sidevoice-core');
  await writeFile(coreWrapper, `#!/bin/sh\nexec "${process.execPath}" "${fakeCore}" "$@"\n`, { mode: 0o755 });
  const env = { ...process.env, PATH: emptyPath, HOME: scratch, XDG_DATA_HOME: path.join(scratch, 'xdg'),
    SIDEVOICE_DATA_DIR: dataDir, SIDEVOICE_CORE_BIN: coreWrapper, SIDEVOICE_SERVICE_MANAGER: 'none',
    SIDEVOICE_CONNECTOR_IDLE_MS: '300', SIDEVOICE_CORE_PORT: '0', CLAUDE_CODE_SESSION_ID: 'r4-sea' };
  for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
  const child = spawn(sea, ['mcp'], { env, stdio: ['pipe', 'pipe', 'pipe'] });
  const replies = new Map(); let buffer = '', stderr = '';
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  child.stdout.on('data', chunk => { buffer += chunk; let index; while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index); buffer = buffer.slice(index + 1); if (line) { const reply = JSON.parse(line); replies.set(reply.id, reply); }
  } });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const waitFor = async predicate => { const deadline = Date.now() + 15_000; while (Date.now() < deadline) { if (predicate()) return; await new Promise(resolve => setTimeout(resolve, 25)); } throw new Error(`SEA MCP/supervisor timed out: ${stderr}`); };
  try {
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18' } }) + '\n');
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }) + '\n');
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'voice_pair_device', arguments: {} } }) + '\n');
    await waitFor(() => replies.has(3));
    assert.equal(replies.get(1).result.serverInfo.name, 'sidevoice');
    assert.deepEqual(replies.get(2).result.tools.map(item => item.name), ['voice_connect', 'voice_pair', 'voice_say', 'voice_disconnect', 'voice_pair_device', 'voice_status']);
    assert.equal(replies.get(3).result.content[0].type, 'text');
    assert.match(replies.get(3).result.content[0].text, /One-time code to pair a device/);
    await waitFor(() => existsSync(connectorSocketOf({ SIDEVOICE_DATA_DIR: dataDir })));
    const ready = JSON.parse(await readFile(path.join(dataDir, 'core', 'core.json'), 'utf8'));
    assert.ok(ready.pid, 'the self-spawned connector starts the core it supervises');
  } finally {
    child.stdin.end();
    await new Promise(resolve => child.once('exit', resolve));
    const readyPath = path.join(dataDir, 'core', 'core.json');
    try { const ready = JSON.parse(await readFile(readyPath, 'utf8')); process.kill(ready.pid, 'SIGTERM'); } catch {}
    const lockPath = path.join(dataDir, 'connector.lock');
    try { const lock = readLock(lockPath); if (lock?.pid) process.kill(lock.pid, 'SIGTERM'); } catch {}
  }
});

test('native SEA uninstalls a no-core, no-agent install and removes all non-lock state', async t => {
  if (!sea) { t.skip(`no SEA target is configured for ${process.platform}/${process.arch}`); return; }
  try { await stat(sea); } catch { t.skip('target SEA is built by the native target CI job'); return; }

  const home = path.join(scratch, 'sea-uninstall-home');
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.startsWith('SIDEVOICE_')) delete env[key];
  for (const key of ['GH_TOKEN', 'GITHUB_TOKEN', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN']) delete env[key];
  Object.assign(env, { HOME: home, XDG_DATA_HOME: path.join(home, 'xdg'), XDG_CONFIG_HOME: path.join(home, 'config'),
    SIDEVOICE_SERVICE_MANAGER: 'none' });

  const installed = spawnSync(sea, ['install', '--no-core', '--no-agents', '--json', '--progress=jsonl'], { env, encoding: 'utf8', timeout: 120_000 });
  assert.equal(installed.status, 0, installed.stderr || installed.stdout);
  assert.equal(installed.stdout.trim().split(/\r?\n/).length, 1, 'stdout contains only the final install JSON');
  assert.equal(JSON.parse(installed.stdout.trim()).ok, true);
  const progress = installed.stderr.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
  assert.ok(progress.length >= 3, 'install emits structured lifecycle progress');
  assert.ok(progress.every(event => event.type === 'progress' && ['download', 'verify', 'stage', 'service-start',
    'wait-calls', 'wait-lock', 'commit', 'pairing', 'rollback'].includes(event.step)));
  assert.ok(progress.some(event => event.step === 'stage'));
  assert.ok(progress.some(event => event.step === 'commit'));
  assert.ok(progress.some(event => event.step === 'pairing'));
  assert.ok(installed.stderr.trim().split(/\r?\n/).every(line => Buffer.byteLength(line) <= 1024), 'each progress record is bounded');
  const dataDir = dataDirOf(env);
  const installation = JSON.parse(await readFile(nodeFiles(dataDir).install, 'utf8'));
  assert.equal(installation.command.length, 1, 'a SEA install records the one-element executable command');
  assert.ok((await readdir(releaseLayout(env).releases)).length > 0, 'the install staged a release before uninstall');

  const removed = spawnSync(sea, ['uninstall', '--json'], { env, encoding: 'utf8', timeout: 120_000 });
  assert.equal(removed.status, 0, removed.stderr || removed.stdout);
  assert.deepEqual(JSON.parse(removed.stdout.trim()), { ok: true, state: 'absent' });
  assert.equal(existsSync(releaseLayout(env).root), false, 'the release tree is gone');
  for (const name of ['install.json', 'mcp.log', 'connector.log', 'core.log', 'core.stderr.log', 'credentials.json']) {
    assert.equal(existsSync(path.join(dataDir, name)), false, `${name} is removed`);
  }
  const leftovers = await readdir(dataDir);
  const permanent = new Set(['install.lock', 'connector.lock', 'node-stopped.json']);
  assert.deepEqual(leftovers.filter(name => !permanent.has(name)), [], 'only the permanent locks and stop marker remain');
});

test('connector CLI acknowledges pre-commit cancellation and reports the committed outcome after SIGINT', async () => {
  const runPausedInstall = async ({ name, resume, stopped = false }) => {
    const home = path.join(scratch, `${name}-home`), hooks = path.join(scratch, `${name}-hooks`);
    await mkdir(hooks, { recursive: true, mode: 0o700 });
    await writeFile(path.join(hooks, `pause-${name}`), '');
    const env = testInstallEnv(home, { SIDEVOICE_TEST_HOOKS: hooks, SIDEVOICE_INSTALL_FROM_SOURCE: '1' });
    const stopMarker = nodeFiles(dataDirOf(env)).stopped;
    if (stopped) {
      await mkdir(path.dirname(stopMarker), { recursive: true, mode: 0o700 });
      await writeFile(stopMarker, JSON.stringify({ by: 'person' }));
    }
    const child = spawn(process.execPath, [path.join(connectorPackage, 'cli.mjs'), 'install', '--no-core', '--no-agents', '--json', '--progress=jsonl'],
      { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.setEncoding('utf8').on('data', chunk => { stdout += chunk; });
    child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
    const closed = new Promise(resolve => child.once('close', (code, childSignal) => resolve({ code, childSignal })));
    try { await waitForPath(path.join(hooks, `paused-${name}`), 60_000); }
    catch (error) {
      child.kill('SIGKILL');
      await closed;
      throw new Error(`${error.message}; CLI stdout=${stdout}; stderr=${stderr}`);
    }
    child.kill('SIGINT');
    if (resume) {
      await new Promise(resolve => setTimeout(resolve, 75));
      await writeFile(path.join(hooks, `resume-${name}`), '');
    }
    const outcome = await closed;
    assert.equal(outcome.childSignal, null, 'the CLI handles SIGINT and returns a final JSON outcome');
    assert.equal(stdout.trim().split(/\r?\n/).length, 1, 'stdout contains exactly one final JSON object');
    const json = JSON.parse(stdout.trim());
    const events = stderr.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
    assert.ok(events.every(event => event.type === 'progress' && Buffer.byteLength(JSON.stringify(event)) <= 1024));
    return { home, env, json, events, code: outcome.code, stopMarker };
  };

  const before = await runPausedInstall({ name: 'install-before-commit', resume: false, stopped: true });
  assert.equal(before.code, 1);
  assert.equal(before.json.ok, false);
  assert.equal(before.json.error.key, 'install.cancelled');
  assert.ok(before.events.some(event => event.step === 'stage'));
  assert.equal(existsSync(releaseLayout(before.env).current), false, 'cancelled candidate never becomes current');
  assert.deepEqual(await readdir(releaseLayout(before.env).releases), [], 'staged candidate is cleaned after cancellation');
  assert.equal(existsSync(before.stopMarker), true, 'pre-commit cancellation preserves an existing stop intent');

  const after = await runPausedInstall({ name: 'install-after-commit', resume: true });
  assert.equal(after.code, 0);
  assert.equal(after.json.ok, true, 'SIGINT after commit does not report a cancellation');
  assert.ok(existsSync(releaseLayout(after.env).current), 'the committed install completes and remains selected');
  assert.ok(after.events.some(event => event.step === 'commit'));
});

test('SEA private Cursor SQLite helper reads a chat without treating the executable as node -e', async t => {
  if (!sea) { t.skip(`no SEA target is configured for ${process.platform}/${process.arch}`); return; }
  try { await stat(sea); } catch { t.skip('target SEA is built by the native target CI job'); return; }
  const { DatabaseSync } = await import('node:sqlite');
  const database = path.join(scratch, 'cursor-state.vscdb'), marker = 'c'.repeat(64);
  const db = new DatabaseSync(database);
  db.exec('CREATE TABLE cursorDiskKV (key TEXT, value BLOB)');
  db.prepare('INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)').run('bubbleId:thread-from-sea:message-1', Buffer.from(JSON.stringify({ marker })));
  db.close();
  const query = spawnSync(sea, ['--sidevoice-cursor-db-query', database, marker, 'bubbleId:', 'bubbleId;'], { encoding: 'utf8', timeout: 15_000 });
  assert.equal(query.status, 0, query.stderr);
  assert.deepEqual(JSON.parse(query.stdout), ['thread-from-sea']);
});
