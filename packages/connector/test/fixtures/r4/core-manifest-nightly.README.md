# Genuine core nightly Sigstore fixture

`core-manifest-nightly.sigstore.json` is the unmodified public sidecar from
`https://github.com/sidevoice/sidevoice-core/releases/download/nightly/core-manifest.json.sigstore.json`.
Its SHA-256 is `baa2f650b2a1e484e11d879e357405ad1299327a1f610ad91620370e442e2f2d`.
The signed subject is `core-manifest.json`, SHA-256
`2b71c746af06bee1f22cb3ac13ee09fa50b9ba254d1f54137d9ee22df447b0e1`, from trusted core workflow run
[`37008243094`](https://github.com/sidevoice/sidevoice-core/actions/runs/37008243094).

The sidecar was cryptographically verified with sigstore-js 5.0.0 and the Sigstore public-good trust root on
2026-10-02. The verified certificate runner OID `1.3.6.1.4.1.57264.1.11` is `github-hosted`. The genuine SLSA
statement's builder ID is
`https://github.com/sidevoice/sidevoice-core/.github/workflows/test.yml@refs/heads/main`; its
`buildDefinition.internalParameters.github.runner_environment` is `github-hosted`. The regression test verifies this
fixture and separately rejects self-hosted values in both signed runner claims.
