import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { lstat, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { validateCoreManifest } from './core-bundle.mjs';

export const SEA_ARTIFACT_NAME = 'sidevoice-connector-macos-aarch64-r4b';
export const PIN_ARTIFACT_NAME = 'sidevoice-connector-macos-aarch64-r4b-pin';
export const CORE_INPUTS_ARTIFACT_NAME = 'sidevoice-connector-macos-aarch64-r4b-core-inputs';
export const PROVENANCE_ARTIFACT_NAME = 'sidevoice-connector-macos-aarch64-r4b-provenance';
export const PIN_METADATA_PROTOCOL = 'sidevoice-metadata-v1';
export const PIN_PROGRESS_PROTOCOL = 'sidevoice-progress-jsonl-v1';

const GIT_SHA = /^[0-9a-f]{40}$/;
const SHA256 = /^[0-9a-f]{64}$/;
const EXPECTED_BUNDLES = [['macos', 'aarch64'], ['linux', 'x86_64'], ['linux', 'aarch64']];

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
function requireValue(condition, message) {
  if (!condition) throw new Error(`Cannot create the R4-b desktop pin: ${message}`);
}
function positiveInteger(value) { return Number.isSafeInteger(value) && value > 0; }

function runArtifact(listing, { name, repository, repositoryId, runId, connectorSha }) {
  const records = listing?.artifacts;
  requireValue(Array.isArray(records) && listing.total_count === records.length,
    'the GitHub Actions run artifact listing is invalid or incomplete');
  const matches = records.filter(record => record?.name === name);
  requireValue(matches.length === 1, `the run must contain exactly one ${name} artifact`);
  const artifact = matches[0];
  requireValue(positiveInteger(artifact.id), `${name} has no valid artifact ID`);
  requireValue(artifact.expired === false, `${name} is expired`);
  requireValue(positiveInteger(artifact.size_in_bytes), `${name} has no valid archive size`);
  requireValue(/^sha256:[0-9a-f]{64}$/i.test(artifact.digest || ''), `${name} has no SHA-256 archive digest`);
  const run = artifact.workflow_run;
  requireValue(run && Number(run.id) === runId && Number(run.repository_id) === Number(repositoryId)
    && Number(run.head_repository_id) === Number(repositoryId) && run.head_branch === 'main' && run.head_sha === connectorSha,
  `${name} does not belong to this repository, workflow run, and connector commit`);
  const archiveUrl = `https://api.github.com/repos/${repository}/actions/artifacts/${artifact.id}/zip`;
  requireValue(artifact.archive_download_url === archiveUrl, `${name} does not use GitHub's canonical artifact ZIP route`);
  return { artifact, archiveUrl, sha256: artifact.digest.slice('sha256:'.length).toLowerCase(), size: artifact.size_in_bytes };
}

export function verifyProductionSea({ executableBytes, version, metadata, coreManifestBytes,
  coreManifestSidecarBytes, expectedConnectorSha, expectedBuildSeq }) {
  requireValue(Buffer.isBuffer(executableBytes) && executableBytes.length > 0, 'the SEA executable is missing');
  requireValue(Buffer.isBuffer(coreManifestBytes) && coreManifestBytes.length > 0, 'the signed core manifest is missing');
  requireValue(Buffer.isBuffer(coreManifestSidecarBytes) && coreManifestSidecarBytes.length > 0,
    'the core manifest Sigstore sidecar is missing');

  let manifest, sidecar;
  try { manifest = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(coreManifestBytes)); }
  catch { throw new Error('Cannot create the R4-b desktop pin: the core manifest is not UTF-8 JSON'); }
  try { sidecar = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(coreManifestSidecarBytes)); }
  catch { throw new Error('Cannot create the R4-b desktop pin: the core manifest sidecar is not UTF-8 JSON'); }
  requireValue(sidecar && typeof sidecar === 'object' && !Array.isArray(sidecar), 'the core manifest sidecar is not a JSON object');

  requireValue(version?.ok === true && version.format === 'sea' && version.sea === true,
    'the built connector is not a native SEA');
  requireValue(version.target === 'macos-aarch64', 'the artifact target is not macOS arm64');
  requireValue(version.channel === 'nightly', 'the production R4-b workflow must use the signed nightly core assets');
  requireValue(GIT_SHA.test(version.connector_sha || '') && version.connector_sha === expectedConnectorSha,
    'the embedded connector SHA does not match this workflow commit');
  requireValue(positiveInteger(version.build_seq) && version.build_seq === expectedBuildSeq,
    'the embedded build sequence does not match this workflow run');

  let manifestSha256;
  try { manifestSha256 = sha256(coreManifestBytes); }
  catch { throw new Error('Cannot create the R4-b desktop pin: could not hash the core manifest'); }
  validateCoreManifest(manifest, metadata?.embedded_core?.version, version.channel);
  requireValue(manifest.bundles.length === EXPECTED_BUNDLES.length && manifest.bundles.every((item, index) =>
    item.os === EXPECTED_BUNDLES[index][0] && item.arch === EXPECTED_BUNDLES[index][1]),
  'the signed core manifest bundle list does not match the desktop pin contract');

  requireValue(metadata?.ok === true && metadata.connector?.format === 'sea' && metadata.connector?.sea === true,
    'connector metadata does not identify a native SEA');
  for (const [actual, expected, label] of [
    [metadata.connector.version, version.version, 'connector version'],
    [metadata.connector.sha, version.connector_sha, 'connector SHA'],
    [metadata.connector.channel, version.channel, 'channel'],
    [metadata.connector.build_seq, version.build_seq, 'build sequence'],
    [metadata.connector.target, version.target, 'target'],
  ]) requireValue(actual === expected, `metadata ${label} does not match --version --json`);
  requireValue(metadata.embedded_core?.manifest_sha256 === manifestSha256,
    'embedded manifest SHA does not match the signed manifest bytes');
  const expectedAssets = manifest.bundles.map(item => ({
    name: path.posix.basename(new URL(item.url).pathname), url: item.url, sha256: item.sha256, size: item.size,
  }));
  requireValue(Array.isArray(metadata.embedded_core.assets) && metadata.embedded_core.assets.length === expectedAssets.length
    && metadata.embedded_core.assets.every((asset, index) => {
      const expected = expectedAssets[index];
      return asset?.name === expected.name && asset?.url === expected.url
        && asset?.sha256 === expected.sha256 && asset?.size === expected.size;
    }), 'embedded metadata assets do not match the signed manifest');
  requireValue(metadata.protocols?.metadata === PIN_METADATA_PROTOCOL
    && metadata.protocols?.progress === PIN_PROGRESS_PROTOCOL, 'metadata/progress protocol names do not match Desktop');
  requireValue(positiveInteger(metadata.embedded_core.api) && positiveInteger(metadata.embedded_core.link)
    && Number.isSafeInteger(metadata.connector.link_min) && Number.isSafeInteger(metadata.connector.link_max)
    && metadata.connector.link_min > 0 && metadata.connector.link_min <= metadata.embedded_core.link
    && metadata.embedded_core.link <= metadata.connector.link_max,
  'core API/link identity or connector link range is invalid');

  return { manifest, manifestSha256, expectedAssets };
}

export function createDesktopPinRecord({ executableBytes, version, metadata, coreManifestBytes,
  coreManifestSidecarBytes, expectedConnectorSha, expectedBuildSeq, repository, repositoryId,
  workflow, runId, artifactRecords }) {
  const verified = verifyProductionSea({ executableBytes, version, metadata, coreManifestBytes,
    coreManifestSidecarBytes, expectedConnectorSha, expectedBuildSeq });
  requireValue(repository === 'sidevoice/sidevoice-connector', 'the workflow repository is not the connector repository');
  requireValue(/^\d+$/.test(repositoryId || '') && positiveInteger(Number(repositoryId)), 'the GitHub repository ID is not numeric');
  requireValue(workflow === '.github/workflows/r4-sea.yml@refs/heads/main',
    'the pin was not produced by the protected main R4-b workflow');
  requireValue(positiveInteger(runId), 'the workflow run ID is invalid');
  requireValue(metadata.connector.link_min <= metadata.embedded_core.link
    && metadata.embedded_core.link <= metadata.connector.link_max, 'the core link is outside the connector range');

  const coreVersion = metadata.embedded_core.version;
  const coreTag = version.channel === 'release' ? `v${coreVersion}` : 'nightly';
  const manifestUrl = `https://github.com/sidevoice/sidevoice-core/releases/download/${coreTag}/core-manifest.json`;
  const sidecarUrl = `${manifestUrl}.sigstore.json`;
  const executableArtifact = runArtifact(artifactRecords, { name: SEA_ARTIFACT_NAME, repository,
    repositoryId, runId, connectorSha: expectedConnectorSha });
  const provenanceArtifact = runArtifact(artifactRecords, { name: PROVENANCE_ARTIFACT_NAME, repository,
    repositoryId, runId, connectorSha: expectedConnectorSha });
  requireValue(executableArtifact.artifact.id !== provenanceArtifact.artifact.id,
    'the executable and provenance sidecar must be separate artifacts');
  const assetUrl = executableArtifact.archiveUrl;
  return {
    schema: 1,
    status: 'ready',
    target: version.target,
    connector_sha: version.connector_sha,
    connector_version: version.version,
    channel: version.channel,
    build_seq: version.build_seq,
    core_version: coreVersion,
    core_manifest_sha256: verified.manifestSha256,
    core_manifest_size: coreManifestBytes.length,
    core_manifest_bytes_base64: coreManifestBytes.toString('base64'),
    core_assets: verified.expectedAssets,
    core_manifest_sidecars: [{ name: 'core-manifest.json.sigstore.json', url: sidecarUrl,
      sha256: sha256(coreManifestSidecarBytes), size: coreManifestSidecarBytes.length }],
    executable_sha256: sha256(executableBytes),
    executable_size: executableBytes.length,
    asset_url: assetUrl,
    metadata_protocol: metadata.protocols.metadata,
    progress_protocol: metadata.protocols.progress,
    core_api: metadata.embedded_core.api,
    core_link: metadata.embedded_core.link,
    link_min: metadata.connector.link_min,
    link_max: metadata.connector.link_max,
    provenance: { repository, repository_id: repositoryId, workflow, run_id: runId,
      artifact_name: SEA_ARTIFACT_NAME, sidecars: [{ name: 'sidevoice-provenance.zip',
        url: provenanceArtifact.archiveUrl, sha256: provenanceArtifact.sha256, size: provenanceArtifact.size }] },
  };
}

export async function verifyArtifactRoundTrip(sourceExecutable, artifactDirectory) {
  const entries = await readdir(artifactDirectory);
  requireValue(entries.length === 1 && entries[0] === 'sidevoice', 'the uploaded artifact must contain only root sidevoice');
  const sourceInfo = await lstat(sourceExecutable);
  const downloadedPath = path.join(artifactDirectory, 'sidevoice');
  const downloadedInfo = await lstat(downloadedPath);
  requireValue(sourceInfo.isFile() && !sourceInfo.isSymbolicLink(), 'the built SEA is not a regular file');
  requireValue(downloadedInfo.isFile() && !downloadedInfo.isSymbolicLink(), 'the downloaded artifact entry is not a regular file');
  const [sourceBytes, downloadedBytes] = await Promise.all([readFile(sourceExecutable), readFile(downloadedPath)]);
  requireValue(sourceBytes.length === downloadedBytes.length && sha256(sourceBytes) === sha256(downloadedBytes),
    'the artifact download bytes differ from the tested SEA executable');
  return { executable_size: sourceBytes.length, executable_sha256: sha256(sourceBytes) };
}

export async function verifyArtifactEntries(artifactDirectory, expectedNames) {
  requireValue(Array.isArray(expectedNames) && expectedNames.length > 0
    && expectedNames.every(name => typeof name === 'string' && /^[A-Za-z0-9._-]+$/.test(name)),
  'the artifact entry names are invalid');
  const entries = await readdir(artifactDirectory);
  requireValue(entries.length === expectedNames.length && expectedNames.every(name => entries.includes(name)),
    `the artifact must contain exactly the root entries ${expectedNames.join(', ')}`);
  for (const name of expectedNames) {
    const entry = await lstat(path.join(artifactDirectory, name));
    requireValue(entry.isFile() && !entry.isSymbolicLink(), `the artifact entry ${name} is not a regular file`);
  }
}

async function productionInputs() {
  const executable = path.resolve(process.env.SIDEVOICE_SEA_EXECUTABLE || '');
  const manifestPath = path.resolve(process.env.SIDEVOICE_CORE_MANIFEST || '');
  const sidecarPath = path.resolve(process.env.SIDEVOICE_CORE_MANIFEST_SIGSTORE || '');
  requireValue(executable !== path.resolve(''), 'SIDEVOICE_SEA_EXECUTABLE is required');
  requireValue(manifestPath !== path.resolve('') && sidecarPath !== path.resolve(''), 'manifest and Sigstore sidecar paths are required');
  const executableBytes = await readFile(executable);
  const coreManifestBytes = await readFile(manifestPath);
  const coreManifestSidecarBytes = await readFile(sidecarPath);
  const run = args => JSON.parse(execFileSync(executable, args, { encoding: 'utf8', timeout: 20_000 }).trim());
  const version = run(['--version', '--json']);
  const metadata = run(['metadata', '--json']);
  const expectedBuildSeq = Number(process.env.GITHUB_RUN_NUMBER);
  const verified = verifyProductionSea({ executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedConnectorSha: process.env.GITHUB_SHA, expectedBuildSeq });
  return { executable, executableBytes, version, metadata, coreManifestBytes, coreManifestSidecarBytes,
    expectedBuildSeq, verified };
}

async function cli() {
  const [mode, arg1, arg2] = process.argv.slice(2);
  if (mode === 'verify-build') {
    const input = await productionInputs();
    process.stdout.write(`${JSON.stringify({ connector_sha: input.version.connector_sha, executable_size: input.executableBytes.length,
      executable_sha256: sha256(input.executableBytes), core_manifest_size: input.coreManifestBytes.length,
      core_manifest_sha256: input.verified.manifestSha256 })}\n`);
    return;
  }
  if (mode === 'write-pin') {
    const input = await productionInputs();
    const workflowRef = process.env.GITHUB_WORKFLOW_REF || '';
    const repository = process.env.GITHUB_REPOSITORY || '';
    const repositoryPrefix = `${repository}/`;
    requireValue(workflowRef.startsWith(repositoryPrefix), 'GITHUB_WORKFLOW_REF does not name this repository');
    const artifactsPath = process.env.SIDEVOICE_ARTIFACTS_JSON;
    requireValue(typeof artifactsPath === 'string' && artifactsPath.length > 0, 'SIDEVOICE_ARTIFACTS_JSON is required');
    const record = createDesktopPinRecord({
      executableBytes: input.executableBytes, version: input.version, metadata: input.metadata,
      coreManifestBytes: input.coreManifestBytes, coreManifestSidecarBytes: input.coreManifestSidecarBytes,
      expectedConnectorSha: process.env.GITHUB_SHA, expectedBuildSeq: input.expectedBuildSeq,
      repository, repositoryId: process.env.GITHUB_REPOSITORY_ID,
      workflow: workflowRef.slice(repositoryPrefix.length), runId: Number(process.env.GITHUB_RUN_ID),
      artifactRecords: JSON.parse(await readFile(artifactsPath, 'utf8')),
    });
    const outputPath = path.resolve(process.env.SIDEVOICE_PIN_OUTPUT || '');
    requireValue(outputPath !== path.resolve(''), 'SIDEVOICE_PIN_OUTPUT is required');
    await writeFile(outputPath, `${JSON.stringify(record, null, 2)}\n`, { mode: 0o600 });
    process.stdout.write(`${JSON.stringify({ output: outputPath, asset_url: record.asset_url,
      executable_sha256: record.executable_sha256, executable_size: record.executable_size })}\n`);
    return;
  }
  if (mode === 'verify-copy') {
    const result = await verifyArtifactRoundTrip(arg1, arg2);
    process.stdout.write(`${JSON.stringify(result)}\n`);
    return;
  }
  if (mode === 'verify-entries') {
    const entries = process.argv.slice(4);
    await verifyArtifactEntries(arg1, entries.length ? entries : [arg2]);
    process.stdout.write(`${JSON.stringify({ ok: true, entries: entries.length ? entries : [arg2] })}\n`);
    return;
  }
  throw new Error('usage: node sea-artifact.mjs verify-build | write-pin | verify-copy <built-sidevoice> <download-directory> | verify-entries <directory> <root-entry>...');
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  try { await cli(); }
  catch (error) { process.stderr.write(`${error.message}\n`); process.exitCode = 1; }
}
