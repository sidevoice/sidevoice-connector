# Native Core consumer slice

The Node SEA remains the public installer/control CLI. The Rust candidate embeds the matching Rust Connector executable and one Rust Core archive in each target SEA; the selected runtime still uses a separate Core process.

## Trusted build input

[`rust-core-production-pin.json`](./rust-core-production-pin.json) pins the reviewed protected-main Core source commit `b41840e41e3eb81905d285514c7deb35bd8efe57` and its `Cargo.lock` SHA-256. [`rust-core-consumer.yml`](../../.github/workflows/rust-core-consumer.yml) checks out that public commit without persisted credentials on each native target runner. It reuses Core's locked build, model staging and native bundle scripts; the model and notice downloads are hash checked by those scripts. A same-run job verifies all three archives and writes Core's unchanged closed `rust-native-v1` manifest. The target candidate build checks out the exact Connector SHA, checks the manifest, lock digest and all archive bytes, and embeds only its matching Core archive. No precompiled cross-repository Actions artifact, long-lived Core credential or install-time Core fetch is involved.

Core's independent T7 run remains the accepted source/behavior proof. Newly compiled archives are not represented as byte-identical to, or Sigstore-attested by, that producer run. The protected-main Connector build must attest the final tested SEA containing the generated archive and record Core source, lock, archive and Connector digests in its pin. A PR check alone is not production provenance.

The npm ESM build remains free of the native pin and asset. The native consumer is enabled only for an explicit target SEA build; it rejects Python Core manifest inputs and installation-time Core overrides.

## Install transaction and runtime paths

The existing `install.apply()` flow owns the transaction. It reads the embedded asset, rechecks the embedded manifest and archive digests, extracts into the temporary sibling release, strips exactly the one fixed `sidevoice-core-rust/` root, verifies `native-core.json` and every file, then runs the native self-test before writing `release.json` or switching `current`.

The installed paths are:

```text
<release>/dist/sidevoice                         # JavaScript distributor and control CLI
<release>/dist/sidevoice-rust                    # selected Rust connector/MCP runtime
<release>/core/bin/sidevoice-core-rust           # selected Rust Core
<release>/core/models/
<release>/core/checks/
<release>/core/lib/
```

The staged self-test is `<release>/core/bin/sidevoice-core-rust --self-test <release>/core/checks/detector-16k.wav <release>/core/models`. Service and on-demand launch use the fixed T7 flags and set `RUSTVANI_CACHE_DIR` to `<release>/core/models`; ambient dynamic-loader override variables are removed. No installer or runtime path fetches Core bytes. The producer's internal file inventory is checked before the root is stripped.

Cancellation and verification failures remove the temporary sibling release before commit. After `current` switches, the existing readiness verifier and `flipBack()` rollback path remain the commit/recovery owner; the Core uses the same private data directory during restart and rollback. A selected native release with an incomplete or different identity fails closed. An already selected Python release remains addressable by its existing Python entrypoint during rollback; it is never used as fallback for a selected native release.

## Release identity and next integration seam

`release.json` keeps `format: "sea"` as the distributor format and separately records:

- `runtime_kind: "rust-native-v1"`, `runtime_build_sha`, and the exact staged `runtime_sha256` for the selected daemon/MCP runtime;
- `core_kind: "rust-native-v1"`, Core source and Cargo lock SHAs, manifest SHA, target, archive SHA and size, fixed entrypoint, and `core_build`;
- `pair_id`, derived from the selected JavaScript runtime identity and native Core identity.

The selected Rust pair embeds and hashes both executables and records their distinct identities under one release pointer. Runtime changes must quiesce the previous writer and refuse a nonempty outbox before selection; automatic recovery quarantines uncertain cross-runtime rows. No old pending row is remapped.

This source-build proof does not by itself publish a Desktop pin or establish integrated Mac acceptance. Final exact-head source reviews, all-target dispatch, protected-main package attestation/pin, and integrated Mac verification remain separate gates.
