/** Closed identity for the Rust Connector executable embedded in a target SEA. */
import path from 'node:path';
import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, realpathSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { BUILD_PACKAGE, RUST_CONNECTOR_BINARY_SHA256, RUST_CONNECTOR_BINARY_SIZE,
  RUST_CONNECTOR_SOURCE_SHA, RUST_CONNECTOR_TARGET } from './build-info.mjs';
import { runningAsSea, getSeaAsset } from './sea-runtime.mjs';
import { RUST_CORE_TARGETS, rustCoreTarget } from './rust-core.mjs';
import { keyed } from './i18n.mjs';

export const RUST_CONNECTOR_KIND = 'rust-native-v1';
export const RUST_CONNECTOR_ENTRYPOINT = 'dist/sidevoice-rust';

const digest = value => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const sourceSha = value => typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);

/** The package carries a Rust Connector only when a trusted target SEA build embedded the matching Core. */
export function embeddedRustConnectorIdentity() {
  if (!runningAsSea()) return null;
  const fields = [RUST_CONNECTOR_TARGET, RUST_CONNECTOR_SOURCE_SHA,
    RUST_CONNECTOR_BINARY_SHA256, RUST_CONNECTOR_BINARY_SIZE];
  if (fields.every(value => value === null)) return null;
  if (!RUST_CORE_TARGETS.includes(RUST_CONNECTOR_TARGET) || RUST_CONNECTOR_TARGET !== rustCoreTarget()
      || !sourceSha(RUST_CONNECTOR_SOURCE_SHA) || !digest(RUST_CONNECTOR_BINARY_SHA256)
      || !Number.isSafeInteger(RUST_CONNECTOR_BINARY_SIZE) || RUST_CONNECTOR_BINARY_SIZE < 1
      || RUST_CONNECTOR_BINARY_SIZE > 100_000_000) {
    throw keyed('install.authenticity', { check: 'manifest' });
  }
  return { kind: RUST_CONNECTOR_KIND, target: RUST_CONNECTOR_TARGET,
    sourceSha: RUST_CONNECTOR_SOURCE_SHA, version: BUILD_PACKAGE.version,
    binarySha256: RUST_CONNECTOR_BINARY_SHA256, binarySize: RUST_CONNECTOR_BINARY_SIZE };
}

/** Copy the named SEA asset only after its closed target, size and digest match the build metadata. */
export function verifiedEmbeddedRustConnector() {
  const identity = embeddedRustConnectorIdentity();
  if (!identity) return null;
  const bytes = getSeaAsset('sidevoice-rust-connector');
  if (!bytes || bytes.length !== identity.binarySize
      || createHash('sha256').update(bytes).digest('hex') !== identity.binarySha256) {
    throw keyed('install.authenticity', { check: 'manifest' });
  }
  return { identity, bytes };
}

/** Check the staged executable after its bytes were verified and before selection. */
export function verifyStagedRustConnector(executable, identity) {
  let info;
  try { info = lstatSync(executable); } catch { throw keyed('install.self-test', { detail: 'the staged Rust Connector is missing' }); }
  if (!info.isFile() || info.isSymbolicLink() || !(info.mode & 0o111)
      || info.size !== identity.binarySize || info.size > 100_000_000) {
    throw keyed('install.self-test', { detail: 'the staged Rust Connector is not the selected executable' });
  }
  const digest = createHash('sha256').update(readFileSync(executable)).digest('hex');
  if (digest !== identity.binarySha256) throw keyed('install.self-test', { detail: 'the staged Rust Connector digest differs from the selected release' });
  const result = spawnSync(executable, ['runtime-identity', '--json'], { encoding: 'utf8', timeout: 10_000 });
  if (result.error || result.status !== 0 || result.stdout.length > 4096) {
    throw keyed('install.self-test', { detail: result.error?.message || result.stderr?.trim() || 'the staged Rust Connector identity check failed' });
  }
  let observed;
  try { observed = JSON.parse(result.stdout.trim()); } catch { observed = null; }
  if (observed?.kind !== identity.kind || observed?.target !== identity.target
      || observed?.source_sha !== identity.sourceSha || observed?.version !== identity.version) {
    throw keyed('install.self-test', { detail: 'the staged Rust Connector reports a different target or source identity' });
  }
  return digest;
}

export function selectedRustConnectorProgram(releaseRoot, release) {
  if (release?.runtime_kind !== RUST_CONNECTOR_KIND) return null;
  const target = rustCoreTarget();
  const valid = RUST_CORE_TARGETS.includes(target) && release.runtime_target === target
    && sourceSha(release.runtime_build_sha) && digest(release.runtime_sha256)
    && Number.isSafeInteger(release.runtime_size) && release.runtime_size > 0
    && release.format === 'sea' && release.core_kind === 'rust-native-v1'
    && release.core_target === target && sourceSha(release.core_source_sha)
    && /^[0-9a-f]{64}$/.test(release.core_archive_sha256 || '')
    && release.core_build === `rust-native-v1-${target}-${release.core_source_sha}-${release.core_archive_sha256}`
    && release.pair_id === `pair-v1:${RUST_CONNECTOR_KIND}:${release.runtime_sha256}:core:${release.core_build}`;
  const executable = path.join(releaseRoot, 'current', RUST_CONNECTOR_ENTRYPOINT);
  let info;
  try { info = lstatSync(executable); } catch {}
  if (!valid || !info?.isFile() || info.isSymbolicLink() || !(info.mode & 0o111)
      || info.size !== release.runtime_size || createHash('sha256').update(readFileSync(executable)).digest('hex') !== release.runtime_sha256) {
    throw keyed('install.not-selected', { detail: 'the selected Rust Connector release identity or executable is invalid' });
  }
  return executable;
}

/** Verify the process answering the private connector socket is this selected Rust release. */
export function matchesSelectedRustConnectorIdentity(observed, release, releaseRoot) {
  if (!observed || release?.runtime_kind !== RUST_CONNECTOR_KIND) return false;
  let executable;
  try { executable = realpathSync(selectedRustConnectorProgram(releaseRoot, release)); }
  catch { return false; }
  return observed.version === release.connector
    && observed.runtime_kind === release.runtime_kind
    && observed.runtime_build_sha === release.runtime_build_sha
    && observed.runtime_sha256 === release.runtime_sha256
    && observed.runtime_target === release.runtime_target
    && observed.release_id === release.id
    && observed.executable === executable;
}
