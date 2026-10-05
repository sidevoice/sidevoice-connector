# Who owns what, and where each piece can be faked

Testing strategy for Sidevoice ([#62](https://github.com/sidevoice/sidevoice-connector/issues/62)):

1. **Prove each harness works with the connector.** Agent connectivity is the hard part.
2. **Fake everything outside the connector.**
3. **Add contracts and drift checks later**, where the fakes meet the real pieces.

This page describes the code as it stood on 2026-10-05:
- connector `main` at 435fd31;
- sidevoice-core `origin/main` at 0a9da38;
- sidevoice-desktop `origin/main` at 4f4d320.

The file paths named below will move with sidevoice-core#57. The wire will not.

## The pieces

```
 agent harness (Claude Code / Codex / Cursor / any)            device (desktop app, browser)
   │ stdio MCP              ▲ delivery route                          │ HTTP + WebSocket (device token)
   ▼                        │ (UDS, codex queue, tmux, HTTP)          ▼
 ┌──────────────┐  IPC   ┌─────────────────┐  protocol v3   ┌──────────────────────────────┐
 │ mcp (façade) │──────▶│ connector daemon │◀──────────────▶│ Core (conversations, voice,  │
 │ one per conv.│ JSON  │ one per machine  │ JSON-RPC over  │ devices, rendezvous to room) │
 └──────────────┘ lines └─────────────────┘ WS on Unix sock└──────────────────────────────┘
                         connector.sock                       core/local.sock + core.json
```

| Piece | Owns | Does not own |
|---|---|---|
| **Connector**: this repo. The Rust `sidevoice-rust-proof` is the runtime that serves `mcp` and `connector`. The JS `@sidevoice/uplink` is the installer, service control and pairing CLI, and runs its own v2 runtime. | <ul><li>The MCP surface an agent sees (`voice_*` tools, instructions).</li><li>Which conversation it is in (harness adapters: identity, delivery route, capabilities).</li><li>Delivering input into the harness and observing read, working and engine state from harness artefacts.</li><li>The durable speech outbox.</li><li>Host-agent registration (`agents.*`).</li><li>Install, service jobs and rollback.</li></ul> | <ul><li>Message text after delivery.</li><li>Calls and audio.</li><li>Devices.</li><li>The room link.</li></ul> |
| **Core**: sidevoice-core | <ul><li>Conversations, bindings and the input journal (push delivery with retries, pull with claims).</li><li>Receipts (`pending`, `delivered`, `read`, `not_sent`, `unconfirmed`).</li><li>Speech admission and its queue.</li><li>The voice pipeline (VAD, STT, TTS).</li><li>Devices and pairing codes.</li><li>The rendezvous with the hosted room.</li></ul> | <ul><li>Any harness.</li><li>Anything an agent runs.</li></ul> |
| **App**: sidevoice-desktop, plus sidevoice-web inside it | <ul><li>The call UI: microphone, playback and selecting a conversation.</li><li>Pairing itself as the local device.</li><li>Driving the connector CLI (`service`, `pair-device`, `pair`, `install --json`, `agents --json`).</li><li>Watching `node.status` on `connector.sock`.</li></ul> | <ul><li>Conversations.</li><li>Delivery.</li><li>Agents' configuration (it asks the connector).</li></ul> |

## The seams

### 1. Harness ↔ MCP façade (`mcp`, stdio)

- **Wire.** MCP over stdio. The tools are `voice_connect`, `voice_say`, `voice_disconnect`, `voice_status`, `voice_pair` and `voice_pair_device`; [#60](https://github.com/sidevoice/sidevoice-connector/pull/60) adds `voice_has_pending` and `voice_get_messages`. The server's `initialize` result carries the operating instructions.
- **Identity.** The façade works out which conversation it is in from the environment the harness gives it (`connector-rust/src/adapters/*`):
  - Claude Code: `CLAUDE_CODE_SESSION_ID` and `CLAUDE_CODE_MESSAGING_SOCKET`.
  - Codex: the thread id in MCP `_meta`, or else `CODEX_THREAD_ID`.
  - Cursor: the MCP client name, the chat id and tmux.
  - Generic: `SIDEVOICE_THREAD` and `SIDEVOICE_DELIVERY_URL`.
- **Faked by** `packages/bench/lib/mcp-client.mjs`. It stands in for the harness's MCP client and sets those variables itself.
- **Real check.** A real harness, by hand. See the [manual checklist](../packages/bench/MANUAL-CHECKS.md).

### 2. Façade ↔ connector daemon (`connector.sock`, JSON lines)

- **Wire.** Private to the connector. The methods are `register`, `publish`, `unregister`, `status`, `identity`, `adopt`, `pair_device`, `node.status` and `shutdown`; #60 adds `pull` and `pull_check`.
- **Real client.** The desktop app reads only `node.status` here. The Rust Core's T3 test drives `register` and `publish`.
- **Not faked.** The bench runs the real façade and the real daemon together, so this seam stays inside the unit under test.

### 3. Connector daemon ↔ harness (delivery routes and harness artefacts)

| Harness | Input goes in by | Read and working come from | Fake (`packages/bench/test/harness-fakes.mjs`) |
|---|---|---|---|
| Claude Code (`claude-uds`) | The messaging socket: an `auth` line, then a `user` line. It sends no acknowledgement, so the result is `unknown`, which Core shows as unconfirmed. | <ul><li>`$CLAUDE_CONFIG_DIR/projects/*/<session>.jsonl`: the user entry carries the envelope header.</li><li>`sessions/*.json` `status`: busy means working, idle means not working.</li></ul> | `claudeFake`: a socket that appends to the transcript, and a registry entry whose status the test sets. |
| Codex (`codex-queue`) | `codex queue --thread T --message <envelope>`, run with the profile's `CODEX_HOME`. | The rollout `CODEX_HOME/sessions/Y/M/D/*-T.jsonl`: <ul><li>`task_started` and `task_complete` give working state;</li><li>the user `response_item` carries the envelope;</li><li>`session_meta.model` gives the engine.</li></ul> | `codexFake`: a `codex` executable (`SIDEVOICE_CODEX_BIN`) that records the call and writes the turn into the rollout. |
| Generic (`http`) | `POST $SIDEVOICE_DELIVERY_URL` with `{thread_id, text, message_id, session_id, revision, channel}`. | Nothing. Capabilities declare `working`, `endOfTurn` and `inspectInbound` as `unsupported`. | `httpReceiver`. |
| Cursor CLI (`cursor-tmux`) and editor (`cursor-app`) | `tmux send-keys` into a `cursor-agent persist` pane, or the editor bridge. | Cursor transcripts under `CURSOR_DATA_DIR`. | Not faked yet: tmux plus Cursor's on-disk layout. This route is checked by hand. |

The fakes copy the artefact formats the connector reads today. **Drift risk:** if Claude Code or Codex change those formats, the fakes still pass. That is exactly what the manual checks and a later recorded-artefact contract test exist to catch.

### 4. Connector daemon ↔ Core (protocol v3)

**Wire.** It is defined by sidevoice-core `rust/server/connectors_v3.rs` and `rust/control/room.rs`.

1. The connector finds Core through `D/core/core.json` (mode 0600): `{pid, launch_id, socket, connector_id, token, connector_protocols:[…3]}`.
2. It checks Core's identity with `GET /api/local/health` on `D/core/local.sock`.
3. It opens a WebSocket at `/api/connectors/v3` on that socket. Frames are JSON-RPC 2.0 text, each up to 1 MiB.
4. The first frame is `connector.hello {protocol:3, connector_id, token, host, platform, version, harnesses}`. Core answers with `{protocol:3}`, then sends `connector.welcome` and then `node.rendezvous`.

Messages in each direction:

| Direction | Requests | Notifications |
|---|---|---|
| Connector → Core | `binding.register`, `speech.publish`, `input.pull`, `device.pairing_code` | `binding.unregister`, `input.working`, `input.engine`, `input.read` |
| Core → Connector | `input.deliver` (60 s), `agents.list`, `agents.connect`, `agents.disconnect`, `agents.dismiss`, `pair.request`, `node.status` | `binding.close`, `node.rendezvous` |

**Faked by** `packages/bench/lib/fake-core.mjs`. Its scope:

- **The wire:** framing, refusals, close codes and capacity limits, as `connectors_v3.rs` has them.
- **What Core does, only as far as a connector can observe it:**
  - bindings, given out as `register` does: a known id is reused, otherwise the connector's active binding on the thread, otherwise a new id; the newest active, live binding by creation is the thread's delivery binding;
  - the journal: one delivery in flight per binding, acknowledgements checked as `valid_delivery_ack` checks them, the 2/5/15/60 s retries, the 600 s TTL, and redelivery at once when a link drops;
  - pull claims and acknowledgements;
  - receipts;
  - speech admission and its de-duplication by `utterance_id`.
- **Policies** let it answer the way a degraded Core would: refuse, a JSON-RPC error, no answer at all, `unknown_binding`, `text_only` or `rejected`.

- **A Core restart** (*Restart Core*): a new launch id, and every binding forgotten, as real Core keeps them in memory only.

**Not modelled:**
- calls, focus and audience (speech is `queued` unless a policy says otherwise);
- more than one connector: the bench issues one credential, so `room.binding_foreign` and `room.pull_binding_superseded` cannot happen;
- the journal surviving a restart: real Core loses it; the bench keeps the rows for display and marks what was waiting as not sent;
- devices;
- the hosted room.

`node.rendezvous` is set by hand.

### 5. App ↔ Core (device API) and App ↔ connector CLI

- **Wire.** Core's HTTP and WebSocket device API (`/api/presentation/*`, `/api/presentation/ws`, `/api/device/*`), behind a device token. The app also runs the connector CLI with `--json`.
- **Not part of the connector's tests.** The bench stands in for the room UI by driving Core's *connector* side directly. Sending text through the bench is what `POST /api/presentation/text` followed by `input.deliver` would do in real Core. The difference is that the bench needs no live call: real Core accepts typed text only when a browser call session (and therefore its VAD model) is focused on the thread.
- **Faked by** nothing here. The app's own repos own this seam.

## Why a fake Core, and where contracts plug in later

The real Rust Core does link the real connector: sidevoice-core `rust-t3.yml` and connector `rust-proof.yml` both run it. It was still a poor fit for this bench, for four reasons:

1. **It has no typed-input path without a call.** Text input needs a WebSocket call session, which starts the Silero VAD (ONNX).
2. **It needs a native toolchain** (libopus, pkg-config and a C toolchain) to build from source.
3. **It is being restructured** under sidevoice-core#57.
4. **It cannot be made to misbehave**, so the connector's failure paths cannot be tested against it.

The fake answers each of these: it runs in milliseconds, on Node with only `ws`, and its answer policies are what test the failure paths.

The fake's fidelity is still a risk, and these are the planned checks against drift (follow-ups, not yet built):

- **Contract check against real Core.** Replay the bench's recorded v3 transcripts (`GET /api/frames` gives the raw frames) against the real Rust Core in Core's CI, and fail on any difference in shape or refusal. The fake-Core unit tests (`test/fake-core.test.mjs`) already state each expectation with its Core source; they are the list to turn into shared fixtures.
- **Harness artefact drift.** Record real Claude Code and Codex transcripts and rollouts from a manual run (`MANUAL-CHECKS.md`), commit them as fixtures, and feed them to the connector's watchers.
- **The JS connector (protocol v2, Socket.IO).** It is out of scope for this bench. A v2 face on the same fake Core state is the natural extension if the JS runtime is kept.
