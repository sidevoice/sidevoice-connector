# Sidevoice connector

One binary, `sidevoice-connector`, that links this machine's agent conversations (Claude Code, Codex, Cursor) to the
Sidevoice core: it installs itself and the core it carries (`install`, below), serves MCP to each conversation and
runs the machine's connector daemon, which holds the link to the core. On npm it is `sidevoice`, whose launcher runs
this binary from the platform package (RELEASING.md); the command is then `sidevoice`.

The connector reports `input.working` as soon as its harness observes a busy transition. Claude Code uses its
session status, Codex uses rollout turn events, and Cursor uses the joined chat's transcript. For an editor chat,
Cursor's Desktop Bridge must first identify that chat; delivery identifying it wakes transcript monitoring at once.
The room exposes this as a boolean fact, and the web waits three seconds before showing Working.

## Commands

| Command | What it does |
| --- | --- |
| `install [--no-agents] [--no-prompt] [--json]` | Installs this package's Sidevoice (or updates to it) and starts it. It connects no agent by itself: it lists the agents found here, and at a terminal offers to connect them (below). `--no-agents` leaves the agents alone; `--no-prompt` does not ask. |
| `uninstall [--json]` | Removes Sidevoice from this computer: its service jobs, its agent registrations, its releases and its data. |
| `mcp` | MCP over stdio for one conversation; it reaches the daemon over `connector.sock`. |
| `connector [--service]` | The daemon: one per data directory. `--service` (or `SIDEVOICE_SERVICE=launchd\|systemd`) when a service manager runs it. |
| `agents [--json]` | The agents on this computer and whether they use Sidevoice. |
| `agents connect\|disconnect\|dismiss\|check <claude\|codex\|cursor> [--json]` | Registers Sidevoice with an agent, removes its registration, dismisses the new-agent notice, or checks that its registration works (below). Only an entry Sidevoice wrote is ever changed. |
| `service <install\|start\|stop\|restart\|status\|uninstall> [--json]` | The login service: two jobs under launchd or the systemd user manager; with no manager, Sidevoice on demand (below). |
| `service login <on\|off\|status> [--json]` | Whether the login service starts at the next login, and only that: turning it off stops nothing, turning it on starts nothing (below). |
| `pair <room-url> <code> [--json]` | Pairs this machine with a room: redeems the one-time code the room shows at its `POST /api/connectors/pair` and writes `credentials.json` (0600). https only; plain http only to loopback or a host in `SIDEVOICE_TRUSTED_CLUSTER_HOSTS` (`.suffix` or exact host, comma-separated). The core follows the file; nothing restarts. |
| `pair-device [--json]` | A one-time code from this machine's core (through the connector, started on demand) to pair a device such as the Sidevoice app: the code, its QR, its validity and where it works (`reach`: `room`, `direct` or `local-only`). `--json`: `{ok, code, expires_in, reach, payload}`. The MCP tool `voice_pair_device` says the same text. |
| `--version [--json]` | The release version; with `--json`, also the target and the source commit. |

The command line is English only and is not translated. With `--json` a command prints one JSON object on stdout; a
failure is `{"ok":false,"error":{"key","params","message"}}` and exit status 1, the key stable and the message
English (`messages/errors.json`, compiled in). Without `--json` a failure is one English line on stderr.

## Where things are

Every place comes from the environment, so a disposable profile is just a set of variables (the tests use one):

- the data directory `D`: `$SIDEVOICE_DATA_DIR`, else `~/.sidevoice`. Private (0700), created by the daemon when
  missing and refused, never repaired, when someone else could enter it or any directory above it could have its
  entries replaced. It holds `connector.sock`, `connector.lock`, `connector.log`, `agents.json`, `agents.lock`,
  `node-stopped.json` (a person's stop), `credentials.json` (the room pairing), the speech outbox,
  `speech-identities.json` (bounded v1 ID and result hashes; no original reply text), its
  `speech-identities.guard` initialization marker, and `core/`
  (the core's own: `core.json`, `local.sock`);
- the release root `R`: `$XDG_DATA_HOME/sidevoice`, else `~/.local/share/sidevoice`;
- the core's port: `$SIDEVOICE_CORE_PORT`, else 10829;
- the core's listen address: `$SIDEVOICE_CORE_HOST`, else `0.0.0.0` (all IPv4 interfaces). Set it to `127.0.0.1`
  to restrict the core to this machine. The same setting is used by the login service and on-demand starts. With a
  compatible core, another device can enter this machine's LAN IP address during pairing without an extra Host
  setting. The pairing code does not discover or announce a LAN address; enter it manually in the current desktop
  app and agree to its private `http://` connection warning. That sends the one-time pairing secret and device
  token over the LAN without TLS. Setting `SIDEVOICE_ALLOWED_HOSTS` disables automatic acceptance of numeric IP
  Hosts; include the address the other device enters. Allowed Origins may still grant their own Host names. A hosted
  HTTPS page or public port forward needs a trusted HTTPS/WSS endpoint, not public plain HTTP. Configure these
  variables when installing or updating the service;
- the agents' homes: `$CLAUDE_CONFIG_DIR` (`~/.claude`), `$CODEX_HOME` (`~/.codex`), `$CURSOR_CONFIG_DIR`
  (`~/.cursor`), `$CURSOR_DATA_DIR`. Each is checked to be the person's and writable by nobody else before it is
  read or written. `CLAUDE_CONFIG_DIR` reaches Claude Code (its CLI, the service jobs) only when the person's
  environment sets it: Claude Code keeps its user configuration, MCP servers included, in `~/.claude.json` without
  it and in `$CLAUDE_CONFIG_DIR/.claude.json` with it, so naming the default would make it read another file.

Records are written whole (a private temporary file renamed into place). Locks are kernel `flock(2)` locks on
permanent lock files, released when their holder dies, with the holder's pid and start time written inside for
people; a pid is signalled only after checking it is still the process recorded (owner, start time, command line).
`connector.log` is rotated at 5 MB by copy-and-truncate, keeping two previous files. A daemon run by hand logs to
stderr and the file; run by a service manager, to the file only.

When it links to the core, the daemon says what this machine is: its host name, its platform (`macOS arm64`,
`Linux x64`), the connector's version and the agents whose homes exist here. The core keeps the latest and the room
lists the machine by it.

Each conversation also reports its engine, `{"model","effort","thinking"}`, on joining and again (`input.engine`)
whenever it changes; the room shows it as the participant's `engine`. A value nobody recorded is `null`, never
guessed, and nothing is reported without a model. Claude Code: the model and effort of each assistant entry in the
transcript, with the launch flags (`--effort`, `--thinking`) for what the entry lacks. Codex: `model` and `effort`
of the rollout's `turn_context`. Cursor: the chat's last used model, no effort.

## The core inside

The connector's package carries the core: next to `bin/sidevoice-connector`, the pinned sidevoice-core release's
archive for the same target (`core/sidevoice-core-<version>-<target>.tar.zst`), named with its version, digest, size
and source commit in the package's inventory, `connector.json` (`cargo xtask dist` puts it there after checking it
against the core release's attestation; RELEASING.md). Nothing is downloaded at install.

Installing a release stages it (`src/core_package.rs`; run as the internal step `stage-core <release> [--json]`):
the archive must have the inventory's size and digest; it is unpacked beside the release, below its one root, plain
files and directories only, and must be exactly the core's own inventory (`native-core.json`: this target, this
source commit, every file with its size and digest); then it is renamed into `<release>/core`. The core is a
`rust-native-v2` archive: its program in `bin/` and its notices in `notices/`, nothing else. The failures carry
stable keys: `core.package-missing`, `core.package-mismatch`. The core never runs with a library-path variable
(`LD_*`, `DYLD_*`) of ours: it loads only the system's libraries.

## Installing

`install` (`src/install.rs`) runs from the package it is part of (an npm platform package or the release archive,
laid out as above) and makes the installation every other part runs (sidevoice/sidevoice-connector#66, decisions 1
and 3):

- `R/releases/<version>/`: one directory per version, with a copy of the binary (`bin/sidevoice-connector`) and the
  core staged in it (above). Another build of a version already there (a nightly, a local build) is
  `<version>+<the first 12 hex digits of its binary's digest>`.
- `R/current`: a link to the selected release, switched by renaming a new link over it.
- `D/install.json`: `command`, `[R/current/bin/sidevoice-connector]`, and `releases`, `R`. The service jobs, the
  desktop app and every agent registration run that command, never the package's own path (npm's cache moves).

Under the install lock, `install` stages the release unless it is there, whole (its core as its inventory says), switches `current`, writes `install.json`, clears a person's stop, and restarts: with a service manager both
jobs are defined (`service install`'s definitions) and restarted; without one, what runs on demand is stopped and the
connector is started from the new release, which starts its core. The pair must then answer within 60 s: the core's
health, for a launch after the restart, and the connector's identity (this release's binary) and its `node.status`.
When it does not, `current` goes back to the release it named before, that one is restarted and verified, the failed
release is deleted, and the install fails with `install.rolled-back` (or `install.rollback-failed`); a first
installation that does not answer stays, failing with `install.verify-failed` and the core's own failure. Releases
other than the current one and the one before it are pruned: exactly one previous, no `verified` link, no `rollback`
command. Running `install` again is the recovery: a release whose core no longer matches its inventory is staged
again.

Installing connects no agent. An agent gets Sidevoice only when the person connects it: Connect in the app (which
calls `agents.connect`), `agents connect <id>`, or their answer to the offer below. Unless `--no-agents`, `install`
lists the agents found (a rescan) and its answer's `agents` has one entry per agent, `{outcome, message}`:
`registered` when it uses Sidevoice, `not-connected` when it does not and the connector can connect it (with
`command`, `npx sidevoice agents connect <id>`), `manual` when that needs a step by hand (the message gives it), or
`failed`, `foreign`, `invalid`, `unknown`. The one entry `install` writes is one Sidevoice wrote that is not current
(`outdated`: disabled, naming another release, or without the installation's environment): it is written again with
the installation's command (Claude Code through `claude mcp add`, Codex through its CLI, Cursor in its `mcp.json`,
only the `sidevoice` entry). That is not a new consent: the person connected that agent already, and the entry would
stop working once the release it names is pruned. Each registration carries the installation's `SIDEVOICE_DATA_DIR`
and `XDG_DATA_HOME`, so an agent opened from any shell reaches this installation. An agent whose MCP settings cannot
be read is `invalid` and left alone.

At a terminal (standard input and output both, no `--json`, no `--no-prompt`, `CI` unset), `install` then offers to
connect the agents it found that the connector can connect itself: all, some (by number) or none, the default
(Enter, or the end of input); those chosen are connected as `agents connect` connects them. The install lock is
released before it asks. With `--json` (the desktop app reads it) or without a terminal nothing is asked.

An agent is found (listed by `agents`, `agents.list` and the install offer, with `present: true`) when any
adapter-declared marker matches. CLI commands are resolved with the `which` crate against the person's freshly
captured login-shell PATH, never the connector service's PATH. Fixed install paths are checked with
`symlink_metadata` only; checking a path never starts an application or runs an OS registration query. The connector
captures PATH with a framed `$SHELL -lc` command on each scan, rejects startup banners outside the frame, and lists
no agents if capture fails. A configuration directory alone is never installation evidence.

| Agent | CLI names (all OS) | macOS fixed markers | Windows fixed markers | Linux fixed markers |
| --- | --- | --- | --- | --- |
| Claude Code | `claude` | `~/.local/bin/claude` (native installer) | `%USERPROFILE%\.local\bin\claude.exe` (native installer) | `~/.local/bin/claude` (native installer) |
| Codex | `codex` | none; npm/Homebrew prefixes vary | none; installation prefixes vary | none; installation prefixes vary |
| Cursor | `cursor-agent`, `cursor` | `/Applications/Cursor.app`, `~/Applications/Cursor.app` | `%LOCALAPPDATA%\Programs\cursor`, `%ProgramFiles%\cursor` | `/usr/share/cursor` (package install), `~/.local/share/cursor` (per-user extracted install) |

`~` uses the process home; Windows environment roots are read from `LOCALAPPDATA` and `ProgramFiles`. Fixed markers
are intentionally narrow. A Cursor app installed elsewhere without a `cursor` or `cursor-agent` command on the
captured PATH is not detected; the person can use Other to connect by hand. Public `evidence` reports only matched
marker kinds (`cli`, `install_file`, `app_bundle`, `install_dir`), never the resolved paths or home directory.
Install-marker checks never run candidate commands. Scans do not probe `--version`, so an agent's `version` is `null`.

Codex queue delivery uses the CLI from that captured PATH and fails if none was found; it never runs a different
`codex` from the connector service's PATH.

An agent whose registration is `connected` also says whether that registration works: its row in `agents` (and the
connector's `agents.list`) carries `health`, `{"state","error","checked_at"}`. The connector starts the entry as the
agent would: its command, arguments, environment and (Codex's `cwd`) working directory, read back from the agent's
configuration over the profile's homes and the login PATH the scan captured. Claude Code's entry is read from
the file its CLI reads (`$CLAUDE_CONFIG_DIR/.claude.json` when the person's environment sets the variable, else
`~/.claude.json`), never with `claude mcp get`, which starts the server it names; nothing
is started before the entry is known to be ours. The server gets `SIDEVOICE_HEALTH_CHECK=1`, which keeps `mcp` from
reaching or starting the daemon. The connector sends `initialize`, `notifications/initialized` and `tools/list`
(their results must be what the protocol defines: `protocolVersion`, `capabilities`, `serverInfo` with its name and
version; tools with their names), calls no tool, and stops the server and its process group, within 5 s. `state` is
`working`, or `not-starting` with `error` saying why in a few English words (`command not found`, `permission
denied`, `could not start`, `working directory not found`, `exited with status N`, `killed by signal N`, `closed its
output`, `timed out`, `protocol mismatch`); `checked_at` is when (ISO 8601, UTC). An agent that is not connected has
`health: null`. The check runs on every scan (`agents.list` with `rescan` or `watch`), after a connect, and on demand
(`agents.check`, `{"id"}`, or `agents check <id>`); a listing from the cache, and a request with too little time
left, keep the last one found (in `agents.json`).

### Agent readiness

Every row in `agents.list` / `agents --json` may carry an optional `readiness` object:

```json
{"readiness":{"state":"needs_attention","reasons":[{"code":"cursor.bridge_not_running","message":"Voice in the Cursor editor needs Cursor's Desktop Bridge: in Cursor, open Settings › Beta, enable “Allow CLI to access desktop agents”, then restart Cursor."}]}}
```

`state` is `ready`, `needs_attention`, or `unsupported`. `reasons` is an ordered array of
`{"code","message"}` objects. `code` is stable in the `<agent>.<code>` namespace;
`message` is a plain English fallback instruction supplied by that agent's check, with no paths,
tokens, versions, or home directories. Consumers use `code` for logic and localization: the web
shows its `agents.readiness.<code>` translation when present, else `message`. An absent
`readiness` field means ready: adapters with no checks omit it. Every check contributes either
OK, needs attention with a reason, or unsupported with a reason. The aggregate is `unsupported`
if any check is unsupported; otherwise `needs_attention` if any check needs attention; otherwise
`ready`. All non-OK reason objects are retained in check order. A ready object has `reasons: []`.
For `cursor.unsupported`, the fallback is “Cursor editor voice is not supported on this computer.”

Cursor currently has one check: a usable Desktop Bridge descriptor with a live process and socket
is ready even when the selected CLI is `cursor-agent`. A detected Cursor editor in either
`/Applications` or the user's `Applications` directory with a missing, empty, stale, or unusable
bridge needs attention with `cursor.bridge_not_running`; non-macOS or no editor or live bridge is
unsupported with `cursor.unsupported`. A first macOS scan reports provisional needs attention
until its background refresh finishes. Refreshed status uses a snapshot up to two seconds old;
filesystem discovery runs off the request worker. Readiness is machine-wide and says nothing about a particular chat.
Per-chat `voice_status.bridge.bridge_chat_known` remains the source for whether that chat is known.
The setting is in Cursor Settings → Beta → “Allow CLI to access desktop agents”. `agents.list`
with `rescan` or `watch`, and `agents check cursor`, refresh readiness; a cached list returns the
last result.

Cursor's chat lookup and model metadata use a bundled SQLite library through bounded, read-only
queries on blocking workers. No system `sqlite3` command is required. Read-only mode preserves
visibility of Cursor's live WAL; immutable mode is avoided because it can miss a newly recorded
join. The connector does not issue writes to Cursor's databases. A query has a two-second deadline,
interrupts on timeout, and caps returned data at 64 KiB. SQLite errors in diagnostics are fixed
codes and contain no query values, chat text, or local paths.

For a Cursor editor chat, a room receipt of `delivered` means Cursor's Desktop Bridge answered `submitted` or
`queued`; it does not prove the editor displayed or read the message. A later transcript observation supplies the
`read` receipt. When investigating a missing message, `connector.log` records `cursor_bridge` lines with fixed
`stage` and `result` codes: `lookup` distinguishes no bridge instance, missing database, SQLite failure, no marker,
ambiguous chats, and a found chat; `send` records the bridge instance ordinal, HTTP status, and whether it queued,
submitted, refused, or left the reply uncertain. Instance ordinals follow descriptor creation order (newest first)
for that scan. `id_hash` is the first 12 hex characters of SHA-256 of the marker or chat ID; the log contains no
bridge token, message text, bridge response body, socket path, database path, or home path. A request whose write
may have reached the bridge remains `unknown` and is not retried blindly.

`uninstall` unloads both jobs and stops what runs on demand (a refusal stops it there, with nothing deleted), removes
the agents' registrations Sidevoice wrote (current or not), then the owned data and releases. It keeps lock files and
the stop, so that a launcher on its way finds the stop and starts nothing. The room keeps this machine's pairing until
it is revoked there.

Before unloading, uninstall checks that `D` is exactly `HOME/.sidevoice` and `R` is exactly
`HOME/.local/share/sidevoice`. A custom data or release path must be dealt with manually; uninstall refuses it.
It checks the trees for mount boundaries before taking the install lock, which uninstall holds without writing to it.
It rejects multiply linked lock files. Before traversal, each owned subtree is atomically moved into a private
`D/.uninstall-claim` directory on the same filesystem, then revalidated and removed through directory handles
without following links. It first copies a recovery CLI to `D/.uninstall-recovery/sidevoice-connector` and atomically
points `D/install.json` at it; both stay available while data or release cleanup can fail. A retry resumes any claimed
entries. Once owned data and releases are gone, uninstall removes the recovery copy and record.
A failed check or a path changed during removal makes `uninstall --json` return its normal `ok: false` error and a
nonzero exit. Symlinks inside an owned tree are removed as links; their targets are left alone. Running uninstall
again after a successful removal is safe.

`--version --json` includes top-level `"safe_uninstall": 1` when this guarded uninstall contract is present.
The successful `uninstall --json` object echoes the same marker. A caller must check the version marker before
starting a destructive reset; the post-uninstall echo alone is too late to protect an older installed binary.

With `--json` both print one JSON object and nothing else.

## Conversation flow

`voice_connect` returns the core's `initial_reply_context` and whether it supports voice contract v1. After a room message, pass its `session_id`, `revision`, `epoch`, and `message_id` as `context_message_id` to `voice_say`, with stable nonempty `event_id` and `utterance_id`. Before any room message, pass `conversation` and omit the whole context to use `initial_reply_context`. A partial context is invalid. Native chat still gets a full written answer and appropriate voice. Typed room context fences older pending audio without interrupting sound already playing or advancing the spoken epoch. `accepted` means only that Core stored the original and admitted `queued`, `pending`, or `text_only` disposition; it never claims sound started or was heard. `rejected` was not admitted. A lost transport response is indeterminate, so retry the same IDs and arguments.

Each binding chooses `input_mode: "push"` (default) or `"pull"` at `voice_connect`. Both expose one logical event: a canonical JSON header, a blank line, and the person's exact words for `channel:"voice"`; a `channel:"note"` event is JSON only. The same message ID and content is one input on redelivery. Spoken words include the app's acoustic-start snapshot and compact hearing changes; typed words omit acoustic-start fields. No full original reply or routine receipt-only model turn is appended. General behavior rules live in the MCP server instructions. In pull mode, `get_messages` reads the same ordered Core queue and exposes the same envelope and stable IDs; `ack_ids` confirms processing, while `after` is only a journal position. Pull may arrive later because the harness must call it.

`voice_get_reply` reads an available immutable original and conservative current-audience hearing without changing playback. `voice_replay` explicitly authorizes an available original from fresh context, using a stable `replay_id`; `resume` repeats from a conservative sentence boundary, while `restart` starts at zero. A focus return with relevant unseen content yields a note, never automatic audio. Missing or expired originals return `unavailable`; no eligible listener is a distinct replay rejection. Existing no-listener `text_only` behavior remains until connector #156.

Candidate integration tests can set `SIDEVOICE_TEST_CORE_DIR` to an unpacked source build of the matching core PR head. `core.pin` remains a numbered published release gate for distribution and CI packaging.

## Tests

`cargo test` runs the unit tests and, in `tests/`, this binary against the sidevoice-core release pinned in
`core.pin` at the repository root (a conversation joins through MCP, input from a call is delivered, a reply is
saved, the core restarts and the link comes back), against the real Codex CLI in a disposable profile, as a
login service under the real launchd (macOS) and the real systemd user manager (Linux; on a CI runner the test
enables lingering for the runner's user), with no service manager at all; `pair` against a room on loopback; and the
core staged from a package into a release; and `install`, `install` again and
`uninstall` from such a package with no service manager (upgrade, rollback and pruning are unit tests, with a
stand-in for the services). `cargo xtask fixtures` fetches the core and
Codex; without them those tests are skipped locally and fail in CI.

Codex (the CLI and the desktop app) names the conversation in each tool call's `_meta`: `x-codex-turn-metadata`,
with its `thread_id`. rmcp hands a request's `_meta` to the handler's context, which is where the connector reads it;
Codex never passes the thread to an MCP server through its environment. Older Codex releases sent no thread at all:
those need `CODEX_THREAD_ID` in the registration's environment. A queued message can be accepted before Codex reads
it; the connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk while an admission is uncertain. A v1 result removes its original text from that outbox; Core's replayable original remains best effort and does not survive a Core restart. The private `speech-identities.json` retains only IDs, the original binding, a request hash, the Core incarnation, compact admission results, and explicit-leave markers. It blocks old IDs from becoming fresh audio after Core or Connector restarts. The separate `speech-identities.guard` records that the ledger was initialized: if the ledger alone disappears, Connector refuses to start. Both files are created and synced before v1 speech can be sent. The file is capped at 8,192 records or 2 MiB; when full, new v1 calls are refused rather than evicting safety records. Remove both only as part of removing the Sidevoice data for that installation. Simultaneous removal of the ledger and guard cannot be distinguished from a pristine installation. Legacy unmarked speech still follows its older outbox behavior ([Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43)).
A speech row copied from the JavaScript connector has no conversation reference; if the core later remints its
binding ID, it is retained and reported rather than assigned to another conversation
([Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41)).

## Test bench

`cargo xtask bench` drives this build and the pinned core from a page on loopback, without the desktop app or a
room, so that a real Claude Code or Codex session can be talked to by typing. It is a developer tool
(`src/bench/`, the package's second binary, `sidevoice-bench`), never part of a release.

When it starts, the bench:

1. makes a private profile, `~/.sidevoice-bench` (`--profile DIR` for another; it must be in a directory nobody else
   can write to, as the connector requires: a group-writable home will not do, `/tmp/...` will). Every directory
   the connector reads is in it, its own `HOME` among them; the profile is kept between runs, so an agent's login in
   it stays. `--reset` uninstalls and deletes it first; `--uninstall` stops what runs there and removes the
   installation;
2. packages this build with the pinned core (as `cargo xtask core` left it in `target/sidevoice-core`, fetched if
   missing) and runs that package's `install --no-agents` with no service manager: the installation an agent's MCP
   server uses, Sidevoice on demand, exactly as on a machine without launchd or systemd;
3. pairs a local device with the core and holds a call open on it, as the app does (when the core is gone, it starts
   the installation's connector, which starts its core);
4. serves the page, `http://127.0.0.1:4477/` (`--port`), and prints the commands below for this profile.

The page lists the conversations that joined (harness, input mode, engine, live or closed). Pick one and type: the
text goes into the call as voice input (`/api/presentation/select`, then `/api/presentation/text`, which is what the
app sends for what was said), so the agent receives it with the canonical voice header and exact words. Each message
shows its receipt as the core records it (`pending`, `delivered`, `unconfirmed`, `read`, `not_sent`), the agent's
`voice_say` replies appear under it, and the call's events (receipts, working state, speech) are listed beside.

Pointing an agent at it (the paths are printed by the bench; `P` is the profile). The agent runs with the profile's
`bench/env.sh` sourced, so that its own home (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) is the one the connector reads, and
its MCP server is the installation's command with the profile's variables:

- **Claude Code**: `. P/bench/env.sh && claude --mcp-config P/bench/mcp.json --strict-mcp-config`. The
  profile's Claude home starts empty: log in once there (`/login`), and accept the folder's trust prompt.
  `--strict-mcp-config` keeps any other Sidevoice registration out of the session. Then ask it to join the voice
  call. If `voice_connect` reports `inbound.ok: false`, Claude Code will not take messages from another session in
  this permission mode: follow its remedy in the profile's settings or use a prompting permission mode.
- **Codex**: `. P/bench/env.sh && codex login` once, then register the server once with the `codex mcp add sidevoice
  --env ... -- .../current/bin/sidevoice-connector mcp` line the bench prints (Codex passes an MCP server only the
  variables it is given), then `. P/bench/env.sh && codex`. If `voice_connect` says it cannot tell which conversation
  this is, this Codex is too old to name its thread in each call: note the thread ID (`/status`), quit, register again
  with `--env CODEX_THREAD_ID=<id>` added, and `codex resume <id>`.

Nothing outside the profile is touched: the operator's own `~/.claude`, `~/.codex` and Sidevoice installation are
neither read nor written, and an installed Sidevoice keeps running beside it (the bench's core takes any free port).

To open the page from another device, the `frp-tunnel` skill can publish the port; start the bench with
`--allow-host <the tunnel's host name>` so that the page answers to it (it refuses any other `Host`). The tunnel's URL
is public and unauthenticated: whoever has it can type into the agents that joined. Close it when done.

`tests/bench.rs` runs the bench's own path in `cargo test`: the bench installs into a private profile, a
conversation (with an HTTP receiver as its harness) joins through the installation's MCP server, what is typed on
the bench is delivered to it, and its reply and the input's receipt show on the bench.

## Login service and Sidevoice on demand

`service install|uninstall|start|stop|restart|status|login` (`src/service/`) runs two jobs of this user's service
manager: the core (`dev.sidevoice.core` under launchd, `sidevoice-core.service` under the systemd user manager) and the
connector daemon (`dev.sidevoice.connector`, `sidevoice-connector.service`). The core job runs
`R/current/core/bin/sidevoice-core-rust` with `idle-exit 0`; the connector job runs `D/install.json`'s `command` +
`connector`. Neither job starts or signals the other. Under systemd, each job must also be enabled for the next login
(`enable`, checked with `is-enabled`; only as below once the person has a choice) and a changed definition read again (`daemon-reload`): a refusal of either fails
the command (`service.not-enabled`), even when the job would run now. A person's stop (`D/node-stopped.json`) is written before the
manager is asked and holds until `service start` or the next login; every change holds `D/install.lock`. On Linux,
`service install` says whether the user lingers and prints `loginctl enable-linger <user>`; it never runs it.

`service status --json`, and a daemon's `node.status`, is derived on every read from the manager, the core's
`core-failure.json` and its health: `absent`, `not-installed`, `stopped-by-person`, `starting`, `backoff`,
`running`, `failed` or `service-failed`, the states the desktop app reads. It also says `start_at_login`: `true`
when the manager starts both jobs at the next login, `false` when it does not start one of them, and `null` when the
manager cannot tell for either, or the service is not whole (no job, or one of the two definitions missing): never
guessed.

`service login on|off` sets whether both jobs start at the next login, and nothing else: launchd `launchctl
enable|disable gui/<uid>/<label>` (no `bootout`, no `bootstrap`), systemd `systemctl --user enable|disable <unit>`
(never `--now`). A running Sidevoice keeps running when it is turned off, a stopped one stays stopped when it is turned
on, and a person's stop is left as it is. Each change is read back from the manager (`launchctl print-disabled`,
`systemctl --user is-enabled`); a refusal, or a change the manager does not confirm, is `service.login-failed`. With
no job defined it is `service.login-no-service`, with one of the two missing `service.login-incomplete` (nothing is
changed; `service install` repairs it), and with no manager `service.no-manager`. `--json` answers as `service
start|stop|status` do, from the status read afterwards: `{"ok":true,"state":"<state>","service":"launchd|systemd|none",
"start_at_login":true|false|null}` (and `failure` when the state has one); `service login status` answers the same.

The choice lives in the manager only. `service install` (or `install`) with no job defined turns it on, over any
override launchd kept from an earlier installation. `service start`, `restart`, `install` over defined jobs and an
update enable the jobs again only when those defined all start at login now; off, or not known (an unreadable
override, systemd's `enabled-runtime`), it is left as it is, never turned on. launchd refuses to bootstrap a job
disabled for login: one turned off is enabled just for its `bootstrap` and disabled again, and when whether it is off
cannot be read, the start is refused (`service.login-unknown`) rather than risk turning it on for good.

With no manager (`SIDEVOICE_SERVICE_MANAGER=none`, a container, an `su` shell, Linux without a user bus) or no jobs
installed, an MCP server starts the connector from the selected installation, and the connector starts the core,
detached, and again whenever its link to it fails.
