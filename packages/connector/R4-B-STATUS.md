# R4-b connector handoff

Branch `feat/r4-single-executable` keeps the npm ESM client and adds a Node 22 CommonJS SEA build, signed core asset verification, safe bundle staging, and R1 release/service lifecycle integration. The SEA install path refuses to proceed when the embedded manifest is absent or an asset fails any integrity or provenance check.

## Local evidence

- Node `v22.23.3`, Linux x86_64. The native SEA ran directly, answered `--version` as `0.6.0`, served an MCP stdio handshake, and self-spawned its connector with an empty `PATH`.
- Latest measured executable: `127,470,784` bytes. SEA preparation blob: `2,597,214` bytes. This local build reports `manifestEmbedded: false` because R4-a assets were not present.
- `node --test packages/connector/test/test_r4.mjs`: 14 passed, 0 failed. Its Sigstore/TUF cases run the production SEA when available. A genuine public npm Sigstore bundle verifies cryptographically through the SEA's bundled sigstore-js and reaches the expected `workflow` refusal for the unrelated signer.
- TUF checks through the SEA: cold online and warm offline verification pass; cold, expired, and corrupt offline caches fail closed with the `sigstore` check named. Production verification does not force cache-only mode.
- `node --test packages/connector/test/test_install.mjs`: 15 passed, 0 failed. Covers R1 install, crash recovery, rollback, concurrency, legacy Node interpreter recovery, format-aware service commands, and registration lifecycle.
- `npm test -w @sidevoice/uplink`: 140 tests; 138 passed, 2 skipped, 0 failed. The two skips need a separate sidevoice-core checkout with its `.venv`.
- GitHub Actions on implementation commit `90cc15dac031ee16feb4d93748ab82bbf37ebaf4` passed the native Linux x86_64, Linux arm64, and macOS arm64 SEA jobs; Client (Node), real-core interoperability, launchd, systemd, and PR-title jobs also passed. Release attachment and npm publication jobs were skipped for the draft PR.
- The first macOS run found a path spelling difference in the existing open-SQLite-process test (`/var/...` versus `lsof`'s `/private/var/...`). The test now resolves both the expected path and open-file paths through `realpathSync`; the macOS job passed on rerun.
- Independent Astra review found no remaining concrete security or lifecycle blockers after the fixes. Native target CI is configured in `.github/workflows/r4-sea.yml`.

## R4-a inputs still required

- No genuine R4-a manifest, bundle, wheel, or Sigstore sidecar was present in the core checkout or workspace when this branch was built. The local executable therefore refuses core installation with the named `manifest` check. The public npm fixture proves public-good Sigstore/TUF behavior inside the SEA; it is not treated as an R4-a core asset.
- The parser currently expects `{version, bundles:[{os,arch,url,sha256,size}], wheel:{url,sha256}}` and the asset filenames described in §4.5. The design specifies the asset fields but not the full JSON envelope. Confirm this shape and filenames against the R4-a manifest and sidecars before embedding them in a production connector.
- Provenance channel mapping follows §4.5: release connector assets use `release-please.yml`; nightly connector assets use `test.yml`. Repeat the pins against genuine release and nightly R4-a assets when available.
- The real bundle install, unsupported-platform wheel fallback, clean-account install with no Node/Python/uv, and macOS quarantine behavior remain to be exercised against R4-a assets. The local empty-`PATH` SEA test uses a fake core wrapper, not a genuine core bundle.
- The new workflow builds and tests native artifacts; it does not publish connector releases or alter an existing release publisher. Production packaging must pass the genuine R4-a manifest and sidecar to `build.mjs`; a missing manifest remains a hard refusal.
- Wheel installation follows §4.5: the wheel is checked against the embedded SHA-256 and Sigstore provenance, then uv applies its configured hash checking to PyPI dependencies. No separate dependency-lock asset is specified by that section.

No minisign path was added. No merge or release was made.

## Astra follow-up on PR #29

The three concrete Astra blockers reported against `ab75dd9faa1a06892478fe3563b20676c787de6a` are addressed in the follow-up changes:

- Uninstall snapshots the release root before deleting it, so one-element SEA commands do not cause a path lookup crash. A native SEA regression performs `install --no-core --no-agents`, then `uninstall --json`, and verifies release files, install state, logs, and credentials are removed; only the permanent locks and stop marker remain.
- Core manifest validation accepts the exact pinned asset paths under either `/v<version>/` for release or `/nightly/` for nightly, while retaining the GitHub host, HTTPS, asset-name, and channel checks. Wrong-channel, wrong-tag, wrong-name, and query-string cases refuse.
- npm release/nightly packaging downloads the R4-a manifest and Sigstore sidecar and passes both to the build. The build verifies the sidecar before embedding the signed manifest and refuses production packaging when the manifest input is absent. Main-branch SEA packaging likewise downloads and passes the genuine nightly pair and fails closed if missing.

PR CI does not have R4-a assets: its SEA jobs exercise the public Sigstore/TUF fixture and fail-closed behavior, which tests the packaging boundary but is not genuine R4-a integration. Main-branch production packaging is wired to the R4-a nightly assets; release packaging is wired to the versioned R4-a assets. Repeat the real asset verification and installation checks when those assets are published.

Focused regressions pass locally (4/4, including native SEA cleanup). `npm test -w @sidevoice/uplink` passes 142 tests, skips 2 real-core interop tests because the core `.venv` is absent, and has 0 failures. The SEA exercised here still has no genuine R4-a manifest, so this verifies the public fixture, fail-closed packaging boundary, and lifecycle fix; it does not claim a real R4-a install. PR CI results are recorded after they finish.
