#!/usr/bin/env node
// `sidevoice`: runs the Sidevoice connector built for this machine.
//
// Of this package's optional dependencies (one per platform), npm installs only the one whose `os`, `cpu` and, on
// Linux, `libc` match this machine. That platform package holds the connector exactly as its release archive lays it
// out, with the inventory `connector.json` naming the binary. This script finds it and runs it with the same
// arguments, standard streams and exit status; it does nothing else. It is written into the npm package by
// `cargo xtask npm` (xtask/src/npm.rs). English only, like the connector's command line.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");

const PREFIX = "@sidevoice/sidevoice-";

function fail(message) {
  process.stderr.write(`sidevoice: ${message}\n`);
  process.exit(1);
}

const platforms = Object.keys(require("../package.json").optionalDependencies || {});
const name = `${PREFIX}${process.platform}-${process.arch}`;
if (!platforms.includes(name)) {
  const supported = platforms.map((platform) => platform.slice(PREFIX.length)).join(", ");
  fail(`${process.platform}-${process.arch} is not supported; Sidevoice runs on ${supported}.`);
}

let binary;
try {
  const root = path.dirname(require.resolve(`${name}/package.json`));
  binary = path.join(root, require(`${name}/connector.json`).entrypoint);
} catch {
  fail(
    `${name} is not installed. npm installs it with this package unless optional dependencies are omitted ` +
      "(--omit=optional, --no-optional) or, on Linux, the C library is not glibc. Reinstall with optional dependencies.",
  );
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  fail(`cannot run ${binary}: ${result.error.message}`);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
