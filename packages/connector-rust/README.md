# Isolated Rust connector proof

This package is an experimental Codex-only connector. It is deliberately outside the npm package, release selection, service jobs and Desktop pin. It cannot install or start Core. The JavaScript connector remains the selected product.

Set `SIDEVOICE_DATA_DIR` to a fresh private directory such as `$P/sidevoice` and `CODEX_HOME` to `$P/codex`, where `$P` is a new `0700` temporary directory. Launch a v3-enabled Core separately with `--data-dir $P/sidevoice/core --socket $P/sidevoice/core/local.sock`. The Core writes its own ready file. Start this binary with `connector`, then register `mcp` in that isolated Codex profile with `codex mcp add sidevoice -- /absolute/path/sidevoice-rust-proof mcp`.

`codex inspect` checks whether the isolated profile's `sidevoice` entry is absent, owned, foreign, or invalid. `codex connect` only adds the entry when absent; it never overwrites a foreign or invalid entry. The command is explicitly run by the test operator and never touches the default Codex profile.

The daemon writes `proof.json` with its PID, executable path and SHA-256, Core launch ID and successful protocol version. The file is evidence of what connected, not an installation record. No build from this directory is an upgrade artifact.
