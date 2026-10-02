import { chmod, copyFile, mkdir, stat, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { build } from 'esbuild';

const root = path.dirname(fileURLToPath(import.meta.url));
const dist = path.join(root, 'dist');
const entry = path.join(root, 'sea.mjs');
const cjsEntry = path.join(dist, 'sea.cjs');
const configPath = path.join(dist, 'sea-config.json');
const blobPath = path.join(dist, 'sea-prep.blob');
const executablePath = path.join(dist, 'sigstore-sea');

await mkdir(dist, { recursive: true });
await build({
  entryPoints: [entry],
  outfile: cjsEntry,
  bundle: true,
  platform: 'node',
  target: 'node22',
  format: 'cjs',
  packages: 'bundle',
  logLevel: 'info',
});

await writeFile(configPath, JSON.stringify({
  main: cjsEntry,
  output: blobPath,
  disableExperimentalSEAWarning: true,
  useCodeCache: false,
}, null, 2) + '\n');

execFileSync(process.execPath, ['--experimental-sea-config', configPath], { cwd: root, stdio: 'inherit' });
await copyFile(process.execPath, executablePath);
await chmod(executablePath, 0o755);
execFileSync(path.join(root, 'node_modules/.bin/postject'), [
  executablePath,
  'NODE_SEA_BLOB',
  blobPath,
  '--sentinel-fuse',
  'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2',
], { cwd: root, stdio: 'inherit' });

const [executable, blob] = await Promise.all([stat(executablePath), stat(blobPath)]);
process.stdout.write(JSON.stringify({
  node: process.version,
  executable: executablePath,
  executableBytes: executable.size,
  seaBlobBytes: blob.size,
}) + '\n');
