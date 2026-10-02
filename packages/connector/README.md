# Sidevoice uplink — the client side (`@sidevoice/uplink`)

The client side. One bin (`sidevoice`), these entry points:

- `install` — under one lock (`release.mjs`): stages this version as an immutable
  release (`~/.local/share/sidevoice/releases/<id>`: the package, the core runtime — uv:
  Python and the pinned wheel, progress shown — its `--self-test`), waits for calls to
  end (`--apply-now` does not), switches `current` to it (`previous` keeps the one before;
  each a symlink replaced by rename), restarts the jobs on it and verifies it within 60 s;
  if it does not run, `current` goes back to `previous`. A same or older version is a
  no-op that still verifies the selection — running it again is the recovery. `--service`
  (or jobs already defined) installs Sidevoice as a login service; the harnesses: each one
  found, `--harness <id>`, or none with `--no-agents`, registered once through
  `current`. `--no-core` leaves the core for later; `--json` prints one object for the
  app. Pairs with nothing; reports whether the machine is paired.
- `rollback [--json]` — `current` back to `previous`, the jobs restarted on it and verified.
- `service install|uninstall|start|stop|restart|status [--json]` — Sidevoice at login: two
  jobs of the user's own service manager, neither supervising the other — the core
  (`dev.sidevoice.core` / `sidevoice-core.service`) and the connector (`dev.sidevoice.connector`
  / `sidevoice-connector.service`, running `connector --service`). The manager restarts them;
  the core's exit status tells it whether to (0 after a failed start: not again). `status` is
  derived on read from the manager, the core's failure report and its health (`deriveStatus`),
  the same object a connector answers to `node.status`. `stop` is a person's stop: nothing
  starts Sidevoice again until `service start` or the next login. `restart` restarts the core
  job. On Linux, `service install` prints the `loginctl enable-linger` command that would keep
  it running without a session, and never runs it.
- `uninstall` — the jobs unloaded (it stops, deleting nothing, if the manager will not), what
  runs on demand stopped, the definitions, then our harness registrations, then the releases
  and `~/.sidevoice` (all but its two lock files).
- `mcp` — the stdio MCP server a harness starts. One per conversation. Exposes
  `voice_connect`, `voice_pair`, `voice_say`, `voice_disconnect`, `voice_pair_device`,
  `voice_status`; carries the
  operational instructions in its `initialize` result. It never talks to the
  room: it keeps one local connection to the connector for as long as the
  session lives, and the binding it registered dies with that connection.
- `connector [--service]` — one per machine (a kernel lock, `connector.lock`). With `--service`
  it is the connector job: it never leaves on its own and never starts a core — it links to the
  core job's when that answers, and again whenever it restarts. Without, it is what the launcher
  (`launcher.mjs`, the only way anything gets a connector) starts where no connector job is
  defined: it starts a core, detached, if none answers (with uv, at the pinned `CORE_VERSION`,
  `core.mjs`), and leaves fifteen seconds after the last binding does. Where a connector job is
  defined the launcher never spawns one. When the connector is restarted or upgraded, each façade
  reconnects by itself and registers its conversations again, without waiting for a tool call.
  Either holds the link to this machine's **core** (`sidevoice/sidevoice-core`) over the core's
  own socket (`core-socket.mjs`), re-announces its bindings on every reconnect, keeps a durable
  outbox for speech published while offline, and delivers one input event at a time per binding
  through the adapter that binding was registered with.
- `pair <room-url> <code> [--json]` — redeems, by hand, a pairing code from the room UI for
  this machine's credential (`~/.sidevoice/credentials.json`, mode 0600), the one its
  core links with the room by. The conversation's path is `voice_pair`, with the code
  the user read from the room; the conversation never asks the room for a code.
  It restarts nothing: the core follows that file, and links with a new pairing by itself.
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
`node --test test/test_connector.mjs` (`test_core.mjs` covers the core's install and start
against a fake `uv` and a fake core, and the two jobs' contract; `test_node.mjs` the jobs,
the launcher and `deriveStatus` against stand-ins of launchd and systemd; `test_install.mjs`
releases, the switch and rollback; `test_security.mjs` the locks and the trust boundary;
`test/service-integration.mjs` runs the real service managers in CI); sidevoice-core's
`test_connector_interop.py` runs this connector for real against the core, from
the checkout and from the bundle. `SIDEVOICE_CORE_WHEEL=<wheel> npm run build`
puts the pinned core's wheel inside `dist/core/`, so the published package
installs it without any index.
