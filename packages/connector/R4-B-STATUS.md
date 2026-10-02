# R4-b connector handoff

Branch `feat/r4-single-executable` keeps the npm ESM client and adds a Node 22 CommonJS SEA build, signed core asset verification, safe bundle staging, and R1 release/service lifecycle integration. The SEA install path refuses to proceed when the embedded manifest is absent or an asset fails any integrity or provenance check.

## Local evidence

- Node `v22.23.3`, Linux x86_64. The native SEA ran directly, answered `--version` as `0.6.0`, served an MCP stdio handshake, and self-spawned its connector with an empty `PATH`.
- Latest measured executable: `127,470,784` bytes. SEA preparation blob: `2,597,214` bytes. This local build reports `manifestEmbedded: false` because R4-a assets were not present.
- `node --test packages/connector/test/test_r4.mjs`: 14 passed, 0 failed. Its Sigstore/TUF cases run the production SEA when available. A genuine public npm Sigstore bundle verifies cryptographically through the SEA's bundled sigstore-js and reaches the expected `workflow` refusal for the unrelated signer.
- TUF checks through the SEA: cold online and warm offline verification pass; cold, expired, and corrupt offline caches fail closed with the `sigstore` check named. Production verification does not force cache-only mode.
- `node --test packages/connector/test/test_install.mjs`: 15 passed, 0 failed. Covers R1 install, crash recovery, rollback, concurrency, legacy Node interpreter recovery, format-aware service commands, and registration lifecycle.
- `npm test -w @sidevoice/uplink`: 140 tests; 138 passed, 2 skipped, 0 failed. The two skips need a separate sidevoice-core checkout with its `.venv`.
- Independent Astra review found no remaining concrete security or lifecycle blockers after the fixes. Native macOS and Linux arm64 jobs are configured in `.github/workflows/r4-sea.yml`; those hosted jobs have not run from this branch yet.

## R4-a inputs still required

- No genuine R4-a manifest, bundle, wheel, or Sigstore sidecar was present in the core checkout or workspace when this branch was built. The local executable therefore refuses core installation with the named `manifest` check. The public npm fixture proves public-good Sigstore/TUF behavior inside the SEA; it is not treated as an R4-a core asset.
- The parser currently expects `{version, bundles:[{os,arch,url,sha256,size}], wheel:{url,sha256}}` and the asset filenames described in §4.5. The design specifies the asset fields but not the full JSON envelope. Confirm this shape and filenames against the R4-a manifest and sidecars before embedding them in a production connector.
- Provenance channel mapping follows §4.5: release connector assets use `release-please.yml`; nightly connector assets use `test.yml`. Repeat the pins against genuine release and nightly R4-a assets when available.
- The real bundle install, unsupported-platform wheel fallback, clean-account install with no Node/Python/uv, and macOS quarantine behavior remain to be exercised against R4-a assets. The local empty-`PATH` SEA test uses a fake core wrapper, not a genuine core bundle.
- The new workflow builds and tests native artifacts; it does not publish connector releases or alter an existing release publisher. Production packaging must pass the genuine R4-a manifest and sidecar to `build.mjs`; a missing manifest remains a hard refusal.
- Wheel installation follows §4.5: the wheel is checked against the embedded SHA-256 and Sigstore provenance, then uv applies its configured hash checking to PyPI dependencies. No separate dependency-lock asset is specified by that section.

No minisign path was added. No merge or release was made.
