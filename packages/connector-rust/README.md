# Rust Connector proof and installed runtime

This package has two modes. The isolated proof mode remains separate from the npm package and release selection. A target SEA built with the trusted Rust Core and Rust Connector inputs can also install the experimental `rust-native-v1` pair: the existing JavaScript SEA stays the user-facing control and installer, while the Rust binary serves MCP and the local daemon and the Rust Core runs from the same selected release. This branch has not been selected for beta or production. The final Rust Connector promotion and Desktop switch remain separate gates.

## Installed pair

The trusted target build verifies the protected-main Core T7 manifest and target archive, verifies the exact Rust Connector target binary and its build identity, then embeds both assets in the existing target SEA. Installation downloads neither native binary. It extracts the Core archive root `sidevoice-core-rust/` into `<release>/core`, producing these fixed paths:

| Path | Selected program or data |
| --- | --- |
| `<release>/core/bin/sidevoice-core-rust` | Native Core entrypoint |
| `<release>/core/models` | Core models |
| `<release>/core/checks` | Core checks |
| `<release>/core/lib` | Core libraries |
| `<release>/dist/sidevoice` | JavaScript installer and control SEA |
| `<release>/dist/sidevoice-rust` | Rust daemon and MCP executable |
| `<release>/release.json` | Closed release and pair identity record |

`release.json` keeps the distributor, runtime and Core identities distinct. The intermediate pair uses `runtime_kind: "rust-native-v1"`, `core_kind: "rust-native-v1"`, and a `pair_id` of `pair-v1:rust-native-v1:<runtime_sha256>:core:rust-native-v1-<target>-<core_source_sha>-<core_archive_sha256>`. Runtime fields are `runtime_build_sha`, `runtime_sha256`, `runtime_size`, and `runtime_target`; `distributor_sha256` and `distributor_size` identify the JavaScript control SEA. The Core fields are `core_source_sha`, `core_cargo_lock_sha256`, `core_manifest_sha256`, `core_archive_sha256`, `core_archive_size`, `core_target`, and `core_entrypoint` (`bin/sidevoice-core-rust`). The selected Rust process revalidates the release record, paths, executable digests, target and pair identity at startup. A runtime-kind or pair change remains an upgrade even when the package version is unchanged.

The existing `current`, `previous` and `verified` release links are the only selection and rollback mechanism. Installation stages and self-tests the embedded Core and Rust Connector before changing `current`; failed readiness uses the existing rollback path. A JavaScript-to-Rust runtime-kind change requires a valid empty `outbox.json` before staging and again under the install lock immediately before commit. It refuses malformed, unreadable or nonempty outboxes without switching the release. Same-runtime upgrades retain the existing speech and acknowledgement behavior.

The installed Rust MCP command is `<release>/dist/sidevoice-rust --installed mcp`; the daemon is `<release>/dist/sidevoice-rust --installed connector`. Harness registration and both service-manager definitions select those commands through `current`. The Node SEA retains install, service administration, pairing CLI and updates. Rust `voice_pair` invokes the fixed selected `<release>/dist/sidevoice pair --json -- <room> <exact-user-code>` path directly, with no shell, and maps its JSON success/error result to the MCP result. Pairing is refused in isolated proof mode. `voice_pair_device` continues over the existing local Core route. Installed Rust identity replies include `runtime_kind`, `runtime_build_sha`, `runtime_sha256`, `runtime_target` and `release_id`; service readiness and install verification require those fields to match the selected release.

The trusted `rust-core-consumer.yml` workflow fetches and verifies only the fixed protected-main Core run in its secret-bearing job. The candidate job receives the same-run verified bytes, checks exact membership and pins again, builds the matching Rust target, packages the SEA, and runs offline stage/self-test coverage for macOS arm64, Linux x86_64 and Linux arm64. Mac acceptance additionally installs the pair into a disposable profile under real launchd, invokes Rust `voice_pair` against a local test room, and checks the running native Core authenticates with the newly written credential. The workflow fails closed unless `SIDEVOICE_CORE_ACTIONS_READ` has Actions:read access to the Core repository; the Connector secret is currently absent, so no signed-positive consumer run or beta selection is established.

## Isolated proof mode

Create a fresh `0700` profile root `$P` with private `home`, `claude`, `codex`, `cursor` and `sidevoice` directories, plus `sidevoice/core`. Launch a v3-enabled Core separately with `--data-dir $P/sidevoice/core --socket $P/sidevoice/core/local.sock`. Start this binary with `connector --profile-root $P`. Register MCP in the disposable Codex profile with `codex mcp add sidevoice -- /absolute/path/sidevoice-rust-proof mcp --profile-root $P`. The profile-root argument reconstructs all private paths in a fresh process without inherited profile variables.

The authenticated Core v3 host API serves `agents.list`, `agents.connect`, `agents.disconnect` and `agents.dismiss` for Claude Code, Codex and Cursor inside this profile. Claude and Codex own their registration changes through their CLIs. Cursor changes only `cursor/mcp.json`, preserving other entries and safe in-profile symlinks. Host-agent work is serialized per request, bounded, and cancelled children are reaped before another request proceeds. The standalone `codex inspect` and `codex connect` commands use that same coordinator and hold the profile lock; they never inspect or modify the default Codex profile.

The proof needs the active Codex thread ID for queued delivery. Codex 0.157.0 did not provide it in MCP request metadata in the isolated live test, so the disposable profile set `CODEX_THREAD_ID` after its session was created. A queued message can be accepted before Codex reads it; the connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process journal; Core intentionally keeps that journal in memory. The current implementation removes an outbox item on that ACK, so a Core crash immediately afterward can lose that speech text. The installed-pair slice preserves this baseline behavior; the separate runtime-promotion gate requires an empty outbox before switching ownership from JavaScript to Rust.

Hosted proof includes the JS agent compatibility suite, the actual Codex CLI in a disposable profile, and the JS→Rust→JS handoff against authenticated Core v3 host routes. A copied JS speech row has no conversation reference; if Core later remints its binding ID, Rust retains and reports that unattributed row rather than assigning it to another conversation. [Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41) tracks that production migration rule, and [Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43) tracks crash-safe speech retention.

In proof mode, the daemon writes `proof.json` with its PID, executable path and SHA-256, Core launch ID and successful protocol version. The file is evidence of what connected, not an installation record. No build from this directory is an upgrade artifact.
