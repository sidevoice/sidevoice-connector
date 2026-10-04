import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { chmod, mkdir, mkdtemp, readFile, readdir, rename, rm, realpath, stat, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createZstdCompress } from 'node:zlib';
import tar from 'tar-stream';
import { finished } from 'node:stream/promises';
import { spawnSync } from 'node:child_process';
import { CORE_VERSION, coreArgs, isRustCoreProgram, rustCoreEnvironment, rustCoreRootForProgram, selfTest } from '../core.mjs';
import { coreProgram, linkCoreRuntimeIntoRelease, point, releaseLayout, selectedMcpCommand } from '../release.mjs';
import { matchesSelectedRustConnectorIdentity, RUST_CONNECTOR_KIND } from '../rust-connector.mjs';
import { writePrivateFile } from '../secure-fs.mjs';
import { CORE_ISSUER, CORE_REPOSITORY, CORE_REPOSITORY_ID, RUST_CORE_SIGNER, SLSA_PREDICATE,
  enforceRustCoreProvenance, sha256, verifyRustCoreArtifact } from '../core-attestation.mjs';
import { parseCanonicalRustCoreJson, RUST_CORE_ENTRYPOINT, RUST_CORE_KIND, RUST_CORE_TARGETS,
  rustCoreTarget, unpackRustCoreArchive, validateRustCoreManifest } from '../rust-core.mjs';

const sourceSha = 'b41840e41e3eb81905d285514c7deb35bd8efe57';
const target = 'linux-x86_64';
const required = {
  'bin/sidevoice-core-rust': Buffer.from('native entrypoint'),
  'checks/detector-16k.wav': Buffer.from('wav fixture'),
  'models/silero.onnx': Buffer.from('silero model'),
  'models/silero_vad_16k.bin': Buffer.from('vad model'),
  'models/smart_turn_weights.bin.gz': Buffer.from('smart turn model'),
  'notices/LICENSE': Buffer.from('notice'),
};

const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const canonical = value => Array.isArray(value) ? `[${value.map(canonical).join(',')}]`
  : value && typeof value === 'object' ? `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
    : JSON.stringify(value);
const OIDS = { issuer: '1.3.6.1.4.1.57264.1.1', buildSigner: '1.3.6.1.4.1.57264.1.9',
  runner: '1.3.6.1.4.1.57264.1.11', source: '1.3.6.1.4.1.57264.1.12',
  repositoryId: '1.3.6.1.4.1.57264.1.15', buildConfig: '1.3.6.1.4.1.57264.1.18' };
const oidValue = value => { const bytes = Buffer.from(value, 'utf8'); return Buffer.concat([Buffer.from([0x0c, bytes.length]), bytes]); };
function rustSigner(overrides = {}) {
  const values = { issuer: CORE_ISSUER, buildSigner: RUST_CORE_SIGNER, source: CORE_REPOSITORY,
    repositoryId: CORE_REPOSITORY_ID, runner: 'github-hosted', buildConfig: RUST_CORE_SIGNER, ...overrides };
  return { identity: { extensions: { issuer: values.issuer }, subjectAlternativeName: values.buildSigner,
    oids: Object.entries(values).map(([key, value]) => ({ oid: { id: OIDS[key].split('.').map(Number) },
      value: key === 'issuer' ? Buffer.from(value, 'utf8') : oidValue(value) })) } };
}
function rustStatement({ subjectName = 'native-core-manifest.json', digest: subjectDigest = 'a'.repeat(64),
  workflowRef = 'refs/heads/main', workflowPath = '.github/workflows/rust-t7.yml',
  predicateType = SLSA_PREDICATE, runner = 'github-hosted' } = {}) {
  return { _type: 'https://in-toto.io/Statement/v1', predicateType,
    subject: [{ name: subjectName, digest: { sha256: subjectDigest } }],
    predicate: { buildDefinition: { externalParameters: { workflow: { repository: CORE_REPOSITORY, ref: workflowRef, path: workflowPath } },
      internalParameters: { github: { repository_id: CORE_REPOSITORY_ID, runner_environment: runner } } },
      runDetails: { builder: { id: RUST_CORE_SIGNER } } } };
}
const expectRefusal = (run, check) => assert.throws(run, error => error.key === 'install.authenticity' && error.check === check);

async function archiveFor({ root = 'sidevoice-core-rust', inventoryEdit = value => value, extraEntries = [] } = {}) {
  const files = new Map(Object.entries(required));
  const inventory = inventoryEdit({ schema: 1, kind: RUST_CORE_KIND, target, source_sha: sourceSha,
    entrypoint: RUST_CORE_ENTRYPOINT,
    files: [...files].map(([name, bytes]) => ({ name, size: bytes.length, sha256: digest(bytes) })).sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0) });
  const directories = new Set([root]);
  for (const name of files.keys()) {
    const pieces = `${root}/${name}`.split('/');
    for (let index = 1; index < pieces.length; index++) directories.add(pieces.slice(0, index).join('/'));
  }
  for (const item of extraEntries) {
    const name = item.name.replace(/^sidevoice-core-rust\//, `${root}/`);
    const pieces = name.split('/');
    for (let index = 1; index < pieces.length; index++) directories.add(pieces.slice(0, index).join('/'));
  }
  const pack = tar.pack();
  for (const name of directories) pack.entry({ name, type: 'directory', mode: 0o755 });
  for (const [name, bytes] of files) pack.entry({ name: `${root}/${name}`, mode: 0o755 }, bytes);
  for (const item of extraEntries) {
    const name = item.name.replace(/^sidevoice-core-rust\//, `${root}/`);
    pack.entry({ name, ...(item.type ? { type: item.type, linkname: item.linkname } : {}) }, item.bytes || Buffer.alloc(0));
  }
  pack.entry({ name: `${root}/native-core.json`, mode: 0o644 }, Buffer.from(`${canonical(inventory)}\n`));
  pack.finalize();
  const compressor = createZstdCompress();
  pack.pipe(compressor);
  const chunks = [];
  for await (const chunk of compressor) chunks.push(chunk);
  await finished(compressor);
  const bytes = Buffer.concat(chunks);
  const bundles = Object.fromEntries(RUST_CORE_TARGETS.map(name => [name, {
    name: `sidevoice-core-rust-${sourceSha}-${name}.tar.zst`, size: 1, sha256: '0'.repeat(64),
  }]));
  bundles[target] = { name: `sidevoice-core-rust-${sourceSha}-${target}.tar.zst`, size: bytes.length, sha256: digest(bytes) };
  const manifest = { schema: 1, kind: RUST_CORE_KIND, source_sha: sourceSha, cargo_lock_sha256: '1'.repeat(64),
    entrypoint: RUST_CORE_ENTRYPOINT, bundles };
  return { bytes, manifest };
}

test('native Core manifest and inventory JSON require canonical UTF-8 and exact schema', () => {
  assert.deepEqual(parseCanonicalRustCoreJson(Buffer.from('{"a":1,"z":[true]}\n')), { a: 1, z: [true] });
  for (const bytes of ['{"a":1, "z":[true]}\n', '{"a":1,"a":2}\n', '\ufeff{"a":1}\n', '{"a":1}\ntrailing']) {
    assert.throws(() => parseCanonicalRustCoreJson(Buffer.from(bytes)));
  }
  const valid = { schema: 1, kind: RUST_CORE_KIND, source_sha: sourceSha, cargo_lock_sha256: '1'.repeat(64),
    entrypoint: RUST_CORE_ENTRYPOINT, bundles: Object.fromEntries(RUST_CORE_TARGETS.map(name => [name, {
      name: `sidevoice-core-rust-${sourceSha}-${name}.tar.zst`, size: 1, sha256: '0'.repeat(64),
    }])) };
  assert.equal(validateRustCoreManifest(valid, { expectedSourceSha: sourceSha }), valid);
  assert.throws(() => validateRustCoreManifest({ ...valid, unexpected: true }));
  assert.throws(() => validateRustCoreManifest(valid, { expectedSourceSha: '2'.repeat(40) }));
  assert.equal(rustCoreTarget('darwin', 'arm64'), 'macos-aarch64');
  assert.equal(rustCoreTarget('linux', 'x64'), 'linux-x86_64');
  assert.equal(rustCoreTarget('linux', 'arm64'), 'linux-aarch64');
  assert.equal(rustCoreTarget('linux', 'ia32'), null);
});

test('native Core service launch is fixed to its release paths and clears ambient loader overrides', () => {
  const binary = '/private/sidevoice/releases/test/core/bin/sidevoice-core-rust';
  assert.equal(isRustCoreProgram(binary), true);
  assert.equal(rustCoreRootForProgram(binary), '/private/sidevoice/releases/test/core');
  assert.equal(isRustCoreProgram('/private/sidevoice/releases/test/core/bin/sidevoice-core'), false);
  const args = coreArgs({ dataDir: '/private/data', launchId: 'launch-test', idleExit: 0,
    roomCredential: '/private/data/credentials.json', nativeRoot: '/private/sidevoice/releases/test/core' });
  assert.equal(args[0], '--data-dir');
  assert.equal(args[1], path.join('/private/data', 'core'));
  assert.equal(args[args.indexOf('--ready-file') + 1], path.join('/private/data', 'core', 'core.json'));
  assert.equal(args[args.indexOf('--host') + 1], '127.0.0.1');
  assert.equal(args[args.indexOf('--launch-id') + 1], 'launch-test');
  assert.equal(args.at(-1), '0');
  assert.equal(args.includes('-I') || args.includes('-m'), false, 'native Core never receives Python launcher flags');

  const original = { LD_LIBRARY_PATH: '/untrusted/lib', LD_PRELOAD: '/untrusted/preload',
    DYLD_INSERT_LIBRARIES: '/untrusted/dylib', PATH: process.env.PATH, RUSTVANI_CACHE_DIR: '/untrusted/models' };
  const safe = rustCoreEnvironment(original, '/private/sidevoice/releases/test/core');
  for (const key of ['LD_LIBRARY_PATH', 'LD_PRELOAD', 'DYLD_INSERT_LIBRARIES']) assert.equal(key in safe, false);
  assert.equal(safe.RUSTVANI_CACHE_DIR, '/private/sidevoice/releases/test/core/models');
  assert.equal(original.RUSTVANI_CACHE_DIR, '/untrusted/models', 'the caller environment stays unchanged');
});

test('a selected native release remains runnable through rollback when the invoking package embeds no Core', async t => {
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-core-rollback-'));
  const env = { ...process.env, XDG_DATA_HOME: parent, HOME: parent };
  const layout = releaseLayout(env), id = 'previous-native-release';
  const directory = path.join(layout.releases, id), binary = path.join(directory, 'core', RUST_CORE_ENTRYPOINT);
  await mkdir(path.dirname(binary), { recursive: true, mode: 0o700 });
  await writeFile(binary, '#!/bin/sh\nexit 0\n', { mode: 0o700 });
  const archiveSha = 'a'.repeat(64), runtimeSha = 'b'.repeat(64);
  const coreBuild = `${RUST_CORE_KIND}-${rustCoreTarget()}-${sourceSha}-${archiveSha}`;
  const release = { id, connector: '0.6.1', core: CORE_VERSION, core_build: coreBuild,
    channel: 'release', build_seq: 0, format: 'sea', runtime_kind: 'javascript',
    runtime_build_sha: null, runtime_sha256: runtimeSha,
    pair_id: `pair-v1:javascript:${runtimeSha}:core:${coreBuild}`,
    core_kind: RUST_CORE_KIND, core_source_sha: sourceSha, core_cargo_lock_sha256: '1'.repeat(64),
    core_manifest_sha256: '2'.repeat(64), core_target: rustCoreTarget(), core_archive_sha256: archiveSha,
    core_archive_size: 123, core_entrypoint: RUST_CORE_ENTRYPOINT };
  writePrivateFile(path.join(directory, 'release.json'), `${JSON.stringify(release)}\n`);
  point(env, 'current', id);
  t.after(() => rm(parent, { recursive: true, force: true }));
  assert.equal(coreProgram(env), path.join(layout.current, 'core', RUST_CORE_ENTRYPOINT));
});

test('selected Rust Connector identity binds the exact release executable and runtime fields', async t => {
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-connector-identity-'));
  const env = { ...process.env, HOME: parent, XDG_DATA_HOME: parent };
  const layout = releaseLayout(env), id = 'selected-rust-runtime';
  const directory = path.join(layout.releases, id);
  const executable = path.join(directory, 'dist', 'sidevoice-rust');
  const bytes = Buffer.from('selected Rust Connector test executable');
  await mkdir(path.dirname(executable), { recursive: true, mode: 0o700 });
  await writeFile(executable, bytes, { mode: 0o700 });
  const runtimeSha = digest(bytes), connectorSource = 'c'.repeat(40), coreSource = 'b'.repeat(40);
  const archiveSha = 'a'.repeat(64), coreBuild = `${RUST_CORE_KIND}-${rustCoreTarget()}-${coreSource}-${archiveSha}`;
  const release = { id, connector: '0.6.0', format: 'sea', runtime_kind: RUST_CONNECTOR_KIND,
    runtime_build_sha: connectorSource, runtime_sha256: runtimeSha, runtime_target: rustCoreTarget(), runtime_size: bytes.length,
    core_kind: RUST_CORE_KIND, core_target: rustCoreTarget(), core_source_sha: coreSource,
    core_archive_sha256: archiveSha, core_build: coreBuild,
    pair_id: `pair-v1:${RUST_CONNECTOR_KIND}:${runtimeSha}:core:${coreBuild}` };
  writePrivateFile(path.join(directory, 'release.json'), `${JSON.stringify(release)}\n`);
  point(env, 'current', id);
  t.after(() => rm(parent, { recursive: true, force: true }));

  const selectedExecutable = path.join(layout.current, 'dist', 'sidevoice-rust');
  assert.deepEqual(selectedMcpCommand(env), [selectedExecutable, '--installed', 'mcp']);
  const identity = { version: release.connector, runtime_kind: release.runtime_kind,
    runtime_build_sha: release.runtime_build_sha, runtime_sha256: release.runtime_sha256,
    runtime_target: release.runtime_target, release_id: release.id,
    executable: await realpath(executable) };
  assert.equal(matchesSelectedRustConnectorIdentity(identity, release, layout.root), true);
  for (const field of ['version', 'runtime_kind', 'runtime_build_sha', 'runtime_sha256', 'runtime_target', 'release_id', 'executable']) {
    assert.equal(matchesSelectedRustConnectorIdentity({ ...identity, [field]: 'wrong' }, release, layout.root), false, field);
  }
});

test('native self-test supplies fixed fixture/model paths and rejects a bad report or exit', async t => {
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-core-self-test-'));
  const binary = path.join(parent, 'sidevoice-core-rust');
  const nativeRoot = path.join(parent, 'core');
  t.after(() => rm(parent, { recursive: true, force: true }));
  const report = { detectors: { sample_rate: 16_000, frames: 2, max_voice_confidence: 0.8,
    smart_turn_probability: 0.4, smart_turn_complete: true }, opus_decoded_samples: 320 };
  const script = value => `#!/usr/bin/env node\nconst args = process.argv.slice(2);\n` +
    `if (args[0] !== '--self-test' || args[1] !== ${JSON.stringify(path.join(nativeRoot, 'checks', 'detector-16k.wav'))} || args[2] !== ${JSON.stringify(path.join(nativeRoot, 'models'))}) process.exit(8);\n` +
    `if (process.env.RUSTVANI_CACHE_DIR !== args[2] || process.env.LD_LIBRARY_PATH || process.env.DYLD_INSERT_LIBRARIES) process.exit(9);\n` +
    `console.log(${JSON.stringify(JSON.stringify(value))});\n`;
  await writeFile(binary, script(report), { mode: 0o700 });
  await chmod(binary, 0o700);
  const result = await selfTest(binary, { ...process.env, LD_LIBRARY_PATH: '/untrusted/lib',
    DYLD_INSERT_LIBRARIES: '/untrusted/dylib' }, { nativeRoot });
  assert.equal(result.opus_decoded_samples, 320);

  await writeFile(binary, script({ ...report, opus_decoded_samples: 319 }), { mode: 0o700 });
  await assert.rejects(selfTest(binary, process.env, { nativeRoot }), error => error.key === 'install.self-test');
  await writeFile(binary, `${script(report)}process.exit(4);\n`, { mode: 0o700 });
  await assert.rejects(selfTest(binary, process.env, { nativeRoot }), error => error.key === 'install.self-test');
});

test('native Core attestation accepts only the protected-main rust-t7 subject and exact bytes', () => {
  const digest = 'a'.repeat(64), subjectName = 'native-core-manifest.json';
  const result = enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest }), digest, subjectName });
  assert.equal(result.signer, RUST_CORE_SIGNER);
  assert.equal(result.sha256, digest);
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner({ buildSigner: `${CORE_REPOSITORY}/.github/workflows/test.yml@refs/heads/main` }),
    statement: rustStatement({ digest }), digest, subjectName }), 'workflow');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner({ buildConfig: `${CORE_REPOSITORY}/.github/workflows/rust-t7.yml@refs/heads/feature` }),
    statement: rustStatement({ digest }), digest, subjectName }), 'build-config');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest, workflowRef: 'refs/heads/feature' }), digest, subjectName }), 'build-config');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest, workflowPath: '.github/workflows/test.yml' }), digest, subjectName }), 'build-config');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest, predicateType: 'https://example.invalid' }), digest, subjectName }), 'predicate');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest, subjectName: 'other.json' }), digest, subjectName }), 'subject');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner(), statement: rustStatement({ digest: 'b'.repeat(64) }), digest, subjectName }), 'subject');
  expectRefusal(() => enforceRustCoreProvenance({ signer: rustSigner({ runner: 'self-hosted' }), statement: rustStatement({ digest }), digest, subjectName }), 'runner');
});

test('fixed-root archive extraction verifies inventory and strips exactly one root', async t => {
  const { bytes, manifest } = await archiveFor();
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-core-test-'));
  const destination = path.join(parent, 'release', 'core');
  await import('node:fs/promises').then(({ mkdir }) => mkdir(path.dirname(destination)));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const extracted = await unpackRustCoreArchive(bytes, destination, { manifest, target });
  assert.equal(extracted.root, destination);
  assert.equal((await readFile(path.join(destination, RUST_CORE_ENTRYPOINT), 'utf8')), 'native entrypoint');
  assert.equal((await stat(path.join(destination, RUST_CORE_ENTRYPOINT))).mode & 0o111, 0o111);
  const names = await (await import('node:fs/promises')).readdir(destination);
  assert.deepEqual(names.sort(), ['bin', 'checks', 'models', 'notices']);
  const temporaryRelease = path.dirname(destination), finalRelease = path.join(parent, 'renamed-release');
  linkCoreRuntimeIntoRelease(temporaryRelease, { kind: RUST_CORE_KIND, bin: path.join(destination, RUST_CORE_ENTRYPOINT) });
  await rename(temporaryRelease, finalRelease);
  const installedFiles = [];
  async function walk(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      assert.equal(entry.isSymbolicLink(), false, `native release contains a symlink: ${path.join(directory, entry.name)}`);
      const full = path.join(directory, entry.name);
      if (entry.isDirectory()) await walk(full);
      else { assert.equal(entry.isFile(), true); installedFiles.push(path.relative(path.join(finalRelease, 'core'), full)); }
    }
  }
  await walk(path.join(finalRelease, 'core'));
  assert.deepEqual(installedFiles.sort(), Object.keys(required).sort(), 'the renamed native release contains exactly the verified inventory');
});

test('archive digest, fixed root, path, type, duplicate, extra-file and inventory failures leave no stage', async t => {
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-core-negative-'));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const cases = [
    ['wrong fixed root', () => archiveFor({ root: 'wrong-root' })],
    ['traversal', () => archiveFor({ extraEntries: [{ name: 'sidevoice-core-rust/../escape', bytes: Buffer.from('x') }] })],
    ['symbolic link', () => archiveFor({ extraEntries: [{ name: 'sidevoice-core-rust/lib/bad', type: 'symlink', linkname: '../../escape' }] })],
    ['duplicate file', () => archiveFor({ extraEntries: [{ name: 'sidevoice-core-rust/models/silero.onnx', bytes: Buffer.from('duplicate') }] })],
    ['extra file', () => archiveFor({ extraEntries: [{ name: 'sidevoice-core-rust/models/extra.bin', bytes: Buffer.from('extra') }] })],
    ['inventory digest', () => archiveFor({ inventoryEdit: value => ({ ...value, files: value.files.map(record => record.name === 'models/silero.onnx' ? { ...record, sha256: '2'.repeat(64) } : record) }) })],
  ];
  for (let index = 0; index < cases.length; index++) {
    const [label, make] = cases[index];
    const { bytes, manifest } = await make();
    const destination = path.join(parent, `case-${index}`);
    await assert.rejects(unpackRustCoreArchive(bytes, destination, { manifest, target }), undefined, label);
    await assert.rejects(stat(destination), { code: 'ENOENT' }, `${label} left partial staging bytes`);
  }
  const good = await archiveFor();
  const badBytes = Buffer.from(good.bytes); badBytes[0] ^= 1;
  const digestDestination = path.join(parent, 'digest-mismatch');
  await assert.rejects(unpackRustCoreArchive(badBytes, digestDestination, { manifest: good.manifest, target }));
  await assert.rejects(stat(digestDestination), { code: 'ENOENT' });
  const cancel = new AbortController(); cancel.abort();
  const cancelledDestination = path.join(parent, 'cancelled');
  await assert.rejects(unpackRustCoreArchive(good.bytes, cancelledDestination, { manifest: good.manifest, target, signal: cancel.signal }));
  await assert.rejects(stat(cancelledDestination), { code: 'ENOENT' });
});

const productionInputsPresent = !!(process.env.SIDEVOICE_RUST_CORE_MANIFEST
  && process.env.SIDEVOICE_RUST_CORE_MANIFEST_SIGSTORE && process.env.SIDEVOICE_RUST_CORE_ARCHIVE
  && process.env.SIDEVOICE_RUST_CORE_ARCHIVE_SIGSTORE && process.env.SIDEVOICE_RUST_CORE_TARGET);
test('protected-main Core artifact verifies, stages offline and passes its native self-test', {
  skip: !productionInputsPresent,
  timeout: 300_000,
}, async t => {
  const pin = JSON.parse(await readFile(new URL('../rust-core-production-pin.json', import.meta.url), 'utf8'));
  const selectedTarget = process.env.SIDEVOICE_RUST_CORE_TARGET;
  const expected = pin.bundles[selectedTarget];
  const manifestBytes = await readFile(process.env.SIDEVOICE_RUST_CORE_MANIFEST);
  const manifestBundle = await readFile(process.env.SIDEVOICE_RUST_CORE_MANIFEST_SIGSTORE);
  const archiveBytes = await readFile(process.env.SIDEVOICE_RUST_CORE_ARCHIVE);
  const archiveBundle = await readFile(process.env.SIDEVOICE_RUST_CORE_ARCHIVE_SIGSTORE);
  assert.equal(manifestBytes.length, pin.manifest.size);
  assert.equal(sha256(manifestBytes), pin.manifest.sha256);
  const manifest = parseCanonicalRustCoreJson(manifestBytes, 'native Core manifest');
  validateRustCoreManifest(manifest, { expectedSourceSha: pin.source_sha });
  assert.deepEqual(manifest.bundles[selectedTarget], expected);
  await verifyRustCoreArtifact({ bytes: manifestBytes, expectedSha256: pin.manifest.sha256, bundleBytes: manifestBundle,
    subjectName: pin.manifest.name, tufCachePath: process.env.SIDEVOICE_TUF_CACHE, label: pin.manifest.name });
  await verifyRustCoreArtifact({ bytes: archiveBytes, expectedSha256: expected.sha256, bundleBytes: archiveBundle,
    subjectName: expected.name, tufCachePath: process.env.SIDEVOICE_TUF_CACHE, label: expected.name });
  const parent = await mkdtemp(path.join(os.tmpdir(), 'sidevoice-rust-core-production-'));
  const destination = path.join(parent, 'release', 'core');
  await import('node:fs/promises').then(({ mkdir }) => mkdir(path.dirname(destination)));
  t.after(() => rm(parent, { recursive: true, force: true }));
  await unpackRustCoreArchive(archiveBytes, destination, { manifest, target: selectedTarget });
  const binary = path.join(destination, RUST_CORE_ENTRYPOINT);
  const report = spawnSync(binary, ['--self-test', path.join(destination, 'checks', 'detector-16k.wav'), path.join(destination, 'models')], {
    encoding: 'utf8', timeout: 120_000, env: rustCoreEnvironment(process.env, destination),
  });
  assert.equal(report.status, 0, report.stderr || report.error?.message);
  const result = JSON.parse(report.stdout.trim().split('\n').at(-1));
  assert.equal(result.detectors.sample_rate, 16_000);
  assert.ok(result.detectors.frames > 0);
  assert.equal(result.opus_decoded_samples, 320);
});
