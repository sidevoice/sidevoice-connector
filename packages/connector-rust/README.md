# Native Connector control and runtime

The isolated migration candidate replaces the shipped JavaScript control layer with Rust: public CLI, installation and update, rollback, uninstall, launchd/systemd service operations, agent discovery and owned registrations, room/device pairing, and runtime-policy calls. The current accepted beta keeps its existing Node SEA; this candidate does not change its pin. Native migration acceptance requires hosted functional proof, independent functional and simplicity reviews, and a coordinated shipping handoff.

## Native executable

`tools/build-native.py --core-inputs <verified-input-directory> --output <artifact-directory>` builds a single `sidevoice` executable on its native GitHub Actions runner. Rust embeds the complete Core manifest and the matching target archive, checked against `packages/connector/rust-core-production-pin.json`. Installation fetches neither Core nor a language runtime. `--version --json` and `metadata --json` report `format: "rust-native"` and `sea: false`; metadata/progress protocol IDs remain unchanged. A Desktop pin must explicitly support this new format before integration.

The public commands remain `install`, `rollback`, `uninstall`, `service`, `agents`, `pair`, `link-room`, `pair-device`, `skill`, `metadata`, `mcp`, and `connector`. Updating means running `install` from the new executable. The native pair requires Core; `--no-core` is refused. JSON command results stay on stdout and `--progress=jsonl` writes progress to stderr. Pairing uses native HTTP and the existing private credential file; runtime policy does not launch a JavaScript control executable.

## Installed pair

| Path | Program or data |
| --- | --- |
| `<release>/dist/sidevoice` | Native public CLI |
| `<release>/dist/sidevoice-rust` | Same native executable under the existing daemon/MCP path |
| `<release>/core/bin/sidevoice-core-rust` | Native Core |
| `<release>/core/models`, `checks`, `lib`, `notices` | Verified Core archive contents |
| `<release>/release.json` | Distributor/runtime/Core/pair identity |

`current`, `previous`, and `verified` remain the only selection pointers. New native releases retain `runtime_kind: "rust-native-v1"`, `core_kind: "rust-native-v1"`, separate byte digests, and the established pair ID. `format: "rust-native"` distinguishes the distributor from legacy `sea` and `esm` selections. Existing legacy releases are recognized for safe rollback and owned-registration cleanup; Node is not embedded in the new release.

The existing npm/SEA build scripts and legacy runtime source remain for comparison and compatibility fixtures. They are **not** the native shipment. The native migration branch disables their automatic publication and npm packaging. Those guards must be integrated together with the parent-owned native distribution and Desktop handoff; registry/publication design remains separate.

## Validation

`rust-control.yml` is an isolated migration gate. Native compilation and tests run only in GitHub Actions. `tools/core-proof-inputs.py` verifies and reuses the exact accepted Core artifact for migration tests while it is available; it is an expiring test input, not a production download mechanism. The installed pairing fixture runs against a disposable profile and local room, never an operator installation.

## Isolated proof mode

Create a fresh `0700` profile root `$P` with private `home`, `claude`, `codex`, `cursor` and `sidevoice` directories, plus `sidevoice/core`. Launch a v3-enabled Core separately with `--data-dir $P/sidevoice/core --socket $P/sidevoice/core/local.sock`. Start this binary with `connector --profile-root $P`. Register MCP in the disposable Codex profile with `codex mcp add sidevoice -- /absolute/path/sidevoice-rust-proof mcp --profile-root $P`. The profile-root argument reconstructs all private paths in a fresh process without inherited profile variables.

The authenticated Core v3 host API serves `agents.list`, `agents.connect`, `agents.disconnect` and `agents.dismiss` for Claude Code, Codex and Cursor inside this profile. Claude and Codex own their registration changes through their CLIs. Cursor changes only `cursor/mcp.json`, preserving other entries and safe in-profile symlinks. Host-agent work is serialized per request, bounded, and cancelled children are reaped before another request proceeds. The standalone `codex inspect` and `codex connect` commands use that same coordinator and hold the profile lock; they never inspect or modify the default Codex profile.

The proof needs the active Codex thread ID for queued delivery. Codex 0.157.0 did not provide it in MCP request metadata in the isolated live test, so the disposable profile set `CODEX_THREAD_ID` after its session was created. A queued message can be accepted before Codex reads it; the connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process journal; Core intentionally keeps that journal in memory. The current implementation removes an outbox item on that ACK, so a Core crash immediately afterward can lose that speech text. The installed-pair slice preserves this baseline behavior; the separate runtime-promotion gate requires an empty outbox before switching ownership from JavaScript to Rust.

Hosted proof includes the JS agent compatibility suite, the actual Codex CLI in a disposable profile, and the JS→Rust→JS handoff against authenticated Core v3 host routes. A copied JS speech row has no conversation reference; if Core later remints its binding ID, Rust retains and reports that unattributed row rather than assigning it to another conversation. [Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41) tracks that production migration rule, and [Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43) tracks crash-safe speech retention.

In proof mode, the daemon writes `proof.json` with its PID, executable path and SHA-256, Core launch ID and successful protocol version. The file is evidence of what connected, not an installation record. A development build without the required native payload is not an upgrade artifact; the native builder above supplies and verifies that payload.
