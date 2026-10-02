import { createHash } from 'node:crypto';
import { verify } from 'sigstore';
import { keyed } from './i18n.mjs';
import { classifyInstallFailure } from './install-errors.mjs';

export const CORE_REPOSITORY = 'https://github.com/sidevoice/sidevoice-core';
export const CORE_REPOSITORY_ID = '1399406535';
export const CORE_ISSUER = 'https://token.actions.githubusercontent.com';
export const CORE_SIGNER = `${CORE_REPOSITORY}/.github/workflows/test.yml@refs/heads/main`;
export const SLSA_PREDICATE = 'https://slsa.dev/provenance/v1';
const BUILD_CONFIGS = Object.freeze({
  release: `${CORE_REPOSITORY}/.github/workflows/release-please.yml@refs/heads/main`,
  nightly: `${CORE_REPOSITORY}/.github/workflows/test.yml@refs/heads/main`,
});

const OID = Object.freeze({ issuer: '1.3.6.1.4.1.57264.1.1', buildSigner: '1.3.6.1.4.1.57264.1.9',
  runner: '1.3.6.1.4.1.57264.1.11', source: '1.3.6.1.4.1.57264.1.12', repositoryId: '1.3.6.1.4.1.57264.1.15',
  buildConfig: '1.3.6.1.4.1.57264.1.18' });

export function refusal(check) {
  return keyed('install.authenticity', { check }, { check });
}

export function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

/** Decode Fulcio's UTF8String extension values as specified by its OID directory. */
function oidText(signer, oid) {
  const extension = signer?.identity?.oids?.find(item => item.oid?.id?.join('.') === oid);
  if (!extension) return undefined;
  const bytes = Buffer.isBuffer(extension.value) ? extension.value : Buffer.from(extension.value?.data ?? []);
  if (oid === OID.issuer) return bytes.toString('utf8'); // v1 Issuer is a raw string.
  if (bytes.length < 2 || bytes[0] !== 0x0c) return undefined;
  let offset = 1, length = bytes[offset++];
  if (length & 0x80) {
    const count = length & 0x7f;
    if (!count || count > 4 || bytes.length < offset + count) return undefined;
    length = 0;
    for (let i = 0; i < count; i++) length = length * 256 + bytes[offset++];
  }
  if (bytes.length !== offset + length) return undefined;
  return bytes.subarray(offset).toString('utf8');
}

const failIf = (condition, check, detail) => { if (!condition) throw refusal(check, detail); };

/** Enforce the production pins after sigstore-js has cryptographically verified the bundle. */
export function enforceCoreProvenance({ signer, statement, digest, channel, label = 'core asset' }) {
  const expectedBuildConfig = BUILD_CONFIGS[channel];
  failIf(!!expectedBuildConfig, 'build-config', `unsupported channel ${channel}`);
  const identity = signer?.identity;
  failIf(identity?.extensions?.issuer === CORE_ISSUER && oidText(signer, OID.issuer) === CORE_ISSUER,
    'issuer', `${label} certificate issuer`);
  failIf(identity?.subjectAlternativeName === CORE_SIGNER && oidText(signer, OID.buildSigner) === CORE_SIGNER,
    'workflow', `${label} signer workflow`);
  failIf(oidText(signer, OID.source) === CORE_REPOSITORY, 'source', `${label} source repository`);
  failIf(oidText(signer, OID.repositoryId) === CORE_REPOSITORY_ID, 'repository-id', `${label} numeric repository id`);
  failIf(oidText(signer, OID.runner) === 'github-hosted', 'runner', `${label} runner environment`);
  failIf(oidText(signer, OID.buildConfig) === expectedBuildConfig, 'build-config', `${label} certificate build config`);

  failIf(statement?._type === 'https://in-toto.io/Statement/v1', 'predicate', `${label} statement type`);
  failIf(statement?.predicateType === SLSA_PREDICATE, 'predicate', `${label} predicate type`);
  const workflow = statement?.predicate?.buildDefinition?.externalParameters?.workflow;
  const expectedPath = expectedBuildConfig.slice(CORE_REPOSITORY.length + '/.github/workflows/'.length).split('@')[0];
  failIf(workflow?.repository === CORE_REPOSITORY && workflow?.ref === 'refs/heads/main' && workflow?.path === `.github/workflows/${expectedPath}`,
    'build-config', `${label} SLSA workflow`);
  failIf(statement?.predicate?.runDetails?.builder?.id === expectedBuildConfig,
    'build-config', `${label} SLSA builder workflow`);
  failIf(statement?.predicate?.buildDefinition?.internalParameters?.github?.repository_id === CORE_REPOSITORY_ID,
    'repository-id', `${label} SLSA repository id`);
  failIf(statement?.predicate?.buildDefinition?.internalParameters?.github?.runner_environment === 'github-hosted',
    'runner', `${label} SLSA runner environment`);

  const subjects = statement?.subject;
  failIf(Array.isArray(subjects) && subjects.length === 1, 'subject', `${label} must have one subject`);
  const subjectDigest = subjects[0]?.digest;
  failIf(subjectDigest && Object.keys(subjectDigest).length === 1 && /^[0-9a-f]{64}$/.test(subjectDigest.sha256),
    'subject', `${label} subject must contain only SHA-256`);
  failIf(subjectDigest.sha256 === digest, 'subject', `${label} SHA-256 subject does not match the downloaded bytes`);
  return { issuer: CORE_ISSUER, signer: CORE_SIGNER, source: CORE_REPOSITORY, repositoryId: CORE_REPOSITORY_ID,
    runner: 'github-hosted', buildConfig: expectedBuildConfig, predicateType: SLSA_PREDICATE, sha256: digest };
}

/** Validate bytes, Sigstore public-good trust, transparency evidence and the pinned SLSA identity. */
export async function verifyCoreArtifact({ bytes, expectedSha256 = null, bundleBytes, channel, tufCachePath,
  tufForceCache = false, label = 'core asset' }) {
  const digest = sha256(bytes);
  if (expectedSha256 !== null) {
    if (!/^[0-9a-f]{64}$/.test(expectedSha256)) throw refusal('sha256', `${label} manifest digest is not lowercase SHA-256`);
    if (digest !== expectedSha256) throw refusal('sha256', `${label} bytes differ from the embedded manifest`);
  }
  let bundle;
  try { bundle = JSON.parse(Buffer.isBuffer(bundleBytes) ? bundleBytes.toString('utf8') : String(bundleBytes)); }
  catch { throw refusal('sigstore-bundle', `${label} Sigstore sidecar is not JSON`); }
  if (!bundle?.dsseEnvelope || !Array.isArray(bundle.dsseEnvelope.signatures)) {
    throw refusal('sigstore-bundle', `${label} Sigstore sidecar is not a DSSE bundle`);
  }
  let signer;
  try { signer = await verify(bundle, { tufCachePath, tufForceCache }); }
  catch (error) {
    // Prose about a refused signing certificate is authenticity evidence, not proxy detection. During TUF access,
    // classify proxy interception from transport error codes; generic certificate wording remains a Sigstore refusal.
    const category = classifyInstallFailure(error, null, { proxyText: false });
    if (category === 'network' || category === 'proxy' || category === 'disk') throw keyed(`install.${category}`);
    throw refusal('sigstore', `${label} Sigstore verification failed (${error?.name ?? 'Error'}: ${error?.message ?? error})`);
  }

  let payload;
  try {
    const encoded = bundle.dsseEnvelope.payload;
    if (typeof encoded !== 'string' || !encoded || Buffer.from(encoded, 'base64').toString('base64') !== encoded) throw new Error('non-canonical payload');
    payload = JSON.parse(Buffer.from(encoded, 'base64').toString('utf8'));
  } catch { throw refusal('predicate', `${label} verified payload is not a valid in-toto statement`); }
  return enforceCoreProvenance({ signer, statement: payload, digest, channel, label });
}

export function expectedBuildConfig(channel) { return BUILD_CONFIGS[channel] ?? null; }
