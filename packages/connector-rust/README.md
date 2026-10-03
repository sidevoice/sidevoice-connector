# Isolated Rust connector proof

This package is an experimental Codex-only connector. It is deliberately outside the npm package, release selection, service jobs and Desktop pin. It cannot install or start Core. The JavaScript connector remains the selected product.

Set `SIDEVOICE_DATA_DIR` to a fresh private directory such as `$P/sidevoice` and `CODEX_HOME` to `$P/codex`, where `$P` is a new `0700` temporary directory. Launch a v3-enabled Core separately with `--data-dir $P/sidevoice/core --socket $P/sidevoice/core/local.sock`. The Core writes its own ready file. Start this binary with `connector`, then register `mcp` in that isolated Codex profile with `codex mcp add sidevoice -- /absolute/path/sidevoice-rust-proof mcp`.

`codex inspect` checks whether the isolated profile's `sidevoice` entry is absent, owned, foreign, or invalid. `codex connect` only adds the entry when absent; it never overwrites a foreign or invalid entry. The command is explicitly run by the test operator and never touches the default Codex profile.

The proof needs the active Codex thread ID for queued delivery. Codex 0.157.0 did not provide it in MCP request metadata in the isolated live test, so the disposable profile set `CODEX_THREAD_ID` after its session was created. A queued message can be accepted before Codex reads it; the connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process journal; Core intentionally keeps that journal in memory. This proof removes an outbox item on that ACK, so a Core crash immediately afterward can lose that speech text. Production use requires a separate privacy and retention decision and a restart-after-ACK test.

The hosted handoff test runs the existing JS connector, this Rust proof, then JS again against one isolated Core launch. A copied JS speech row has no conversation reference; if Core later remints its binding ID, Rust retains and reports that unattributed row rather than assigning it to another conversation. [Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41) tracks that production migration rule, and [Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43) tracks crash-safe speech retention.

The daemon writes `proof.json` with its PID, executable path and SHA-256, Core launch ID and successful protocol version. The file is evidence of what connected, not an installation record. No build from this directory is an upgrade artifact.
