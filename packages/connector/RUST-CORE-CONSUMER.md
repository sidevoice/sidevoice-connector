# Native Core consumer slice

This change keeps the Node SEA as the public installer/control CLI and keeps its JavaScript Connector daemon selected. It embeds one protected-main Rust Core archive inside each target SEA. It does **not** select the Rust Connector executable and is not the final Rust/Rust beta pair.

## Trusted build input

[`rust-core-production-pin.json`](./rust-core-production-pin.json) pins Core `b41840e41e3eb81905d285514c7deb35bd8efe57`, protected-main T7 run `37165556340`, the canonical native manifest, and the exact archive name, size and SHA-256 for each of the three SEA targets. [`rust-core-consumer.yml`](../../.github/workflows/rust-core-consumer.yml) uses a base-defined `pull_request_target` verifier for same-repository pull requests and a protected-main push verifier. Fork pull requests remain on ordinary CI and are outside this credentialed gate. The verifier job has no checkout and no candidate code. It requires a distinct, short-lived `SIDEVOICE_CORE_ACTIONS_READ` secret scoped to Actions:read on `sidevoice/sidevoice-core`; it exits before download if the secret is absent. This credential is not the Desktop `SIDEVOICE_CONNECTOR_ACTIONS_READ` token, which is scoped to Connector. The candidate matrix starts only after verification succeeds, receives no Core credential, checks out the exact candidate SHA with `persist-credentials: false`, and is limited to read-only Actions, contents and cache access. There is no arbitrary-SHA manual dispatch. When the credential is configured, the verifier downloads that exact `rust-native-signed-inputs` artifact and verifies the manifest and selected archive with `gh attestation verify` against repository `sidevoice/sidevoice-core`, workflow `rust-t7.yml`, `refs/heads/main`, and the pinned source SHA. `build.mjs` then verifies the closed 8-file artifact membership, canonical manifest, exact target record, bytes, and Sigstore sidecars before it passes only that target archive to Node SEA as `sidevoice-rust-core.tar.zst`. The existing R4 SEA workflow supplies PR build/test coverage without the cross-repository credential.

The first trusted main run, Connector run `37171588054`, failed closed before artifact download because `SIDEVOICE_CORE_ACTIONS_READ` resolved to an empty value. The verification step emitted the required-secret failure; upload and all three candidate jobs were skipped. This is a confirmed credential setup blocker, not a successful signed-positive consumer proof. The gate must not be bypassed or given the Connector-scoped Desktop token. Install a credential explicitly scoped to Core Actions:read, then rerun the exact candidate SHA before treating any hosted Core consumer result as positive.

The npm ESM build remains free of the native pin and asset. The native consumer is enabled only for an explicit target SEA build; it rejects Python Core manifest inputs and installation-time Core overrides.

## Install transaction and runtime paths

The existing `install.apply()` flow owns the transaction. It reads the embedded asset, rechecks its signed manifest digest and archive digest, extracts into the temporary sibling release, strips exactly the one fixed `sidevoice-core-rust/` root, verifies `native-core.json` and every file, then runs the native self-test before writing `release.json` or switching `current`.

The installed paths are:

```text
<release>/dist/sidevoice                         # JavaScript distributor and daemon
<release>/core/bin/sidevoice-core-rust           # selected Rust Core
<release>/core/models/
<release>/core/checks/
<release>/core/lib/
```

The staged self-test is `<release>/core/bin/sidevoice-core-rust --self-test <release>/core/checks/detector-16k.wav <release>/core/models`. Service and on-demand launch use the fixed T7 flags and set `RUSTVANI_CACHE_DIR` to `<release>/core/models`; ambient dynamic-loader override variables are removed. No installer or runtime path fetches Core bytes. The producer's internal file inventory is checked before the root is stripped.

Cancellation and verification failures remove the temporary sibling release before commit. After `current` switches, the existing readiness verifier and `flipBack()` rollback path remain the commit/recovery owner; the Core uses the same private data directory during restart and rollback. A selected native release with an incomplete or different identity fails closed. An already selected Python release remains addressable by its existing Python entrypoint during rollback; it is never used as fallback for a selected native release.

## Release identity and next integration seam

`release.json` keeps `format: "sea"` as the distributor format and separately records:

- `runtime_kind: "javascript"`, `runtime_build_sha`, and the exact staged `runtime_sha256` for the selected daemon/MCP runtime;
- `core_kind: "rust-native-v1"`, Core source and Cargo lock SHAs, manifest SHA, target, archive SHA and size, fixed entrypoint, and `core_build`;
- `pair_id`, derived from the selected JavaScript runtime identity and native Core identity.

This distinction lets a later candidate treat an equal package version with a different runtime kind as an upgrade. The follow-on Rust Connector transaction must embed and hash its target executable, write `runtime_kind: "rust-native-v1"` and its exact runtime digest into this same record, derive the service/MCP command from that selected record, and prove readiness and rollback in this same release-pointer transaction. `install.apply()` currently waits for calls when `core_build` changes; the follow-on must gate on the runtime change too and add the reviewed precommit empty-outbox refusal before any pointer or registration mutation. It must not remap old pending rows. That work is outside this Core consumer slice.

Desktop selection, manual Mac acceptance, and a claim that the beta is a Rust Connector plus Rust Core pair remain later gates.
