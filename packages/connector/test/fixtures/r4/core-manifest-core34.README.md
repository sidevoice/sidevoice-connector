# Cross-repository core manifest fixture

`core-manifest-core34.json` is the output of `tools/core_manifest.py` from `sidevoice/sidevoice-core` commit
`ba6aecab3805e6b243d925701be8c3968ce51978` (PR #34). The producer source SHA-256 was
`1f3a0022a80e3d29929b8df62abf5182c581fa0636f34b2252a6e7109cace4a2`.

The four input files contained only `fixture-only synthetic bytes for <filename>\n`; their hashes and sizes are
therefore fixture values. This fixture checks the producer/consumer JSON contract and pinned URL shape. It is not
a genuine R4-a manifest, asset, signature, or integration result.
