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

## Desktop CLI handoff after `e1a2dda`

The connector implements the active Desktop `pin.rs` and `connector-pin.json` field names. `--version --json` reports
`ok`, `version`, `target`, `channel`, `connector_sha`, and `build_seq`; `metadata --json` reports connector
`{version,sha,channel,build_seq,link_min,link_max}`, embedded core `{version,manifest_sha256,assets,api,link}`, and
`sidevoice-metadata-v1` / `sidevoice-progress-jsonl-v1`. `assets` contains every signed bundle record in producer
order, independent of the executable's target, as required for Desktop's exact array comparison. The metadata test
builds a CLI from core PR #34's synthetic producer fixture and checks the complete output. The fixture is not genuine
R4-a asset evidence.

The connector preserves the producer manifest bytes exactly as `{bundles,wheel}` and reports core version separately.
The active Desktop pin accepts this exact producer schema and binds the separate `core_version` through versioned
asset URLs, bundle hashes and the pinned manifest digest. Its current target is macOS arm64, which has a size-bearing
bundle entry. The core wheel record has no size; a future Desktop pin for a target requiring wheel fallback needs a
size-bearing wheel contract or an explicitly optional size field.

Progress records remain bounded JSONL on stderr with one final result on stdout. Pre-commit cancellation removes the
partial uv runtime and candidate release, preserves any old selection and stop intent, and returns
`install.cancelled`. After commit begins, verification or rollback completes before reporting the actual result.
Transport failures use `install.network`, recognized proxy/TLS or HTTP 407 failures use `install.proxy`, and disk
exhaustion uses `install.disk`; digest, manifest, Sigstore and provenance refusals remain `install.authenticity` with a
named check. Unclassified transport errors use the network key; proxy classification is limited to explicit HTTP 407
or recognizable TLS certificate errors. Desktop receives only allowlisted authenticity check parameters.

The independent GPT Sol review of `c28c0ef39d0bb642ce9f328437d3d68f4f97d5bc` blocked on the full asset list, partial
first-install uv cleanup and stop-intent preservation, and stable network/proxy/disk keys. Those findings are addressed
in the current follow-up with focused regressions; the review must be repeated on the new exact head. Per the operator's
binding rule, all subsequent adversarial reviews use GPT Sol, never Astra.

Genuine R4-a manifest, bundle, wheel and Sigstore sidecars remain unavailable. Public Sigstore/TUF fixtures and the
synthetic producer manifest test cryptographic-library behavior, cache failure behavior and schema compatibility; they
are not evidence of a genuine Sidevoice core installation. Real R4-a verification/install, no-runtime clean-account
execution and macOS quarantine remain later integration gates. No merge or release was made.

## Native CI artifact handoff for Desktop PR #21

`.github/workflows/r4-sea.yml` keeps pull-request and non-main dispatch SEA builds manifestless and test-only, with no
production artifact upload. On protected `main`, all three native test jobs must pass before the macOS arm64 SEA is
uploaded as `sidevoice-connector-macos-aarch64-r4b`; the ZIP contains only root `sidevoice` and its bytes are checked
against the tested build. A separate post-test job uses `actions/attest@v4` to create a genuine public-good Sigstore
build attestation for that executable, verifies it against the exact signer workflow, source ref/commit, and
GitHub-hosted runner identity, then uploads the bundle in
`sidevoice-connector-macos-aarch64-r4b-provenance`. The final macOS job verifies the downloaded bundle again and
generates `sidevoice-connector-macos-aarch64-r4b-pin` from official run-artifact metadata.

The pin uses the documented `https://api.github.com/repos/sidevoice/sidevoice-connector/actions/artifacts/<id>/zip`
route. Its provenance sidecar URL points to the provenance artifact ZIP; its digest and size are the ZIP-level
`digest`/`size_in_bytes` fields from GitHub's artifact API. That ZIP contains only root `sidevoice.sigstore.json`.
Desktop must verify the ZIP bytes, extract the root bundle, then cryptographically verify the bundle against the SEA.
The connector-side fetch/auth contract and exact Desktop follow-up requirements are recorded in
[`R4-B-DESKTOP-HANDOFF-2026-10-02.md`](R4-B-DESKTOP-HANDOFF-2026-10-02.md).

Actions artifacts are temporary dogfood inputs retained for 90 days, not a durable release source. The current
`sidevoice/sidevoice-core` nightly release listing contains no `core-manifest.json`, its `.sigstore.json` sidecar, or
the R4-a platform bundles, so no genuine production SEA or Desktop pin can be emitted until those core assets are
published. PR/test-only manifestless artifacts must not be used for Desktop packaging.
