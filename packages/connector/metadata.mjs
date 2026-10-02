import { BUILD_PACKAGE, CORE_MANIFEST, CORE_MANIFEST_SHA256 } from './build-info.mjs';
import { API_RANGE, CORE_VERSION, LINK_RANGE } from './core.mjs';
import { coreTarget, validateCoreManifest } from './core-bundle.mjs';
import { runningAsSea } from './sea-runtime.mjs';

export const METADATA_PROTOCOL = 'sidevoice-metadata-v1';
export const PROGRESS_PROTOCOL = 'sidevoice-progress-jsonl-v1';

const targetName = target => target
  ? `${target.os === 'macos' ? 'macos' : target.os}-${target.arch}`
  : `${process.platform}-${process.arch}`;

function embeddedAssets(channel, manifest) {
  if (!manifest) return [];
  validateCoreManifest(manifest, CORE_VERSION, channel);
  return manifest.bundles
    .map(({ url, sha256, size }) => ({ name: new URL(url).pathname.split('/').at(-1), url, sha256, size }));
}

export function versionMetadata({ buildPackage = BUILD_PACKAGE, target = coreTarget(), sea = runningAsSea() } = {}) {
  const sidevoice = buildPackage.sidevoice || {};
  return {
    ok: true,
    version: buildPackage.version,
    target: targetName(target),
    channel: sidevoice.channel || 'release',
    connector_sha: sidevoice.connector_sha || null,
    build_seq: Number.isSafeInteger(sidevoice.build_seq) ? sidevoice.build_seq : 0,
    format: sea ? 'sea' : 'esm',
    sea,
  };
}

export function connectorMetadata({ buildPackage = BUILD_PACKAGE, manifest = CORE_MANIFEST,
  manifestSha256 = CORE_MANIFEST_SHA256, target = coreTarget(), sea = runningAsSea() } = {}) {
  const version = versionMetadata({ buildPackage, target, sea });
  return {
    ok: true,
    connector: {
      version: version.version,
      sha: version.connector_sha,
      channel: version.channel,
      build_seq: version.build_seq,
      target: version.target,
      format: version.format,
      sea: version.sea,
      link_min: LINK_RANGE[0],
      link_max: LINK_RANGE[1],
    },
    embedded_core: {
      version: CORE_VERSION,
      manifest_sha256: manifest ? manifestSha256 : null,
      // Desktop validates metadata against the complete signed producer manifest, regardless of host target.
      assets: embeddedAssets(version.channel, manifest),
      api: API_RANGE[0],
      link: LINK_RANGE[0],
    },
    protocols: { metadata: METADATA_PROTOCOL, progress: PROGRESS_PROTOCOL },
  };
}
