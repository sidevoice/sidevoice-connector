# Sidevoice uplink — the client side (`@sidevoice/uplink`)

The client side. One bin (`sidevoice`), these entry points:

- `install` — one transaction (`install-txn.mjs`): stages this version and the
  core (uv: Python and the pinned wheel, progress shown), runs the core's
  `--self-test`, waits for calls to end (`--apply-now` does not), commits behind a
  journal so a killed installer is recovered, restarts the node and rolls back if
  it does not come up. A same or older version is a no-op. Then the node service
  (`--no-service` opts out) and the harnesses: each one found, `--harness <id>`, or
  none with `--no-agents`. `--no-core` leaves the core for later; `--json` prints
  one object for the app. Pairs with nothing; reports whether the machine is paired.
- `service install|uninstall|start|stop|restart|status [--json]` — the node
  service: a LaunchAgent (`dev.sidevoice.node`, macOS) or a systemd user unit
  (`sidevoice-node.service`, Linux) running `connector --supervise`; where there is
  no user service manager (containers, pods), a detached supervisor. `stop` is a
  person's stop: nothing starts the node again until `service start` or the next
  login. On Linux, `install` prints the `loginctl enable-linger` command that would
  keep it running without a session, and never runs it.
- `uninstall` — the node service, then our harness registrations, then the
  copies, then `~/.sidevoice`; it stops before deleting anything if the service
  manager will not unload the service.
- `mcp` — the stdio MCP server a harness starts. One per conversation. Exposes
  `voice_connect`, `voice_pair`, `voice_say`, `voice_disconnect`, `voice_pair_device`,
  `voice_status`; carries the
  operational instructions in its `initialize` result. It never talks to the
  room: it keeps one local connection to the connector for as long as the
  session lives, and the binding it registered dies with that connection.
- `connector [--supervise]` — one per machine. With `--supervise` it is the node
  service: the core is its child (`--idle-exit 0`, a launch id per start), judged
  ready by its health on its socket, restarted with backoff within a budget
  (`supervisor.mjs`), and reported by `node.status`. Without, it is what the
  launcher (`launcher.mjs`, the only way anything gets a connector) starts where
  no service is installed: the core detached, and itself gone fifteen seconds after
  the last binding leaves; a supervisor finding one takes its bindings over whole
  (`handover`). Either installs (with `uv`, at the pinned `CORE_VERSION`) and runs
  this machine's **core** (`sidevoice/sidevoice-core`, `core.mjs`), holds the link to
  it over the core's own socket (`core-socket.mjs`),
  re-announces its bindings on every reconnect, keeps a durable outbox
  for speech published while offline, and delivers one input event at a time per
  binding through the adapter that binding was registered with. A file lock
  makes it a singleton.
- `pair <room-url> <code> [--json]` — redeems, by hand, a pairing code from the room UI for
  this machine's credential (`~/.sidevoice/credentials.json`, mode 0600), the one its
  core links with the room by. The conversation's path is `voice_pair`, with the code
  the user read from the room; the conversation never asks the room for a code.
  The running connector then restarts the core with it (`node.restart`).
- `link-room <room-url>` — the same without a page: asks the room for a code
  (`POST /api/connectors/pairing-code`, naming the room as the origin) and redeems it.
  Registering with a room is open: a room is a relay and grants nothing by itself.
- `pair-device [--json]` — prints a one-time code (and a QR of it) that pairs a device — the
  desktop app, a browser — with this machine, starting the core if needed; the same
  as the conversation's `voice_pair_device`. The core issues it and keeps the devices
  (sidevoice-core's `server/devices.py`); the person pastes it in
  the app under Máquinas → Emparejar. `--json` adds its `reach`: `room`, `direct`
  or `local-only`.

Harness modules implement one contract (`harness-contract.mjs`): delivery,
inbound inspection, mechanical working state (polled or lifecycle-backed), end-of-turn reporting and session
identity. Every capability is explicitly `supported` or `unsupported`; an old
or malformed declaration becomes `unknown`, never false. Claude Code, Codex, the
Cursor CLI and generic HTTP each have one module (`harness-*.mjs`).

The conversations live in the core, not in the hosted room (the core's side of
the room is sidevoice-core's `server/rendezvous.py`). The connector links to the
core over its Unix socket, `~/.sidevoice/core/local.sock` — in a directory only this
OS user can enter, checked before every dial — with the credential the core wrote in
`~/.sidevoice/core/core.json`;
the core dials the room with this machine's pairing and tells the connector how
that goes (`node.rendezvous`: reachable, or refused because the pairing was
revoked). `SIDEVOICE_URL` + `SIDEVOICE_CONNECTOR_ID` + `SIDEVOICE_CONNECTOR_TOKEN`
name a core somebody else runs instead (a checkout's, a test's).

The link is Socket.IO (`link.mjs`), to `/api/connectors/link` in the
namespace `/connectors`, WebSocket transport only. Who this connector is travels
in the connect handshake — `connector_id`, `token`, `protocol`, host and version
— so a credential the core does not know never reaches an event, and a refused
credential is not retried. Acknowledgements, keepalive and reconnection with backoff are the
library's. The durable outbox is not: a buffer dies with the process, and speech
a conversation was told was queued must not.

Events: `connector.welcome` after connecting; `binding.register` (carrying the
declared harness capabilities), answered with the binding or `{ error }`;
`binding.unregister`; `input.deliver`, answered with the acknowledgement;
`input.working`; `input.read`; `speech.publish`, answered with what the core did
with it; `binding.close`; `node.rendezvous`; `pair.request`, answered with the
pairing's outcome; `device.pairing_code`, answered with `{code, payload, expires_in}`.
Protocol version 2 on this link.

Node 22+. The published package has **no runtime dependencies**: `npm run build`
bundles `socket.io-client`, `qrcode` and everything else into `dist/cli.mjs` with esbuild,
so installing it copies files and fetches nothing. Tests run on the source:
`node --test test/test_connector.mjs` (`test_core.mjs` covers the install and
supervision against a fake `uv` and a fake core, `test_supervisor.mjs` the state
machine on a fake clock, `test_node.mjs` the handover, the launcher and the service
commands against stand-ins of launchd and systemd, `test_install.mjs` the install
transaction; `test/service-integration.mjs` runs the real service managers in CI); sidevoice-core's
`test_connector_interop.py` runs this connector for real against the core, from
the checkout and from the bundle. `SIDEVOICE_CORE_WHEEL=<wheel> npm run build`
puts the pinned core's wheel inside `dist/core/`, so the published package
installs it without any index.
