# Sidevoice connector

One binary, `sidevoice-connector`, that links this machine's agent conversations (Claude Code, Codex, Cursor) to the
Sidevoice core: it installs itself and the core it carries (`install`, below), serves MCP to each conversation and
runs the machine's connector daemon, which holds the link to the core. On npm it is `sidevoice`, whose launcher runs
this binary from the platform package (RELEASING.md); the command is then `sidevoice`.

## Commands

| Command | What it does |
| --- | --- |
| `install [--no-agents] [--json]` | Installs this package's Sidevoice (or updates to it), starts it, and registers it with the agents found here (below). |
| `uninstall [--json]` | Removes Sidevoice from this computer: its service jobs, its agent registrations, its releases and its data. |
| `mcp` | MCP over stdio for one conversation; it reaches the daemon over `connector.sock`. |
| `connector [--service]` | The daemon: one per data directory. `--service` (or `SIDEVOICE_SERVICE=launchd\|systemd`) when a service manager runs it. |
| `agents [--json]` | The agents on this computer and whether they use Sidevoice. |
| `agents connect\|disconnect\|dismiss <claude\|codex\|cursor> [--json]` | Registers Sidevoice with an agent, removes its registration, or dismisses the new-agent notice. Only an entry Sidevoice wrote is ever changed. |
| `service <install\|start\|stop\|restart\|status\|uninstall> [--json]` | The login service: two jobs under launchd or the systemd user manager; with no manager, Sidevoice on demand (below). |
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
- the agents' homes: `$CLAUDE_CONFIG_DIR` (`~/.claude`), `$CODEX_HOME` (`~/.codex`), `$CURSOR_CONFIG_DIR`
  (`~/.cursor`), `$CURSOR_DATA_DIR`. Each is checked to be the person's and writable by nobody else before it is
  read or written.

Records are written whole (a private temporary file renamed into place). Locks are kernel `flock(2)` locks on
permanent lock files, released when their holder dies, with the holder's pid and start time written inside for
people; a pid is signalled only after checking it is still the process recorded (owner, start time, command line).
`connector.log` is rotated at 5 MB by copy-and-truncate, keeping two previous files. A daemon run by hand logs to
stderr and the file; run by a service manager, to the file only.

When it links to the core, the daemon says what this machine is: its host name, its platform (`macOS arm64`,
`Linux x64`), the connector's version and the agents whose homes exist here. The core keeps the latest and the room
lists the machine by it.

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
command. Then the agents found are registered with the installation's command (Claude Code through `claude mcp add`,
Codex through its CLI, Cursor in its `mcp.json`, only the `sidevoice` entry and only one Sidevoice wrote; an agent
that needs its configuration by hand is told how), unless `--no-agents`. Each registration carries the installation's
`SIDEVOICE_DATA_DIR` and `XDG_DATA_HOME`, so an agent opened from any shell reaches this installation; one of ours
without them, disabled, or naming another release is not current, and is written again. Running `install` again is the
recovery: a release whose core no longer matches its inventory is staged again.

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

Codex 0.157.0 does not give the active thread ID in MCP request metadata, so queued delivery into a Codex
conversation needs `CODEX_THREAD_ID` in its environment. A queued message can be accepted before Codex reads it; the
connector sends `input.read` only after the same message appears in that thread's rollout.

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

- **Claude Code**: `. P/bench/env.sh && claude --mcp-config P/bench/claude-mcp.json --strict-mcp-config`. The
  profile's Claude home starts empty: log in once there (`/login`), and accept the folder's trust prompt.
  `--strict-mcp-config` keeps any other Sidevoice registration out of the session. Then ask it to join the voice
  call. If `voice_connect` reports `inbound.ok: false`, Claude Code will not take messages from another session in
  this permission mode: follow its remedy in the profile's settings or use a prompting permission mode.
- **Codex**: `. P/bench/env.sh && codex login` once, then register the server once with the `codex mcp add sidevoice
  --env ... -- .../current/bin/sidevoice-connector mcp` line the bench prints (Codex passes an MCP server only the
  variables it is given), then `. P/bench/env.sh && codex`. If `voice_connect` says it cannot tell which conversation
  this is, this Codex does not give its thread ID to MCP servers: note the thread ID (`/status`), quit, register again
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

`service install|uninstall|start|stop|restart|status` (`src/service/`) runs two jobs of this user's service manager:
the core (`dev.sidevoice.core` under launchd, `sidevoice-core.service` under the systemd user manager) and the
connector daemon (`dev.sidevoice.connector`, `sidevoice-connector.service`). The core job runs
`R/current/core/bin/sidevoice-core-rust` with `idle-exit 0`; the connector job runs `D/install.json`'s `command` +
`connector`. Neither job starts or signals the other. Under systemd, each job must also be enabled for the next login
(`enable`, checked with `is-enabled`) and a changed definition read again (`daemon-reload`): a refusal of either fails
the command (`service.not-enabled`), even when the job would run now. A person's stop (`D/node-stopped.json`) is written before the
manager is asked and holds until `service start` or the next login; every change holds `D/install.lock`. On Linux,
`service install` says whether the user lingers and prints `loginctl enable-linger <user>`; it never runs it.

`service status --json`, and a daemon's `node.status`, is derived on every read from the manager, the core's
`core-failure.json` and its health: `absent`, `not-installed`, `stopped-by-person`, `starting`, `backoff`,
`running`, `failed` or `service-failed`, the states the desktop app reads.

With no manager (`SIDEVOICE_SERVICE_MANAGER=none`, a container, an `su` shell, Linux without a user bus) or no jobs
installed, an MCP server starts the connector from the selected installation, and the connector starts the core,
detached, and again whenever its link to it fails.
