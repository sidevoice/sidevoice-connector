/** Build the unchanged npm ESM client and, on request, a native Node 22 CommonJS SEA executable. */
import { chmod, copyFile, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { CORE_VERSION } from './core.mjs';
import { verifyCoreArtifact } from './core-attestation.mjs';
import { validateCoreManifest } from './core-bundle.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(root, 'dist');
const outSea = path.join(root, 'dist-sea');
const packagePath = path.join(root, 'package.json');
const original = JSON.parse(await readFile(packagePath, 'utf8'));
const shipped = { ...original };
delete shipped.devDependencies;

// Build channel/order metadata is copied into ESM and compiled into the SEA executable.
if (process.env.SIDEVOICE_CHANNEL || process.env.SIDEVOICE_BUILD_SEQ) {
  const channel = process.env.SIDEVOICE_CHANNEL || 'release';
  if (!['release', 'nightly'].includes(channel)) throw new Error(`SIDEVOICE_CHANNEL is ${channel}: release or nightly`);
  const build_seq = Number(process.env.SIDEVOICE_BUILD_SEQ || 0);
  if (!Number.isSafeInteger(build_seq) || build_seq < 0) throw new Error(`SIDEVOICE_BUILD_SEQ is ${process.env.SIDEVOICE_BUILD_SEQ}: a non-negative integer`);
  shipped.sidevoice = { channel, build_seq };
}

let manifestText = null;
if (process.env.SIDEVOICE_CORE_MANIFEST) {
  const manifestPath = path.resolve(process.env.SIDEVOICE_CORE_MANIFEST);
  const sidecarPath = process.env.SIDEVOICE_CORE_MANIFEST_SIGSTORE
    ? path.resolve(process.env.SIDEVOICE_CORE_MANIFEST_SIGSTORE) : `${manifestPath}.sigstore.json`;
  const raw = await readFile(manifestPath);
  const sidecar = await readFile(sidecarPath).catch(() => { throw new Error(`missing signed core manifest sidecar: ${sidecarPath}`); });
  const parsed = JSON.parse(raw.toString('utf8'));
  validateCoreManifest(parsed, CORE_VERSION);
  const channel = shipped.sidevoice?.channel || 'release';
  await verifyCoreArtifact({ bytes: raw, bundleBytes: sidecar, channel,
    tufCachePath: process.env.SIDEVOICE_TUF_CACHE || path.join(os.homedir(), '.sidevoice', 'sigstore-build'), label: 'core-manifest.json' });
  // Preserve the signed manifest's exact bytes in the generated JavaScript string.
  manifestText = raw.toString('utf8');
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
      'import.meta.url': JSON.stringify('file:///sidevoice-runtime/sea.mjs'),
    },
  });
  await writeFile(configPath, JSON.stringify({ main: cjs, output: blobPath,
    disableExperimentalSEAWarning: true, useCodeCache: false }, null, 2) + '\n');
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
    executableBytes: executable.size, seaBlobBytes: blob.size, manifestEmbedded: !!manifestText }) + '\n');
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
