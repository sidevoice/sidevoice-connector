/** Build the unchanged npm ESM client and, on request, a native Node 22 CommonJS SEA executable. */
import { chmod, copyFile, lstat, mkdir, readFile, readdir, rm, stat, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { CORE_VERSION } from './core.mjs';
import { sha256, verifyCoreArtifact, verifyRustCoreArtifact } from './core-attestation.mjs';
import { validateCoreManifest } from './core-bundle.mjs';
import { parseCanonicalRustCoreJson, RUST_CORE_TARGETS, rustCoreTarget, validateRustCoreManifest } from './rust-core.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(root, 'dist');
const outSea = path.join(root, 'dist-sea');
const packagePath = path.join(root, 'package.json');
const exactKeys = (value, keys) => value && typeof value === 'object' && !Array.isArray(value)
  && Object.keys(value).sort().join(',') === [...keys].sort().join(',');
const original = JSON.parse(await readFile(packagePath, 'utf8'));
const shipped = { ...original };
delete shipped.devDependencies;

let connectorSha = process.env.SIDEVOICE_CONNECTOR_SHA || process.env.GITHUB_SHA || null;
if (!connectorSha) {
  try { connectorSha = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(); }
  catch { connectorSha = null; }
}
if (connectorSha !== null && !/^[0-9a-f]{40}$/.test(connectorSha)) throw new Error('connector build SHA must be 40 lowercase hexadecimal characters');

// Build channel/order metadata is copied into ESM and compiled into the SEA executable.
const channel = process.env.SIDEVOICE_CHANNEL || 'release';
if (!['release', 'nightly'].includes(channel)) throw new Error(`SIDEVOICE_CHANNEL is ${channel}: release or nightly`);
const build_seq = Number(process.env.SIDEVOICE_BUILD_SEQ || 0);
if (!Number.isSafeInteger(build_seq) || build_seq < 0) throw new Error(`SIDEVOICE_BUILD_SEQ is ${process.env.SIDEVOICE_BUILD_SEQ}: a non-negative integer`);
shipped.sidevoice = { ...(shipped.sidevoice || {}), channel, build_seq, connector_sha: connectorSha };

if (process.env.SIDEVOICE_REQUIRE_CORE_MANIFEST === '1' && !process.env.SIDEVOICE_CORE_MANIFEST) {
  throw new Error('SIDEVOICE_CORE_MANIFEST is required for release and nightly builds');
}

let manifestText = null;
let manifestSha256 = null;
if (process.env.SIDEVOICE_CORE_MANIFEST) {
  const manifestPath = path.resolve(process.env.SIDEVOICE_CORE_MANIFEST);
  const sidecarPath = process.env.SIDEVOICE_CORE_MANIFEST_SIGSTORE
    ? path.resolve(process.env.SIDEVOICE_CORE_MANIFEST_SIGSTORE) : `${manifestPath}.sigstore.json`;
  const raw = await readFile(manifestPath);
  const sidecar = await readFile(sidecarPath).catch(() => { throw new Error(`missing signed core manifest sidecar: ${sidecarPath}`); });
  const parsed = JSON.parse(raw.toString('utf8'));
  const channel = shipped.sidevoice?.channel || 'release';
  validateCoreManifest(parsed, CORE_VERSION, channel);
  await verifyCoreArtifact({ bytes: raw, bundleBytes: sidecar, channel,
    tufCachePath: process.env.SIDEVOICE_TUF_CACHE || path.join(os.homedir(), '.sidevoice', 'sigstore-build'), label: 'core-manifest.json' });
  // Preserve the signed manifest's exact bytes in the generated JavaScript string.
  manifestText = raw.toString('utf8');
  manifestSha256 = (await import('node:crypto')).createHash('sha256').update(raw).digest('hex');
}

let rustManifestText = null;
let rustManifestSha256 = null;
let rustSourceSha = null;
let rustTarget = null;
let rustArchivePath = null;
let rustArchiveSha256 = null;
let rustArchiveSize = null;
if (process.env.SIDEVOICE_REQUIRE_RUST_CORE === '1') {
  if (process.env.SIDEVOICE_BUILD_SEA !== '1') throw new Error('native Core inputs may be embedded only in a target SEA');
  if (manifestText !== null || process.env.SIDEVOICE_CORE_MANIFEST) throw new Error('Python and native Core manifests cannot be selected in the same SEA');
  const pin = JSON.parse(await readFile(path.join(root, 'rust-core-production-pin.json'), 'utf8'));
  const sourcePattern = /^[0-9a-f]{40}$/;
  const digestPattern = /^[0-9a-f]{64}$/;
  if (!exactKeys(pin, ['schema', 'repository', 'workflow', 'ref', 'run_id', 'source_sha', 'manifest', 'bundles'])
      || pin.schema !== 1 || pin.repository !== 'sidevoice/sidevoice-core'
      || pin.workflow !== '.github/workflows/rust-t7.yml' || pin.ref !== 'refs/heads/main'
      || !/^\d+$/.test(pin.run_id) || !sourcePattern.test(pin.source_sha || '')
      || !pin.manifest || !exactKeys(pin.manifest, ['name', 'size', 'sha256']) || pin.manifest.name !== 'native-core-manifest.json'
      || !Number.isSafeInteger(pin.manifest.size) || !digestPattern.test(pin.manifest.sha256 || '')
      || !pin.bundles || Object.keys(pin.bundles).sort().join(',') !== [...RUST_CORE_TARGETS].sort().join(',')) {
    throw new Error('native Core production pin is malformed');
  }
  if ((process.env.SIDEVOICE_RUST_CORE_RUN_ID && process.env.SIDEVOICE_RUST_CORE_RUN_ID !== pin.run_id)
      || (process.env.SIDEVOICE_RUST_CORE_SOURCE_SHA && process.env.SIDEVOICE_RUST_CORE_SOURCE_SHA !== pin.source_sha)) {
    throw new Error('native Core build input does not come from the pinned protected-main run');
  }
  rustTarget = rustCoreTarget();
  if (!rustTarget || !RUST_CORE_TARGETS.includes(rustTarget)
      || (process.env.SIDEVOICE_RUST_CORE_TARGET && process.env.SIDEVOICE_RUST_CORE_TARGET !== rustTarget)) {
    throw new Error(`native Core has no exact input for this SEA target (${process.platform}/${process.arch})`);
  }
  const manifestPath = path.resolve(process.env.SIDEVOICE_RUST_CORE_MANIFEST || '');
  const manifestSigstorePath = process.env.SIDEVOICE_RUST_CORE_MANIFEST_SIGSTORE
    ? path.resolve(process.env.SIDEVOICE_RUST_CORE_MANIFEST_SIGSTORE) : `${manifestPath}.sigstore.json`;
  const archiveRecord = pin.bundles[rustTarget];
  if (!archiveRecord || !exactKeys(archiveRecord, ['name', 'size', 'sha256'])
      || archiveRecord.name !== `sidevoice-core-rust-${pin.source_sha}-${rustTarget}.tar.zst`
      || !Number.isSafeInteger(archiveRecord.size) || archiveRecord.size < 1
      || !digestPattern.test(archiveRecord.sha256 || '')) throw new Error(`native Core ${rustTarget} pin is malformed`);
  for (const target of RUST_CORE_TARGETS) {
    const record = pin.bundles[target];
    if (!record || !exactKeys(record, ['name', 'size', 'sha256'])
        || record.name !== `sidevoice-core-rust-${pin.source_sha}-${target}.tar.zst`
        || !Number.isSafeInteger(record.size) || record.size < 1 || !digestPattern.test(record.sha256 || '')) {
      throw new Error(`native Core ${target} production pin is malformed`);
    }
  }
  rustArchivePath = path.resolve(process.env.SIDEVOICE_RUST_CORE_ARCHIVE || path.join(path.dirname(manifestPath), archiveRecord.name));
  const archiveSigstorePath = process.env.SIDEVOICE_RUST_CORE_ARCHIVE_SIGSTORE
    ? path.resolve(process.env.SIDEVOICE_RUST_CORE_ARCHIVE_SIGSTORE) : `${rustArchivePath}.sigstore.json`;
  const inputDirectory = path.dirname(manifestPath);
  const expectedInputNames = [pin.manifest.name, `${pin.manifest.name}.sigstore.json`,
    ...RUST_CORE_TARGETS.flatMap(target => [pin.bundles[target].name, `${pin.bundles[target].name}.sigstore.json`])].sort();
  const actualInputNames = (await readdir(inputDirectory)).sort();
  if (path.dirname(manifestSigstorePath) !== inputDirectory || path.dirname(rustArchivePath) !== inputDirectory
      || path.dirname(archiveSigstorePath) !== inputDirectory || path.basename(manifestPath) !== pin.manifest.name
      || path.basename(manifestSigstorePath) !== `${pin.manifest.name}.sigstore.json`
      || path.basename(rustArchivePath) !== archiveRecord.name
      || path.basename(archiveSigstorePath) !== `${archiveRecord.name}.sigstore.json`
      || actualInputNames.join('\n') !== expectedInputNames.join('\n')) {
    throw new Error('native Core run artifact does not have its exact pinned input membership');
  }
  const [manifestBytes, manifestSidecar, archiveBytes, archiveSidecar] = await Promise.all([
    readFile(manifestPath), readFile(manifestSigstorePath).catch(() => { throw new Error(`missing signed native Core manifest sidecar: ${manifestSigstorePath}`); }),
    readFile(rustArchivePath), readFile(archiveSigstorePath).catch(() => { throw new Error(`missing signed native Core archive sidecar: ${archiveSigstorePath}`); }),
  ]);
  if (manifestBytes.length !== pin.manifest.size || sha256(manifestBytes) !== pin.manifest.sha256) {
    throw new Error('native Core manifest differs from the protected-main production pin');
  }
  const manifest = parseCanonicalRustCoreJson(manifestBytes, 'native Core manifest');
  validateRustCoreManifest(manifest, { expectedSourceSha: pin.source_sha });
  for (const target of RUST_CORE_TARGETS) {
    const expected = pin.bundles[target];
    const actual = manifest.bundles[target];
    if (!expected || expected.name !== actual.name || expected.size !== actual.size || expected.sha256 !== actual.sha256) {
      throw new Error(`native Core manifest ${target} record differs from the protected-main production pin`);
    }
  }
  await verifyRustCoreArtifact({ bytes: manifestBytes, expectedSha256: pin.manifest.sha256, bundleBytes: manifestSidecar,
    subjectName: pin.manifest.name, tufCachePath: process.env.SIDEVOICE_TUF_CACHE || path.join(os.tmpdir(), 'sidevoice-tuf'),
    label: pin.manifest.name });
  if (archiveBytes.length !== archiveRecord.size || sha256(archiveBytes) !== archiveRecord.sha256) {
    throw new Error(`native Core ${rustTarget} archive differs from the protected-main production pin`);
  }
  await verifyRustCoreArtifact({ bytes: archiveBytes, expectedSha256: archiveRecord.sha256, bundleBytes: archiveSidecar,
    subjectName: archiveRecord.name, tufCachePath: process.env.SIDEVOICE_TUF_CACHE || path.join(os.tmpdir(), 'sidevoice-tuf'),
    label: archiveRecord.name });
  rustManifestText = manifestBytes.toString('utf8');
  rustManifestSha256 = pin.manifest.sha256;
  rustSourceSha = pin.source_sha;
  rustArchiveSha256 = archiveRecord.sha256;
  rustArchiveSize = archiveRecord.size;
}

let rustConnectorPath = null;
let rustConnectorTarget = null;
let rustConnectorSourceSha = null;
let rustConnectorSha256 = null;
let rustConnectorSize = null;
if (process.env.SIDEVOICE_REQUIRE_RUST_CONNECTOR === '1') {
  if (process.env.SIDEVOICE_BUILD_SEA !== '1' || process.env.SIDEVOICE_REQUIRE_RUST_CORE !== '1') {
    throw new Error('the Rust Connector can be embedded only beside its pinned native Core in a target SEA');
  }
  rustConnectorTarget = rustCoreTarget();
  rustConnectorSourceSha = connectorSha;
  if (!rustConnectorTarget || !RUST_CORE_TARGETS.includes(rustConnectorTarget)
      || process.env.SIDEVOICE_RUST_CONNECTOR_TARGET !== rustConnectorTarget
      || !/^[0-9a-f]{40}$/.test(connectorSha || '')
      || process.env.SIDEVOICE_RUST_CONNECTOR_SOURCE_SHA !== connectorSha) {
    throw new Error('Rust Connector build identity does not match this exact target and source commit');
  }
  rustConnectorPath = path.resolve(process.env.SIDEVOICE_RUST_CONNECTOR_BINARY || '');
  const binary = await readFile(rustConnectorPath);
  const binaryInfo = await lstat(rustConnectorPath);
  rustConnectorSize = binary.length;
  rustConnectorSha256 = (await import('node:crypto')).createHash('sha256').update(binary).digest('hex');
  if (!binaryInfo.isFile() || binaryInfo.isSymbolicLink() || !(binaryInfo.mode & 0o111) || rustConnectorSize < 1 || rustConnectorSize > 100_000_000) {
    throw new Error('Rust Connector binary is not a bounded executable');
  }
  const runtime = execFileSync(rustConnectorPath, ['runtime-identity', '--json'], {
    encoding: 'utf8', timeout: 10_000, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let identity;
  try { identity = JSON.parse(runtime); } catch { throw new Error('Rust Connector did not report valid runtime identity JSON'); }
  if (!exactKeys(identity, ['kind', 'target', 'source_sha', 'version']) || identity.kind !== 'rust-native-v1'
      || identity.target !== rustConnectorTarget || identity.source_sha !== connectorSha || identity.version !== shipped.version) {
    throw new Error('Rust Connector binary does not match the exact candidate build identity');
  }
}

await rm(out, { recursive: true, force: true });
await mkdir(out, { recursive: true });
await build({
  entryPoints: [path.join(root, 'cli.mjs')],
  outfile: path.join(out, 'cli.mjs'),
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node22',
  external: ['node:*'],
  legalComments: 'none',
  define: {
    __SIDEVOICE_CORE_MANIFEST_JSON__: JSON.stringify(manifestText ?? 'null'),
    __SIDEVOICE_CORE_MANIFEST_SHA256__: JSON.stringify(manifestSha256 ?? 'null'),
    __SIDEVOICE_RUST_CORE_MANIFEST_JSON__: JSON.stringify(null),
    __SIDEVOICE_RUST_CORE_MANIFEST_SHA256__: JSON.stringify(null),
    __SIDEVOICE_RUST_CORE_SOURCE_SHA__: JSON.stringify(null),
    __SIDEVOICE_RUST_CORE_TARGET__: JSON.stringify(null),
    __SIDEVOICE_RUST_CORE_ARCHIVE_SHA256__: JSON.stringify(null),
    __SIDEVOICE_RUST_CORE_ARCHIVE_SIZE__: 'null',
    __SIDEVOICE_RUST_CONNECTOR_TARGET__: JSON.stringify(null),
    __SIDEVOICE_RUST_CONNECTOR_SOURCE_SHA__: JSON.stringify(null),
    __SIDEVOICE_RUST_CONNECTOR_BINARY_SHA256__: JSON.stringify(null),
    __SIDEVOICE_RUST_CONNECTOR_BINARY_SIZE__: 'null',
  },
});

// The ESM build keeps createRequire for CommonJS dependencies such as ws optional speed-ups.
const esm = path.join(out, 'cli.mjs');
const code = await readFile(esm, 'utf8');
const requireBanner = 'import { createRequire as __sidevoiceCreateRequire } from "node:module";\nconst require = __sidevoiceCreateRequire(import.meta.url);\n';
await writeFile(esm, code.replace(/^#!.*\n/, line => line + requireBanner), { mode: 0o755 });
await writeFile(path.join(out, 'package.json'), JSON.stringify(shipped, null, 2) + '\n');

// The npm artifact above remains ESM and platform independent. CI opts into a native SEA per target runner.
if (process.env.SIDEVOICE_BUILD_SEA === '1') {
  const pin = JSON.parse(await readFile(path.join(root, 'sea-targets.json'), 'utf8'));
  if (process.version !== pin.nodeVersion) throw new Error(`SEA requires pinned Node ${pin.nodeVersion}; got ${process.version}`);
  const target = pin.targets.find(item => item.platform === process.platform && item.nodeArch === process.arch);
  if (!target) throw new Error(`SEA has no native target for ${process.platform}/${process.arch}`);

  await rm(outSea, { recursive: true, force: true });
  await mkdir(outSea, { recursive: true });
  const cjs = path.join(outSea, 'sea.cjs');
  const configPath = path.join(outSea, 'sea-config.json');
  const blobPath = path.join(outSea, 'sea-prep.blob');
  const executablePath = path.join(outSea, 'sidevoice');
  await build({
    entryPoints: [path.join(root, 'sea.mjs')],
    outfile: cjs,
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node22',
    packages: 'bundle',
    external: ['node:*'],
    legalComments: 'none',
    define: {
      __SIDEVOICE_PACKAGE_JSON__: JSON.stringify(JSON.stringify(shipped)),
      __SIDEVOICE_CORE_MANIFEST_JSON__: JSON.stringify(manifestText ?? 'null'),
      __SIDEVOICE_CORE_MANIFEST_SHA256__: JSON.stringify(manifestSha256 ?? 'null'),
      __SIDEVOICE_RUST_CORE_MANIFEST_JSON__: JSON.stringify(rustManifestText),
      __SIDEVOICE_RUST_CORE_MANIFEST_SHA256__: JSON.stringify(rustManifestSha256),
      __SIDEVOICE_RUST_CORE_SOURCE_SHA__: JSON.stringify(rustSourceSha),
      __SIDEVOICE_RUST_CORE_TARGET__: JSON.stringify(rustTarget),
      __SIDEVOICE_RUST_CORE_ARCHIVE_SHA256__: JSON.stringify(rustArchiveSha256),
      __SIDEVOICE_RUST_CORE_ARCHIVE_SIZE__: JSON.stringify(rustArchiveSize),
      __SIDEVOICE_RUST_CONNECTOR_TARGET__: JSON.stringify(rustConnectorTarget),
      __SIDEVOICE_RUST_CONNECTOR_SOURCE_SHA__: JSON.stringify(rustConnectorSourceSha),
      __SIDEVOICE_RUST_CONNECTOR_BINARY_SHA256__: JSON.stringify(rustConnectorSha256),
      __SIDEVOICE_RUST_CONNECTOR_BINARY_SIZE__: JSON.stringify(rustConnectorSize),
      'import.meta.url': JSON.stringify('file:///sidevoice-runtime/sea.mjs'),
    },
  });
  await writeFile(configPath, JSON.stringify({ main: cjs, output: blobPath,
    disableExperimentalSEAWarning: true, useCodeCache: false,
    ...(rustArchivePath || rustConnectorPath ? { assets: {
      ...(rustArchivePath ? { 'sidevoice-rust-core.tar.zst': rustArchivePath } : {}),
      ...(rustConnectorPath ? { 'sidevoice-rust-connector': rustConnectorPath } : {}),
    } } : {}) }, null, 2) + '\n');
  execFileSync(process.execPath, ['--experimental-sea-config', configPath], { cwd: root, stdio: 'inherit' });
  await copyFile(process.execPath, executablePath);
  await chmod(executablePath, 0o755);
  if (process.platform === 'darwin') execFileSync('codesign', ['--remove-signature', executablePath], { stdio: 'inherit' });
  const postject = path.resolve(root, '..', '..', 'node_modules', '.bin', 'postject');
  const injectArgs = [executablePath, 'NODE_SEA_BLOB', blobPath, '--sentinel-fuse', 'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2'];
  if (process.platform === 'darwin') injectArgs.push('--macho-segment-name', 'NODE_SEA');
  execFileSync(postject, injectArgs, { stdio: 'inherit' });
  if (process.platform === 'darwin') execFileSync('codesign', ['--force', '--sign', '-', '--timestamp=none', executablePath], { stdio: 'inherit' });
  const builtPath = path.join(outSea, target.name);
  await mkdir(builtPath, { recursive: true });
  const targetExecutable = path.join(builtPath, 'sidevoice');
  await copyFile(executablePath, targetExecutable);
  await chmod(targetExecutable, 0o755);
  const [executable, blob] = await Promise.all([stat(targetExecutable), stat(blobPath)]);
  process.stdout.write(JSON.stringify({ node: process.version, target: target.name, executable: targetExecutable,
    executableBytes: executable.size, seaBlobBytes: blob.size, manifestEmbedded: !!manifestText,
    rustCoreTarget: rustTarget, rustCoreArchiveSha256: rustArchiveSha256,
    rustConnectorTarget, rustConnectorSha256 }) + '\n');
}

// An explicitly named wheel is copied into the ESM artifact only for the existing developer workflow. Production
// core installs use the signed R4-a manifest and the target bundle (or its verified wheel fallback).
const wheel = process.env.SIDEVOICE_CORE_WHEEL;
if (wheel) {
  const expected = `sidevoice_core-${CORE_VERSION}-py3-none-any.whl`;
  if (path.basename(wheel) !== expected) throw new Error(`SIDEVOICE_CORE_WHEEL is ${path.basename(wheel)}, but this package pins ${expected}`);
  await mkdir(path.join(out, 'core'), { recursive: true });
  await copyFile(wheel, path.join(out, 'core', expected));
}
