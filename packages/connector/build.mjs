/** The published package, as one file with nothing to resolve at install time.
 *
 *  `socket.io-client` is a dependency of this source and of no artifact: esbuild puts it inside
 *  `dist/cli.mjs` along with everything else `cli.mjs` reaches, so what npm ships declares no
 *  runtime dependency and `sidevoice install` stays a copy of files that run with `node`. A
 *  machine gaining a voice is not the moment to discover a cold cache or a proxy.
 *
 *  `package.json` is copied next to the bundle because the modules inside it read their own
 *  version from the file beside them, and that is true of the source and of the bundle alike. */
import { fileURLToPath } from 'node:url';
import { copyFile, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { build } from 'esbuild';

/** Some of what travels inside the bundle is CommonJS and calls `require` — for `fs`, for the
 *  optional native speed-ups `ws` asks for and does without. An ES module has no `require`, and
 *  esbuild's stand-in throws rather than guess, so one is made here from this file's own URL.
 *  Without it the bundle dies on its first import (2026-09-22), which is why the interop test
 *  runs against the built artifact and not only the source. */
const REQUIRE = 'import { createRequire } from "node:module";\nconst require = createRequire(import.meta.url);\n';

const here = new URL('./', import.meta.url);
const out = new URL('./dist/', here);
await rm(fileURLToPath(out), { recursive: true, force: true });
await mkdir(fileURLToPath(out), { recursive: true });

await build({
  entryPoints: [fileURLToPath(new URL('./cli.mjs', here))],
  outfile: fileURLToPath(new URL('./cli.mjs', out)),
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node22',
  // Node's own modules are Node's; everything else travels.
  external: ['node:*'],
  legalComments: 'none',
});

const bundle = fileURLToPath(new URL('./cli.mjs', out));
const code = await readFile(bundle, 'utf8');
await writeFile(bundle, code.replace(/^#!.*\n/, line => line + REQUIRE), { mode: 0o755 });

// The modules inside read their own version from the `package.json` beside them, and that is as
// true of the bundle as of the source it was built from.
await copyFile(fileURLToPath(new URL('./package.json', here)), fileURLToPath(new URL('./package.json', out)));
