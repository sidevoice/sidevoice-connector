# Sidevoice connector

One binary, `sidevoice-connector`, that links this machine's agent conversations (Claude Code, Codex, Cursor) to the
Sidevoice core: it installs itself and the core it carries (`install`, below), serves MCP to each conversation and
runs the machine's connector daemon, which holds the link to the core. On npm it is `sidevoice`, whose launcher runs
this binary from the platform package (RELEASING.md); the command is then `sidevoice`.

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
  `node-stopped.json` (a person's stop), `credentials.json` (the room pairing), the speech outbox, and `core/`
  (the core's own: `core.json`, `local.sock`);
- the release root `R`: `$XDG_DATA_HOME/sidevoice`, else `~/.local/share/sidevoice`;
- the core's port: `$SIDEVOICE_CORE_PORT`, else 10829;
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
`command`, `sidevoice agents connect <id>`), `manual` when that needs a step by hand (the message gives it), or
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

### Cursor Desktop Bridge status

The Cursor row in `agents.list` / `agents --json` also has `cursor_bridge`, separate from MCP `registration` and
`health`. The web translates its stable step IDs and reason keys. It contains no display text, home path, bridge
token, or socket path. Example:

```json
{
  "state": "needs_attention",
  "reason": "bridge_missing",
  "cursor_version": "3.22.12",
  "minimum_cursor_version": "3.22.12",
  "minimum_connector_version": "0.9.0",
  "steps": [
    {"id":"cursor_version","state":"done"},
    {"id":"enable_desktop_bridge","state":"unknown"},
    {"id":"bridge_running","state":"needs_action"},
    {"id":"sqlite3","state":"done"},
    {"id":"register_sidevoice","state":"done"},
    {"id":"join_editor_chat","state":"unknown"},
    {"id":"wait_for_chat_registration","state":"unknown"}
  ]
}
```

`state` is `ready`, `needs_attention`, or `unknown`. `reason` is null when ready, otherwise one of
`cursor_version_too_old`, `bridge_missing`, `bridge_stale`, `sqlite3_missing`, `sidevoice_not_registered`,
`cursor_version_unknown`, or `registration_unknown`. Step states are `done`, `needs_action`, or `unknown`.
`cursor_version` is the parsed version from the editor's `cursor --version`, or null when unavailable. The minimum
Cursor version is 3.22.12; the minimum connector version is 0.9.0. A live bridge requires a usable protocol-1
descriptor under `~/.cursor/desktop-bridge/*.json`, with a live process and an existing socket. `sqlite3` must be
executable on the captured login PATH. Missing and empty directories both give `bridge_missing`; unusable descriptors
give `bridge_stale`. `agents.list` with `rescan` or `watch`, and `agents check cursor`, refresh the checks; a cached
list returns the last result.

For `enable_desktop_bridge`, the web should explain Cursor → Settings → Beta → “Allow CLI to access desktop agents”
(visible only with the account's `desktop_bridge` beta). Cursor exposes no readable setting for this check: it is
`done` when a live bridge proves it is enabled and `unknown` otherwise. For `register_sidevoice`, the person
connects Cursor to Sidevoice, restarts Cursor if needed, and approves the Sidevoice MCP server. Then they ask a
**new editor chat** to join the voice room. The join result has `route: "cursor-editor-bridge"` and a `chat_marker`;
the per-chat `voice_status.bridge.state` becomes `registered` with `bridge_chat_known: true` once Cursor records the
join (it can first say `waiting for Cursor to register this chat`). These last two steps remain `unknown` in the
machine-wide row because they can only be checked for that chat. After registration, selecting the conversation in
the room and sending text should make it appear in the editor chat, where the agent can reply by voice.

`uninstall` unloads both jobs and stops what runs on demand (a refusal stops it there, with nothing deleted), removes
the agents' registrations Sidevoice wrote (current or not), then `R` and everything in `D` but its lock files and the
stop, so that a launcher on its way finds the stop and starts nothing. The room keeps this machine's pairing until it is revoked
there.

With `--json` both print one JSON object and nothing else.

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

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process
journal, which the core keeps in memory; the connector removes an outbox item on that acknowledgement, so a core
crash immediately afterwards can lose that speech text ([Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43)).
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
app sends for what was said), so the agent receives it with its voice header and `[Sidevoice]` line. Each message
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
