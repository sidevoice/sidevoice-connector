# R4-b: Node 22 SEA + sigstore-js feasibility spike

Status: **feasible on Linux x86_64**. The injected Node 22 single executable ran `sigstore` and verified a genuine
Sigstore public-good SLSA bundle. A warmed TUF cache also verified with Node's HTTP, HTTPS, TCP and TLS connection
APIs disabled inside the SEA process. This is only the bounded verifier spike; it does not build the connector or
installer.

## Reproduce

From this directory, with Node 22 and network access for npm and the first TUF metadata refresh:

```sh
npm ci
npm test
```

`npm test` bundles `sea.mjs` to CommonJS with esbuild (`target: node22`), creates a Node SEA preparation blob,
injects it into the running Node binary with `postject`, and runs that executable directly. The test fixture is
checked in, so it does not fetch a bundle or artifact. The first verification refreshes an empty TUF cache from the
Sigstore public-good mirror; subsequent test cases exercise cold-offline failure and warm-cache offline success.

The executable accepts `bundle.json artifact.tgz tuf-cache [warm|offline]`. In `offline` mode, it sets
`tufForceCache: true` and replaces Node's HTTP, HTTPS, TCP and TLS connection methods with a throwing guard before
calling the verifier.

## Fixture

The fixture is the SLSA provenance bundle published by npm for `sigstore@5.0.0`, extracted from the public npm
attestation response at
[`registry.npmjs.org/-/npm/v1/attestations/sigstore@5.0.0`](https://registry.npmjs.org/-/npm/v1/attestations/sigstore%405.0.0).
Its subject names `pkg:npm/sigstore@5.0.0` and the SHA-512 below matches the checked-in tarball fetched from
[`registry.npmjs.org/sigstore/-/sigstore-5.0.0.tgz`](https://registry.npmjs.org/sigstore/-/sigstore-5.0.0.tgz).
The signer is from the public `sigstore/sigstore-js` repository, separate from `sidevoice-core`.

| Item | Value |
|---|---|
| Bundle media type | `application/vnd.dev.sigstore.bundle.v0.3+json` |
| Predicate | `https://slsa.dev/provenance/v1` |
| Subject | `pkg:npm/sigstore@5.0.0` |
| Subject SHA-512 | `849a897e81bf7b8a8541a6ae40bd3473a27a16b1cb0462ad905bd6dc96d34881053a12eb4a375233f81677fc9439f628fe0b0445e8535c60b5559597fbd5f80e` |
| Certificate issuer | `https://token.actions.githubusercontent.com` |
| Certificate identity | `https://github.com/sigstore/sigstore-js/.github/workflows/release.yml@refs/heads/main` |
| Checked-in bundle SHA-256 | `df9038fbb4fff417d6152864cf938a75dcb3460deb9f8099651b13305a9fc530` |
| Checked-in package tarball SHA-256 | `3718e9a74a5a824a785b5f3930f767a4efdb052b3d4db65cd7649a5e2aa2f465` |

## Recorded run

Run date: 2026-10-02. Host: Node `v22.23.3`, `linux/x64`. Exact dependencies are locked in `package-lock.json`:
`sigstore@5.0.0`, `@sigstore/tuf@5.0.0`, `esbuild@0.25.12` and `postject@1.0.0-alpha.6`.

The generated SEA reported `sea: true` and `node: v22.23.3`; it verified the subject digest and returned this signer:

```json
{"ok":true,"sea":true,"node":"v22.23.3","bundleMediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","predicateType":"https://slsa.dev/provenance/v1","subject":"pkg:npm/sigstore@5.0.0","sha512":"849a897e81bf7b8a8541a6ae40bd3473a27a16b1cb0462ad905bd6dc96d34881053a12eb4a375233f81677fc9439f628fe0b0445e8535c60b5559597fbd5f80e","signerIdentity":{"issuer":"https://token.actions.githubusercontent.com","subjectAlternativeName":"https://github.com/sigstore/sigstore-js/.github/workflows/release.yml@refs/heads/main"},"networkDisabled":false}
```

The same output with `"networkDisabled":true` was produced using the already populated cache.

| Output | Bytes | Approx. size |
|---|---:|---:|
| Node executable before injection | 124,827,920 | 119.0 MiB |
| esbuild CommonJS entry | 1,819,777 | 1.74 MiB |
| SEA preparation blob | 1,819,919 | 1.74 MiB |
| Injected SEA executable | 126,684,352 | 120.77 MiB |

The run exercised the default public-good TUF mirror and populated these files under the supplied cache path:
`root.json`, `timestamp.json`, `snapshot.json`, `targets.json`, `targets/trusted_root.json` and
`targets/registry.npm.org%2Fkeys.json`. `@sigstore/tuf` embeds an initial root and seed targets in its package;
sigstore-js then uses the TUF updater and persistent cache. `tufForceCache` prevents downloads while cached metadata
is unexpired. The behavior and option are documented by the
[`@sigstore/tuf` package](https://www.npmjs.com/package/%40sigstore/tuf).

## Tests and failure behavior

`npm ci && npm test` passed: **5 tests, 5 passed, 0 failed**. The tests run the injected executable, not the JS entry
under Node:

1. Check that the checked-in artifact SHA-512 matches the signed SLSA subject.
2. From a new cache, refresh TUF metadata and verify the bundle, subject digest, issuer and workflow identity.
3. With an empty cache and network disabled, fail closed with `TUFError: error refreshing TUF metadata`.
4. With the warmed cache and network disabled, verify successfully.
5. With one signature byte changed, reject with `VerificationError: tlog entry signature mismatch`.

During development, a call of `verify(bundle, undefined, options)` silently discarded `options` because the library's
no-payload overload is `verify(bundle, options)`. The cache-path assertion exposed the mistake; the spike now uses the
documented overload. This was an API-use error, not a SEA limitation.

`postject` emitted repeated ELF warnings that it could not find section-name offsets for `.note` and `.note.100`, then
reported injection complete. The resulting binary passed all five tests. No Sigstore or TUF verification failure
occurred for the genuine bundle.

## R4-a availability and limits

The latest public `sidevoice-core` release checked for this spike was `nightly`, published 2026-10-01. Its assets were
`models-catalog.json`, `models-vectors.json`, `SHA256SUMS`, the wheel and the source distribution. It did not yet have
the R4-a core bundle, `core-manifest.json` or `.sigstore.json` assets, so this run cannot be repeated against the
Sidevoice core yet. Repeat the same verifier test against those assets when R4-a publishes them.

Only the Linux x86_64 Node binary was injected and tested. macOS arm64, Linux arm64, archive safety, certificate and
provenance pins specific to Sidevoice, full connector bundling, and the installer are outside this spike. The measured
Linux SEA is 126,684,352 bytes; the final connector executable will be larger if its bundled code exceeds this minimal
Sigstore entry. This spike found no reason to invoke the minisign fallback.
