# Sidevoice connector bench

The bench is a fake Sidevoice **Core**: it speaks Core↔Connector protocol v3 on a private profile's socket. The **real** Rust connector links to it unchanged. A web UI then lets you play the room:

- send what you would say, as text, to any conversation;
- watch receipts, the agent's spoken replies, bindings, capabilities, push/pull mode, working state and the raw protocol frames;
- send the connector every command Core can send it;
- make Core misbehave on purpose.

Who owns what, and why the bench uses a fake Core rather than the real one: [`docs/TESTING-SEAMS.md`](../../docs/TESTING-SEAMS.md).

## 1. Build the connector from source

You need:
- Node.js 22 or newer;
- a Rust toolchain ([rustup](https://rustup.rs));
- a C linker. On macOS, `xcode-select --install`. On Linux, `cc`/`gcc`.

```sh
npm ci                                                   # from the repository root
(cd packages/connector-rust && cargo build --release)    # → packages/connector-rust/target/release/sidevoice-rust-proof
```

The bench picks up whichever of `target/release` or `target/debug` is newer. Use `--connector <path>` to point it at any other build.

## 2. Start the bench

```sh
npm run bench                  # http://127.0.0.1:4477/
npm run bench -- --help        # --port, --profile <dir>, --connector <binary>, --no-connector, --reset
```

When it starts, the bench:

1. **Creates a private profile** at `~/.sidevoice-bench/profile`. Every directory in it is mode 0700, with `sidevoice/`, `claude/`, `codex/`, `cursor/` and so on inside.
2. **Starts the fake Core** in `sidevoice/core/`, with its `core.json`, `local.sock` and health endpoint.
3. **Runs `sidevoice-rust-proof --profile-root <profile> connector` against it.** The connector exits on its own 15 s after its last conversation leaves, so the bench starts it again.
4. **Prints the commands that point each harness at it.** The UI's *Point a harness at this bench* panel shows the same commands.

Nothing outside the profile is created or changed, and your real `~/.sidevoice` is not touched. A real Sidevoice installation can keep running alongside. `rm -rf ~/.sidevoice-bench` (or `--reset`) removes everything.

## 3. Point a harness at it (isolated, reversible)

| Harness | One-time setup | Start | Undo |
|---|---|---|---|
| Claude Code | None: the bench writes `<profile>/bench/claude-mcp.json` | `claude --mcp-config <profile>/bench/claude-mcp.json --strict-mcp-config` | Nothing to undo |
| Codex | `CODEX_HOME=<profile>/codex codex login`<br>`CODEX_HOME=<profile>/codex codex mcp add sidevoice -- <binary> --profile-root <profile> mcp` | `CODEX_HOME=<profile>/codex codex` | `CODEX_HOME=<profile>/codex codex mcp remove sidevoice` (or remove the profile) |
| Cursor CLI | `cd <profile>/bench/cursor-project && cursor-agent mcp enable sidevoice` (the bench writes the project's `.cursor/mcp.json`; needs tmux) | `cd <profile>/bench/cursor-project && cursor-agent persist` | `cursor-agent mcp disable sidevoice` in that directory |
| Anything else | Start `<binary> --profile-root <profile> mcp` as its MCP server, with `SIDEVOICE_THREAD=<id>` and `SIDEVOICE_DELIVERY_URL=<your receiver>` | | |

What each harness setup isolates:

- **Claude Code.** `--strict-mcp-config` loads only the bench's server, so a real Sidevoice registration does not join the session. Claude Code still uses your normal login and settings, and it never writes them.
- **Codex.** It runs entirely inside the profile's `CODEX_HOME`, which is the same home the connector hands to `codex queue`. Its login token lives in that home and goes away with the profile.
- **Cursor.** The server is project-scoped. If your global `~/.cursor/mcp.json` also has a `sidevoice` entry, check with `cursor-agent mcp list` which one is loaded before you trust a result.

Once a harness is running, ask the agent to *join the voice call*. The conversation appears in the bench, and what you type there arrives in the agent as a voice message.

The exact path of `<profile>` and `<binary>` is printed by `npm run bench`; copy the commands from there.

## 4. Automated checks

```sh
npm run test:bench
```

The suite takes a few seconds and needs no model calls and no real agent. It has two parts:

- **`test/fake-core.test.mjs`: protocol v3 as the fake Core serves it.** Each case is the behaviour of sidevoice-core at 0a9da38:
  - discovery;
  - the handshake and its refusals;
  - binding validation;
  - push delivery and the receipts it produces;
  - retries, and one message in flight per thread;
  - speech admission;
  - pull claims and acknowledgements;
  - answer policies.
- **`test/harness-connectivity.test.mjs`: the real connector binary.** It runs both `mcp` and `connector` against the fake Core, with each harness's delivery route faked (`test/harness-fakes.mjs`):

  | Route | What is checked |
  |---|---|
  | link | `proof.json` reports protocol 3; `node.status` answers. |
  | http | `SIDEVOICE_DELIVERY_URL` gets the voice message; the declared capabilities match; the reply is published; leaving unregisters. |
  | claude-uds | The messaging socket gets `auth` and then the envelope; the transcript turns it into *read*; the session registry drives *working*; the reply and leaving as above. |
  | codex-queue | `codex queue --thread` runs with the profile's `CODEX_HOME`; the rollout turns it into *read*, *working* true then false, and the engine; the reply and leaving as above. |
  | outbox | speech Core refuses stays queued on disk and is replayed when the link comes back. |
  | connector restart | the façade registers its conversation again by itself, and delivery resumes. |
  | revoked pairing | `node.rendezvous` refused closes the conversation; `voice_status` says so. |
  | room close | `binding.close` from Core reaches the agent's `voice_status`. |

  Without a built binary these tests are **skipped** with a reason. `BENCH_REQUIRE_CONNECTOR=1` (set in CI) makes a missing binary a failure instead, and `SIDEVOICE_BENCH_CONNECTOR=<path>` picks a specific binary.

The automated tests prove the connector's half of each route against the artefact formats it expects. The harness's half can only be shown with the real agent: [`MANUAL-CHECKS.md`](MANUAL-CHECKS.md).

## 5. What the UI does

| Panel | What you do and see there |
|---|---|
| **Conversations** | One card per binding: harness, push/pull, live or closed, working, capabilities (an `*` marks experimental), delivery route, Cursor route, engine, last turn, inbound check. Click a card to talk to it. *Close from the room* sends `binding.close`. |
| **Conversation** | Type and send (Enter). Each message shows its receipt as it moves (pending → delivered or unconfirmed → read, or not sent), with the history in its tooltip. The agent's replies show with *Play* (browser speech synthesis); *Speak replies aloud* plays new ones as they arrive. |
| **Core → connector** | Host agents: `agents.list`, `agents.connect`, `agents.disconnect`, `agents.dismiss`. Also `node.status`, `pair.request`, *Drop the link* (as a Core restart would; the connector reconnects and replays its outbox), and the room link (`node.rendezvous`: reachable, unreachable, revoked). |
| **How Core answers** | For `binding.register`, `speech.publish`, `input.pull` and `device.pairing_code`, choose `normal`, `refuse`, JSON-RPC `error`, `silent` (never answers), `text_only`, `unknown_binding` or `rejected`. Use this to watch the connector's outbox, retries and refusals. |
| **Raw JSON-RPC** | Send any request, notification or verbatim frame. |
| **Protocol frames** | Every frame in both directions, live. Filter it, hide the handshake (on by default, because the connector reconnects each time the bench restarts it), or pause. `GET /api/frames` returns the same log as JSON. |
| **Connector process output** | The connector's stderr and the bench's start and exit lines. |

The UI listens on 127.0.0.1 only. It answers only to its own `Host`, and accepts actions only from its own page.

## Layout

```
bench.mjs              CLI: profile, fake Core, connector supervisor, UI server (HTTP + SSE)
lib/fake-core.mjs      the fake Core (protocol v3, journal, receipts, policies, frame log)
lib/connector.mjs      finds and supervises the Rust connector; cleanEnv() drops inherited harness variables
lib/profile.mjs        the private profile layout the connector expects with --profile-root
lib/harness-setup.mjs  per-harness isolated setup commands and files
lib/mcp-client.mjs     a minimal stdio MCP client (the harness side in tests)
lib/i18n.mjs           message bundles (messages/en.json is the fallback; es.json)
public/                the UI (no build step)
test/                  protocol tests, harness fakes, connectivity tests
```

User-facing text, in the UI and in the CLI, comes from `messages/<lang>.json`. Add every new key to `en.json` (see [`AGENTS.md`](../../AGENTS.md)).
