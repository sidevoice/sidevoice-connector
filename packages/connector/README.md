# Sidevoice uplink — the client side (`@sidevoice/uplink`)

The client side. One bin (`sidevoice`), these entry points:

- `install` — registers the MCP server with the harness (re-pinned to this
  version when an older one was registered) and removes a skill copy an earlier
  version left. Pairs with nothing; reports whether the machine is paired and
  with which room.
- `mcp` — the stdio MCP server a harness starts. One per conversation. Exposes
  `voice_connect`, `voice_pair`, `voice_say`, `voice_disconnect`, `voice_status`; carries the
  operational instructions in its `initialize` result. It never talks to the
  room: it keeps one local connection to the connector for as long as the
  session lives, and the binding it registered dies with that connection.
- `connector` — one per machine, started by the first façade that needs it and
  gone fifteen seconds after the last binding leaves. Holds the outbound link to
  the room, re-announces its bindings on every reconnect, keeps a durable outbox
  for speech published while offline, and delivers one input event at a time per
  binding through the adapter that binding was registered with. A file lock
  makes it a singleton.
- `pair` — redeems, by hand, a pairing code from the room UI for this machine's
  credential (`~/.sidevoice/credentials.json`, mode 0600). The usual path is the
  conversation's `voice_pair`, with the code the user read from the room; nothing
  on the client side ever asks the room for a code.

Harness modules implement one contract (`harness-contract.mjs`): delivery,
inbound inspection, mechanical working state (polled or lifecycle-backed), end-of-turn reporting and session
identity. Every capability is explicitly `supported` or `unsupported`; an old
or malformed declaration becomes `unknown`, never false. Claude Code, Codex and
generic HTTP each have one module. See the repository's
[`docs/HARNESS_CONTRACT.md`](../../docs/HARNESS_CONTRACT.md).

The link is Socket.IO (`link.mjs`), to `/api/connectors/link` in the
namespace `/connectors`, WebSocket transport only. Who this connector is travels
in the connect handshake — `connector_id`, `token`, `protocol`, host and version
— so a credential the room does not know never reaches an event; the room
refuses it with a message that says to pair again, and a refused credential is
not retried. Acknowledgements, keepalive and reconnection with backoff are the
library's. The durable outbox is not: a buffer dies with the process, and speech
a conversation was told was queued must not.

Events: `connector.welcome` after connecting; `binding.register` (carrying the
declared harness capabilities), answered with the binding or `{ error }`;
`binding.unregister`; `input.deliver`, answered with the acknowledgement;
`input.working`; `input.read`; `speech.publish`, answered with what the room did
with it; `binding.close`. Protocol version 2.

Node 22+. The published package has **no runtime dependencies**: `npm run build`
bundles `socket.io-client` and everything else into `dist/cli.mjs` with esbuild,
so installing it copies files and fetches nothing. Tests run on the source:
`node --test test/test_connector.mjs`; the room's `test_connector_interop.py`
runs this connector for real, from the checkout and from the bundle.
