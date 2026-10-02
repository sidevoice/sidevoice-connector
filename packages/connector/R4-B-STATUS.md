# R4-b connector handoff

Branch `feat/r4-single-executable` keeps the npm ESM client and adds a Node 22 CommonJS SEA build, signed core asset verification, safe bundle staging, and R1 release/service lifecycle integration. The SEA install path refuses to proceed when the embedded manifest is absent or an asset fails any integrity or provenance check.

## Local evidence

- Node `v22.23.3`, Linux x86_64. The native SEA ran directly, answered `--version` as `0.6.0`, served an MCP stdio handshake, and self-spawned its connector with an empty `PATH`.
- Latest measured executable: `127,470,784` bytes. SEA preparation blob: `2,597,643` bytes. This local build reports `manifestEmbedded: false` because R4-a assets were not present.
- `node --test packages/connector/test/test_r4.mjs`: 20 passed, 0 failed. Its Sigstore/TUF cases run the production SEA when available. A genuine public npm Sigstore bundle verifies cryptographically through the SEA's bundled sigstore-js and reaches the expected `workflow` refusal for the unrelated signer.
- TUF checks through the SEA: cold online and warm offline verification pass; cold, expired, and corrupt offline caches fail closed with the `sigstore` check named. Production verification does not force cache-only mode.
- `node --test packages/connector/test/test_install.mjs`: 15 passed, 0 failed. Covers R1 install, crash recovery, rollback, concurrency, legacy Node interpreter recovery, format-aware service commands, and registration lifecycle.
- `npm test -w @sidevoice/uplink`: 146 tests; 144 passed, 2 skipped, 0 failed. The two skips need a separate sidevoice-core checkout with its `.venv`.
- GitHub Actions on implementation commit `90cc15dac031ee16feb4d93748ab82bbf37ebaf4` passed the native Linux x86_64, Linux arm64, and macOS arm64 SEA jobs; Client (Node), real-core interoperability, launchd, systemd, and PR-title jobs also passed. Release attachment and npm publication jobs were skipped for the draft PR.
- The first macOS run found a path spelling difference in the existing open-SQLite-process test (`/var/...` versus `lsof`'s `/private/var/...`). The test now resolves both the expected path and open-file paths through `realpathSync`; the macOS job passed on rerun.
- Independent Astra review found no remaining concrete security or lifecycle blockers after the fixes. Native target CI is configured in `.github/workflows/r4-sea.yml`.

## R4-a inputs still required

- No genuine R4-a manifest, bundle, wheel, or Sigstore sidecar was present in the core checkout or workspace when this branch was built. The local executable therefore refuses core installation with the named `manifest` check. The public npm fixture proves public-good Sigstore/TUF behavior inside the SEA; it is not treated as an R4-a core asset.
- Core PR #34 (`ba6aecab3805e6b243d925701be8c3968ce51978`) confirms the producer envelope is exactly `{bundles, wheel}`. The consumer is aligned to that shape; it binds assets to the connector's pinned core version through the exact release tag and versioned asset filename, with each downloaded asset bound to the signed manifest SHA-256.
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

Focused regressions pass locally (5/5 for producer schema, fallback, and nightly URL coverage; 4/4 after rebuilding for the producer fixture, fallback, and native SEA run/uninstall). `npm test -w @sidevoice/uplink` passes 146 tests, skips 2 real-core interop tests because the core `.venv` is absent, and has 0 failures. The SEA exercised here still has no genuine R4-a manifest, so this verifies the public fixture, fail-closed packaging boundary, and lifecycle fix; it does not claim a real R4-a install. PR CI results are recorded after they finish.

## Follow-up to final-head review at `6bebc27`

- The producer/consumer regression fixture was generated by core PR #34's `tools/core_manifest.py` from commit `ba6aecab3805e6b243d925701be8c3968ce51978`. It uses synthetic file bytes to test the real producer schema and URLs; it is explicitly not genuine R4-a asset evidence.
- Core selection now uses a signed bundle only when the manifest contains the current platform entry. Missing or unmapped bundle targets use the signed wheel path. With no uv, the public install returns the keyed `install.no-bundle`; with uv, the wheel and its sidecar must still pass digest and Sigstore verification.
- Actual R4-a bundle/wheel installation, no-runtime clean account, and macOS quarantine checks remain a later gate until genuine assets and sidecars are available.
- Focused follow-up regressions pass 5/5; the complete connector suite passes 144 tests with 2 separate-core skips. Rebuilt on Node `v22.23.3` as Linux x86_64 SEA (`127,470,784` executable bytes, `2,597,643` preparation-blob bytes, `manifestEmbedded: false`); the rebuilt executable passes direct-run and uninstall regressions. Exact-head PR CI and independent Astra re-review are pending.

## Desktop CLI handoff work after `e1a2dda`

The connector implements the field names in the active desktop WIP's `pin.rs` and `connector-pin.json` contract:
`--version --json` reports `ok`, `version`, `target`, `channel`, `connector_sha`, and `build_seq`; `metadata --json`
reports connector `{version,sha,channel,build_seq,link_min,link_max}`, embedded core `{version,manifest_sha256,assets,api,link}`,
and the exact `sidevoice-metadata-v1` / `sidevoice-progress-jsonl-v1` identifiers. Install progress uses only the desktop's
stable step enum, bounded JSONL on stderr, and a single final JSON result on stdout. Downloads report artifact-byte
counters; non-download work reports null counters. Pre-commit SIGINT returns keyed `install.cancelled`; after the
selection switch it is ignored while verification or rollback produces the true final result.

The signed-manifest identity has one unresolved cross-repository contract mismatch. Core PR #34 produces exactly
`{bundles,wheel}`. Connector embeds those exact signed bytes, hashes them, and reports `CORE_VERSION` separately; it
does not add an unsigned `version` property. Desktop `ConnectorPin::validate_ready()` currently insists that the
decoded manifest contains `version == core_version`, so it cannot accept this authentic producer output as written.
The safe options are: (1) update desktop validation to accept the producer's exact two-key schema and bind its version
through the pinned `core_version`, exact versioned asset URLs, artifact hashes, and manifest SHA-256; or (2) revise the
core producer schema, re-sign the changed manifest and assets, and update both consumer contracts. Option 1 preserves
the producer's current signed bytes and is the connector's recommendation. Do not synthesize `version` into the
manifest or its digest. Desktop pins also require `size` for every `CoreAssetPin`; the core wheel record currently has
no size. The shipped macOS target's bundle record has the required size, but a desktop pin for a no-bundle target would
need either a size-bearing wheel field from core or an explicitly optional wheel-size contract.

Metadata lists only the bundle record for the executable's own target. Its manifest digest still binds all bundle and
wheel records. This matches desktop pinning to the macOS arm64 executable; fallback-wheel installs on other/no-bundle
targets remain covered by connector tests but cannot produce the current desktop `CoreAssetPin` array until its
size contract is resolved.

Local verification on Node `v22.23.3`, Linux x86_64: `npm run build:sea -w @sidevoice/uplink` produced a 127,470,784-byte
executable and a 2,611,620-byte SEA blob; `manifestEmbedded: false`. `npm test -w @sidevoice/uplink` passed 148 tests,
skipped the 2 separate-core `.venv` interop cases, and failed none. Focused runs also passed the metadata/progress,
Sigstore verifier abort, install cancellation, and native SEA regressions. The local SEA remains manifestless because
genuine R4-a assets and sidecars are unavailable; synthetic producer fixtures and the public Sigstore fixture are test
evidence only, not R4-a integration. Fresh exact-head CI and independent GPT Sol review are pending. No merge or release
was made.
