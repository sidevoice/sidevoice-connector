# Sidevoice uplink — the client side (`@sidevoice/uplink`)

The client side. Three entry points behind one bin (`sidevoice`):

- `mcp` — the stdio MCP server a harness starts. One per conversation. Exposes
  `voice_connect`, `voice_say`, `voice_disconnect`, `voice_status`; carries the
  operational instructions in its `initialize` result. It never talks to the
  room: it keeps one local connection to the connector for as long as the
  session lives, and the binding it registered dies with that connection.
- `connector` — one per machine, started by the first façade that needs it and
  gone fifteen seconds after the last binding leaves. Holds the outbound
  WebSocket to the room, re-announces its bindings after a reconnect, keeps a
  durable outbox for speech published while offline, answers the room's
  heartbeat, and delivers one input event at a time per binding through the
  adapter that binding was registered with. A file lock makes it a singleton.
- `pair` — redeems a pairing code from the room UI for this machine's
  credential (`~/.sidevoice/credentials.json`, mode 0600).

Adapters (`adapters.mjs`): `claude-uds` writes the voice envelope as a user
message to the Claude Code session's inbox socket; `codex-queue` runs
`codex queue --thread <id>`; `http` posts to any local receiver that accepts the
room's message shape (for harnesses that have neither).

Protocol (newline-free JSON over the WebSocket): `connector.hello` ->
`connector.welcome`; `binding.register` -> `binding.registered|rejected`;
`binding.unregister`; `input.deliver` -> `input.ack`; `speech.publish` ->
`speech.published`; `heartbeat` <-> `heartbeat.ack`. Protocol version 1.

Node 22+, no dependencies. Tests: `node --test test/test_connector.mjs`.
