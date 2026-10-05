# Manual checks per harness

These are the checks that need a real agent and a model, so they cannot run unattended. Run them against the bench (`npm run bench`), with each harness set up as the [README](README.md#3-point-a-harness-at-it-isolated-reversible) describes. The setup is isolated: your real agent configuration is never touched.

Record each run in the PR or issue as one line:

```
<harness> <harness version> · connector <git sha> · <date> · pass/fail per item · notes
```

`claude --version`, `codex --version` and `cursor-agent --version` give the harness version.

**Tick a check only when you have seen it in the bench.** The agent's own report that something happened does not count.

## Common to every harness

- [ ] **C1 Join.** Ask the agent to join the voice call (`voice_connect`).
  - A live card appears in *Conversations* with the right harness, title, delivery route and push mode.
  - Frames shows the `binding.register` request and its answer.
- [ ] **C2 Capabilities.** The card's capabilities match the table below. Grey or red is fine where the table says `unsupported`; `unknown` is a finding.
- [ ] **C3 Voice in.** Send a short instruction from the bench, such as "list the files in this folder".
  - The message reaches the agent as a voice message: a JSON header, your words, then the `[Sidevoice]` line.
  - The agent acts on it.
- [ ] **C4 Receipts.** The message's pill moves pending → delivered or unconfirmed, then to **read** once the agent has taken it. Note how long it took.
- [ ] **C5 Voice out.** The agent answers with `voice_say`.
  - The reply appears under your message, answering its revision.
  - *Play* speaks it.
  - In Frames, `speech.publish` carries the message's `session_id` and `revision`.
- [ ] **C6 Working.** While the agent works, the card shows *working*, and *idle* when it stops. Skip this check where the capability is `unsupported`.
- [ ] **C7 Engine.** The card shows the model, and the effort where the harness exposes it.
- [ ] **C8 Interrupt mid-turn.** Send a second message while the agent is working.
  - It queues: the next one is not sent until the first is delivered.
  - It reaches the agent at the harness's next opportunity.
  - It ends as *read*.
- [ ] **C9 Room close.** Press *Close from the room*. The agent's next `voice_status` (ask it) reports the room closed the conversation, and its `voice_say` fails saying so.
- [ ] **C10 Leave.** Rejoin, then ask the agent to leave (`voice_disconnect`). The card turns *closed*, and Frames shows `binding.unregister`.
- [ ] **C11 Connector restart.** Join, press *Stop connector* and then *Start connector*.
  - The binding comes back without any tool call: the façade registers it again.
  - Then send a message (C3, C4).
- [ ] **C12 Speech while Core refuses it.** Set *How Core answers* → `speech.publish` = `error`, then ask the agent to say something.
  - Its `voice_say` reports *queued* rather than failing.
  - Set the policy back to `normal` and press *Drop the link*. The connector replays its outbox when it reconnects (it replays only then), and the reply arrives.
- [ ] **C13 Revoked pairing.** Press *Pairing revoked*.
  - The agent's conversation is closed (`connector_revoked`), and joining again explains why.
  - Press *Room reachable* to restore it.

C11–C13 are also covered by `test/harness-connectivity.test.mjs` with the HTTP fake. By hand, they show what the agent itself says and does.

## Expected capabilities

| Harness | deliver | inspectInbound | working | endOfTurn | sessionIdentity | Delivery route |
|---|---|---|---|---|---|---|
| Claude Code | supported | supported | supported | supported | supported | `claude-uds` |
| Codex | supported | unsupported | supported | supported | supported | `codex-queue` |
| Cursor CLI (persist) | supported | unsupported | supported | supported | supported | `cursor-tmux` (route `cursor-cli-persist`) |
| HTTP | supported | unsupported | unsupported | unsupported | supported | `http` |

## Claude Code

- [ ] **CC1 Messaging socket.** C3 works only if this Claude Code version gives the MCP server `CLAUDE_CODE_MESSAGING_SOCKET`. If `voice_connect` says it cannot tell which conversation this is, record the Claude Code version: that is the finding.
- [ ] **CC2 Inbound check.** Start one session with `--permission-mode bypassPermissions` and no `crossSessionInbound: "accept"` in its settings.
  - `voice_connect` returns `inbound.ok: false` with a reason and a remedy, and the card's *Inbound check* shows the same.
  - A message sent anyway stays *unconfirmed*.
  - Repeat with a prompting permission mode: `inbound.ok: true`, and C4 reaches *read*.
- [ ] **CC3 Unconfirmed, then read.** The socket never acknowledges, so delivery shows *unconfirmed* first. It turns *read* only once the message appears in the session transcript.
- [ ] **CC4 Pull mode** (needs #60 merged). Join with `input: "pull"`.
  - The card shows *pull*, and messages sent from the bench stay *pending* (no `input.deliver` in Frames).
  - `voice_get_messages` returns them, and the receipt becomes *delivered*.
  - Passing them in `ack_ids` makes them *read*.
  - With the PreToolUse hook installed, the next tool call is denied once while there are unfetched messages.

## Codex

- [ ] **CX1 Thread identity.** `voice_connect` succeeds. If it says it cannot tell the conversation, this Codex version does not put the thread id in MCP `_meta` (seen on 0.157.0). Work around it:
  1. Note the thread id (`/status`).
  2. Quit.
  3. Run `CODEX_THREAD_ID=<id> CODEX_HOME=<profile>/codex codex resume <id>`.
  4. Record the version.
- [ ] **CX2 Queue.** C3 arrives as a queued message in the same thread. `codex queue` is run with the profile's `CODEX_HOME`. The connector's process output shows no "Codex queue failed".
- [ ] **CX3 Idle delivery.** Send a message while Codex is idle and waiting for you. Record whether it starts a turn by itself or waits for your next input. That is Codex behaviour, but it decides what the room promises.
- [ ] **CX4 Read only after it is taken.** The pill stays *delivered* until the message shows up in the thread's rollout, then turns *read*.

## Cursor CLI

- [ ] **CU1 Approval.** `cursor-agent mcp list`, run in the scratch project, shows `sidevoice` as loaded after `cursor-agent mcp enable sidevoice`.
- [ ] **CU2 Persist route.** In a chat started with `cursor-agent persist`, the card shows the `cursor-tmux` delivery route and `cursor-cli-persist`, and C3 types the message into that pane.
- [ ] **CU3 Not persisted.** In a plain `cursor-agent` chat, `voice_connect` explains that nothing can put a message into this chat.
- [ ] **CU4 Transcript.** C4 *read* and C6 *working* come from Cursor's transcript. Note any lag.

## Cursor editor (experimental)

This route is not set up by the bench. The editor needs the Sidevoice card open in the chat, which `connector-rust/src/cursor_app.rs` and the card's HTML provide. If you test it:

- [ ] Record the editor version.
- [ ] Record whether the card opened.
- [ ] Run C1–C5.
