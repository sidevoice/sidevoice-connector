# Sidevoice connector

One binary, `sidevoice-connector`, that links this machine's agent conversations (Claude Code, Codex, Cursor) to the
Sidevoice core: it installs itself and the core it carries (`install`, below), serves MCP to each conversation and
runs the machine's connector daemon, which holds the link to the core. On npm it is `sidevoice`, whose launcher runs
this binary from the platform package (RELEASING.md); the command is then `sidevoice`.

The connector reports `input.working` as soon as its harness observes a busy transition. Claude Code uses its
session status, Codex uses rollout turn events, and Cursor uses the joined chat's transcript. For an editor chat,
Cursor's Desktop Bridge must first identify that chat; delivery identifying it wakes transcript monitoring at once.
The core exposes this as a boolean fact, and the web waits three seconds before showing Working.

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
| `pair-device [--json]` | A one-time code from this machine's core (through the connector, started on demand) to pair a device such as the Sidevoice app: the code, its QR, its validity and where it works (`reach`: `direct` or `local-only`). `--json`: `{ok, code, expires_in, reach, payload}`. The MCP tool `voice_pair_device` says the same text. |
| `--version [--json]` | The release version; with `--json`, also the target and the source commit. |

The command line is English only and is not translated. With `--json` a command prints one JSON object on stdout; a
failure is `{"ok":false,"error":{"key","params","message"}}` and exit status 1, the key stable and the message
English (`messages/errors.json`, compiled in). Without `--json` a failure is one English line on stderr.

On Windows, Codex and Claude Code installed through npm, pnpm or yarn classic use their native platform binary
when the installed package provides one. On ARM64 Windows, native ARM64 packages are preferred, with x64 packages
as an emulated fallback. If native resolution fails, the original launcher remains available and the reason is logged.
Yarn Plug'n'Play native resolution is unsupported: use Yarn's `nodeLinker: node-modules` or npm/pnpm to resolve the
native program directly. Native installers keep their own executable.

## Where things are

Every place comes from the environment, so a disposable profile is just a set of variables (the tests use one):

- the data directory `D`: one rule for every Sidevoice program on the machine (core, connector, the desktop app),
  owned by `sidevoice-platform`'s `data_dir`: `$SIDEVOICE_DATA_DIR`, else the OS's standard place for a program's
  data: macOS `~/Library/Application Support/Sidevoice`, Linux `$XDG_DATA_HOME/sidevoice` (else
  `~/.local/share/sidevoice`), Windows `%LOCALAPPDATA%\Sidevoice`. Private (0700), created by the daemon when
  missing and refused, never repaired, when someone else could enter it or any directory above it could have its
  entries replaced. It holds `connector.lock`, `connector.log`, `connector.json`, `agents.json`, `agents.lock`,
  `node-stopped.json` (a person's stop), the speech outbox, and `core/`
  (the core's own: `core.json`, which names the core's endpoint);
- where each program listens is published, not worked out: `D/connector.json` (`{"pid", "endpoint"}`, private,
  written once the connector listens and removed when it exits cleanly; after a crash it stays and `pid` tells it is
  stale) and the core's `D/core/core.json` (`endpoint`). `endpoint` is the record `sidevoice-platform`'s `ipc`
  writes and reads: a Unix socket path, or a named pipe on Windows. Whoever needs to reach either (the desktop app)
  reads it there;
- the local sockets (`connector.sock`, the core's `local.sock`): in this user's runtime directory, each in a private
  `sidevoice-<hash of its data directory>` directory (`D` for the connector, `D/core` for the core), so their paths
  stay within the OS limit: Linux `$XDG_RUNTIME_DIR` (`/run/user/<uid>`), macOS the per-user `$TMPDIR`
  (`/var/folders/…/T/`). Without a runtime directory (Linux with no login session) they are `D/connector.sock` and
  `D/core/local.sock`. Windows uses named pipes instead, with no path (`sidevoice-platform`'s `ipc`);
- the release root `R`: `D/install`, always inside `D`;
- the home: `$HOME` (Windows: `%USERPROFILE%`);
- the core's port: `$SIDEVOICE_CORE_PORT`, else 10829;
- the core's listen address: `$SIDEVOICE_CORE_HOST`, else `0.0.0.0` (all IPv4 interfaces). Set it to `127.0.0.1`
  to restrict the core to this machine. The same setting is used by the login service and on-demand starts. With a
  compatible core, another device can enter this machine's LAN IP address during pairing without an extra Host
  setting. The compatible Core includes its private interface IP addresses and actual listening port in new pairing
  codes, up to the web client's code URL limit. The desktop app can prefill one of these addresses; agree to its
  private `http://` connection warning to continue. That sends the one-time pairing secret and device
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
`Linux x64`), the connector's version and the agents whose homes exist here. The core keeps the latest and the app
lists the machine by it.

Each conversation also reports its engine, `{"model","effort","thinking"}`, on joining and again (`input.engine`)
whenever it changes; the app shows it as the participant's `engine`. A value nobody recorded is `null`, never
guessed, and nothing is reported without a model. Claude Code: the model and effort of each assistant entry in the
transcript, with the launch flags (`--effort`, `--thinking`) for what the entry lacks. Codex: `model` and `effort`
of the rollout's `turn_context`. Cursor CLI: the chat's `lastUsedModel` in its own `store.db`, including an explicit
`[effort=...]` model parameter when present. Cursor editor: the identified chat's `composerData:<chat-id>` model
configuration in the Desktop Bridge user-data database. Its `effort` is the chat's `effort` or `effort` model
parameter, then a model-specific default recorded with that configuration. A global Cursor picker is never used
for another chat; if the chat records no effort or model-specific default, `effort` stays null. `thinking` stays
null. These are raw model and effort IDs for the app to render, not English display text.

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

- `R/releases/<version>/`: one directory per version, with a copy of the binary (`bin/sidevoice-connector`; on Windows also `bin/sidevoice-background.exe`, which the service tasks run so no console window opens) and the
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
(`R` is derived from it), so an agent opened from any shell reaches this installation. An agent whose MCP settings cannot
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

For a Cursor editor chat, a receipt of `delivered` means Cursor's Desktop Bridge answered `submitted` or
`queued`; it does not prove the editor displayed or read the message. A later transcript observation supplies the
`read` receipt. When investigating a missing message, `connector.log` records `cursor_bridge` lines with fixed
`stage` and `result` codes: `lookup` distinguishes no bridge instance, missing database, SQLite failure, no marker,
ambiguous chats, and a found chat; `send` records the bridge instance ordinal, HTTP status, and whether it queued,
submitted, refused, or left the reply uncertain. Instance ordinals follow descriptor creation order (newest first)
for that scan. `id_hash` is the first 12 hex characters of SHA-256 of the marker or chat ID; the log contains no
bridge token, message text, socket path, database path, or home path. An unconfirmed send also logs
`reply_body`, capped at 512 bytes: arbitrary string values and unknown keys are redacted, while known
status codes, booleans and the JSON shape remain visible. Cursor owns this Desktop Bridge; the connector
checks descriptor `protocolVersion: 1`, not the Cursor application or extension build version. A request whose
write may have reached the bridge remains `unknown` and is not retried blindly.

Cursor editor chats cannot be steered. Claude Code takes a voice message into the running turn and Codex can steer
one, but the Desktop Bridge's `sendMessage` has no documented way to add input to a turn in progress. The connector
sends every message with `force:false`, leaving Cursor to decide: for an idle chat the bridge answers `submitted` (a
turn starts), for a busy one `queued`, which the connector reports as delivered like `submitted`; the message is
then held by Cursor and runs after the current turn. The `force` field is undocumented and its effect is
unverified (it may interrupt the running turn), so it is not used. A reply that is neither accepted nor an explicit
refusal stays `unknown` and is not retried blindly; `status: "error"` or a 4xx is a failed send. The working
state comes from the chat's transcript, so it stays right while a message waits in Cursor's queue. The decision is
`classify_send_reply` in `adapters/cursor.rs`, with unit tests over mock bridge replies.

`uninstall` unloads both jobs and stops what runs on demand (a refusal stops it there, with nothing deleted), removes
the agents' registrations Sidevoice wrote (current or not), then the owned data and releases. It keeps lock files and
the stop, so that a launcher on its way finds the stop and starts nothing.

Before unloading, uninstall checks that `R` is `D/install` and that `D` is ours: never the home directory, a directory
the home is in, or a file-system or volume root, whatever it holds; and either exactly the OS's standard place above
(wherever `XDG_DATA_HOME` puts it, inside the home or not), or, for a `$SIDEVOICE_DATA_DIR` elsewhere, a directory
holding an installation of ours (an `install.json` of this user's that nobody else can change, naming `R` as its
releases). Anything else, such as a folder that merely has the program's name, is refused with nothing deleted. A
directory elsewhere with no `install.json` and nothing but what an uninstall leaves proves nothing: uninstall answers
`installed: false` and writes, stops and deletes nothing. What `D` is (its file id) is recorded when it is checked, or
when uninstall makes it, and every later use compares it, so a directory swapped in meanwhile is refused and left alone;
whether `D` is still ours is checked again once the install lock is held.
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

`voice_connect` returns the core's conversation `epoch` and `initial_reply_context`. After an incoming message, pass its `session_id`, `revision`, and `epoch` together to `voice_say`; for speech before the first message, pass `conversation` to use the returned initial context. Only a confirmed spoken turn advances the epoch. Typed context does not interrupt playback. A refused `voice_say` while a person is speaking is terminal: wait for that person's message; nothing is queued for later audio. A saved publication records admission, not proof of hearing.

Each binding attempts injection when its harness supports it. `get_messages` is available for every conversation and reads only journal input that has not been injected or acknowledged. Its `get` operation returns full messages and their agent envelope, with stable `message_id` and cursor. An unacknowledged claim is returned again; pass `ack_ids` only after processing the messages. `check` reports pending counts without claiming. A claim prevents later injection. Sidevoice only notes have `channel: "note"` and empty user text; `reply_not_heard`, empty turn, return, and `reply_heard` correction notices remain in the ordinary queue. Core evidence uses only confirmed `partial` or `not_heard` fates, with full reply text and a Unicode-scalar extent. The agent envelope tells the agent to revisit what still matters, identifies each reply by its opening words, and shows only the tail of the confirmed heard prefix; the full reply remains in its `voice_say` context. A note with no person words asks the agent to tell them now what still matters. Physical playback causes stay in app diagnostics.

Candidate integration tests can set `SIDEVOICE_TEST_CORE_DIR` to an unpacked source build of the matching core PR head. The `sidevoice-platform` tag in `connector/Cargo.toml` remains a numbered published release gate for distribution and CI packaging.

## Tests

`cargo test` runs the unit tests and, in `tests/`, this binary against the sidevoice-core release named by
the `sidevoice-platform` tag in `connector/Cargo.toml` (a conversation joins through MCP, input from a call is delivered, a reply is
saved, the core restarts and the link comes back), against the real Codex CLI in a disposable profile, as a
login service under the real launchd (macOS) and the real systemd user manager (Linux; on a CI runner the test
enables lingering for the runner's user), with no service manager at all; and the
core staged from a package into a release; and `install`, `install` again and
`uninstall` from such a package with no service manager (upgrade, rollback and pruning are unit tests, with a
stand-in for the services). `cargo xtask fixtures` fetches the core and
Codex; without them those tests are skipped locally and fail in CI.

Codex (the CLI and the desktop app) names the conversation in each tool call's `_meta`: `x-codex-turn-metadata`,
with its `thread_id`. rmcp hands a request's `_meta` to the handler's context, which is where the connector reads it;
Codex never passes the thread to an MCP server through its environment. Older Codex releases sent no thread at all:
those need `CODEX_THREAD_ID` in the registration's environment. A queued message can be accepted before Codex reads
it; the connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process
journal, which the core keeps in memory; the connector removes an outbox item on that acknowledgement, so a core
crash immediately afterwards can lose that speech text ([Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43)).
A speech row copied from the JavaScript connector has no conversation reference; if the core later remints its
binding ID, it is retained and reported rather than assigned to another conversation
([Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41)).

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
