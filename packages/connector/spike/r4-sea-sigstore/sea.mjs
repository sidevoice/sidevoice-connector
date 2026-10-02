import { createHash } from 'node:crypto';
import http from 'node:http';
import https from 'node:https';
import { readFile } from 'node:fs/promises';
import net from 'node:net';
import { isSea } from 'node:sea';
import tls from 'node:tls';
import { verify } from 'sigstore';

async function main() {
  const [bundlePath, artifactPath, cachePath, mode = 'warm'] = process.argv.slice(2);
  if (!bundlePath || !artifactPath || !cachePath) {
    process.stdout.write(JSON.stringify({ ok: false, error: { name: 'UsageError' } }) + '\n');
    process.exitCode = 2;
    return;
  }

  try {
    if (mode === 'offline') disableNetwork();
    const bundle = JSON.parse(await readFile(bundlePath, 'utf8'));
    const options = { tufCachePath: cachePath };
    if (mode === 'offline') options.tufForceCache = true;

    const signer = await verify(bundle, options);
    const statement = JSON.parse(Buffer.from(bundle.dsseEnvelope.payload, 'base64').toString('utf8'));
    const subject = statement.subject?.find((entry) => entry.name === 'pkg:npm/sigstore@5.0.0');
    if (!subject) throw new Error('Expected npm provenance subject is absent');

    const artifactDigest = createHash('sha512').update(await readFile(artifactPath)).digest('hex');
    if (artifactDigest !== subject.digest.sha512) throw new Error('Artifact SHA-512 does not match the attested subject');

    process.stdout.write(JSON.stringify({
      ok: true,
      sea: isSea(),
      node: process.version,
      bundleMediaType: bundle.mediaType,
      predicateType: statement.predicateType,
      subject: subject.name,
      sha512: artifactDigest,
      signerIdentity: signer.identity && {
        issuer: signer.identity.extensions?.issuer,
        subjectAlternativeName: signer.identity.subjectAlternativeName,
      },
      networkDisabled: mode === 'offline',
    }) + '\n');
  } catch (error) {
    process.stdout.write(JSON.stringify({
      ok: false,
      sea: isSea(),
      node: process.version,
      error: { name: error?.name ?? 'Error', message: error?.message ?? String(error) },
    }) + '\n');
    process.exitCode = 1;
  }
}

function disableNetwork() {
  const deny = () => {
    const error = new Error('Network access disabled by offline test mode');
    error.name = 'NetworkDisabledError';
    throw error;
  };
  http.request = deny;
  https.request = deny;
  net.connect = deny;
  tls.connect = deny;
}

void main();
