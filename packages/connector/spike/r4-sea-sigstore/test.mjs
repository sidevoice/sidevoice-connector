import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test, { after, before } from 'node:test';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const fixtures = path.join(root, 'fixtures');
const bundlePath = path.join(fixtures, 'sigstore-5.0.0.sigstore.json');
const artifactPath = path.join(fixtures, 'sigstore-5.0.0.tgz');
const executablePath = path.join(root, 'dist/sigstore-sea');
let scratch;
let warmCache;
let warmResult;

before(async () => {
  scratch = await mkdtemp(path.join(os.tmpdir(), 'r4-sea-sigstore-'));
  warmCache = path.join(scratch, 'tuf-cache-warm');
  execFileSync(process.execPath, ['build.mjs'], { cwd: root, stdio: 'inherit' });
});

after(async () => {
  if (scratch) await rm(scratch, { recursive: true, force: true });
});

test('fixture is a genuine package artifact whose digest is named by its SLSA statement', async () => {
  const bundle = JSON.parse(await readFile(bundlePath, 'utf8'));
  const statement = JSON.parse(Buffer.from(bundle.dsseEnvelope.payload, 'base64').toString('utf8'));
  const subject = statement.subject.find((entry) => entry.name === 'pkg:npm/sigstore@5.0.0');
  const digest = createHash('sha512').update(await readFile(artifactPath)).digest('hex');

  assert.equal(bundle.mediaType, 'application/vnd.dev.sigstore.bundle.v0.3+json');
  assert.equal(statement.predicateType, 'https://slsa.dev/provenance/v1');
  assert.ok(subject, 'the statement must name the fixture package');
  assert.equal(subject.digest.sha512, digest);
});

test('the actual Node 22 SEA initializes public-good TUF trust and verifies the genuine bundle', async () => {
  warmResult = runSea(bundlePath, artifactPath, warmCache, 'warm');

  assert.equal(warmResult.status, 0, JSON.stringify(warmResult.output));
  assert.equal(warmResult.json.ok, true);
  assert.equal(warmResult.json.sea, true);
  assert.equal(warmResult.json.node, process.version);
  assert.equal(warmResult.json.sha512, '849a897e81bf7b8a8541a6ae40bd3473a27a16b1cb0462ad905bd6dc96d34881053a12eb4a375233f81677fc9439f628fe0b0445e8535c60b5559597fbd5f80e');
  assert.deepEqual(warmResult.json.signerIdentity, {
    issuer: 'https://token.actions.githubusercontent.com',
    subjectAlternativeName: 'https://github.com/sigstore/sigstore-js/.github/workflows/release.yml@refs/heads/main',
  });
  assert.ok((await readdir(warmCache)).length > 0, 'verification must materialize the TUF cache');
});

test('cold offline TUF cache fails closed while refreshing metadata', () => {
  const result = runSea(bundlePath, artifactPath, path.join(scratch, 'tuf-cache-cold'), 'offline');
  assert.notEqual(result.status, 0);
  assert.equal(result.json.ok, false);
  assert.equal(result.json.sea, true);
  assert.equal(result.json.error.name, 'TUFError');
  assert.equal(result.json.error.message, 'error refreshing TUF metadata');
});

test('warm TUF cache verifies after network access is disabled inside the SEA process', async () => {
  assert.ok(warmResult, 'the online trust-root initialization test must run first');
  const result = runSea(bundlePath, artifactPath, warmCache, 'offline');
  assert.equal(result.status, 0, JSON.stringify(result.output));
  assert.equal(result.json.ok, true);
  assert.equal(result.json.sea, true);
  assert.equal(result.json.networkDisabled, true);
});

test('SEA verifier rejects a signature modified after bundle publication', async () => {
  const bundle = JSON.parse(await readFile(bundlePath, 'utf8'));
  const signature = bundle.dsseEnvelope.signatures[0].sig;
  bundle.dsseEnvelope.signatures[0].sig = (signature[0] === 'A' ? 'B' : 'A') + signature.slice(1);
  const modifiedPath = path.join(scratch, 'tampered.sigstore.json');
  await writeFile(modifiedPath, JSON.stringify(bundle));

  const result = runSea(modifiedPath, artifactPath, warmCache, 'offline');
  assert.notEqual(result.status, 0);
  assert.equal(result.json.ok, false);
  assert.equal(result.json.sea, true);
  assert.equal(result.json.error.name, 'VerificationError');
  assert.equal(result.json.error.message, 'tlog entry signature mismatch');
});

function runSea(bundle, artifact, cache, mode) {
  const child = spawnSync(executablePath, [bundle, artifact, cache, mode], {
    cwd: root,
    encoding: 'utf8',
    timeout: 120_000,
  });
  const output = (child.stdout ?? '').trim();
  let json;
  try {
    json = JSON.parse(output);
  } catch {
    json = undefined;
  }
  return { status: child.status, signal: child.signal, stderr: child.stderr, output, json };
}
