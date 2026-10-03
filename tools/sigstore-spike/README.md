# Hosted Sigstore verifier feasibility proof

This crate is isolated from the connector and its installer. It reads staged files, verifies a Sigstore sidecar with released `sigstore-verify` 0.14.0 and public-good TUF, then enforces Sidevoice's Core provenance pins. It has no extraction, execution, package, or publication path. The Linux Actions job fetches one coherent moving nightly manifest, sidecars, and Linux archive into runner-local temporary files, records source and byte digests, verifies the actual archive bytes, and deletes them. The macOS job verifies the small genuine npm fixture under its own identity.

The crate owns no certificate, DSSE, Rekor, or TUF cryptography. Its adapter checks the raw legacy Fulcio issuer `.1.1` from the same verified bundle after cryptographic success, including required extension multiplicity. It also checks the crate's normalized issuer and SAN policy, CI claims, SLSA predicate, one SHA-256-only subject, and the signed manifest's archive digest and size. The verifier uses `verify_reader`; a second pass on the same open file measures SHA-256 and size for policy and reporting. A future installer would need to protect staged bytes through verification and use; this proof does not establish that transaction.

## Dependency and toolchain finding

`Cargo.lock` came from the first read-only hosted run and pins published Sigstore 0.14.0 packages. Although Sigstore 0.14.0 declares Rust 1.86, the resolved ICU 2.3 dependencies in this lock require Rust 1.88. The isolated workflow therefore runs Rust 1.88 with `--locked`. The existing `packages/connector-rust` proof and its toolchain settings are unchanged. A later integration must either adopt a compatible locked graph/toolchain or pin older compatible transitive dependencies after review.

The nightly is mutable. Each successful hosted run records its exact source run and commit plus SHA-256 of the manifest, sidecars, and archive. The repository contains only the existing small npm and older Core sidecar fixtures, not a Core archive. Release-channel parity and installer download, URL, archive, cancellation, rollback, and error-mapping behavior remain separate gates.
