# Sidevoice connector (Rust)

One binary, `sidevoice-connector`, that links this machine's agent conversations (Claude Code, Codex, Cursor) to the
Sidevoice core: it serves MCP to each conversation and runs the machine's connector daemon, which holds the link to
the core. It is migrating to stand alone (sidevoice/sidevoice-connector#68); until that is done, installing, the
service commands of an installed release and pairing are still the JavaScript package's (`packages/connector`).

## Commands

| Command | What it does |
| --- | --- |
| `mcp` | MCP over stdio for one conversation; it reaches the daemon over `connector.sock`. |
| `connector [--service]` | The daemon: one per data directory. `--service` (or `SIDEVOICE_SERVICE=launchd\|systemd`) when a service manager runs it. |
| `agents [--json]` | The agents on this computer and whether they use Sidevoice. |
| `agents connect\|disconnect\|dismiss <claude\|codex\|cursor> [--json]` | Registers Sidevoice with an agent, removes its registration, or dismisses the new-agent notice. Only an entry Sidevoice wrote is ever changed. |
| `service <install\|start\|stop\|restart\|status\|uninstall> --json` | The login service (being rebuilt: launchd and systemd). |
| `--version [--json]` | The release version; with `--json`, also the target and the source commit. |
| `--installed` | Run as the installed release this binary is part of (below). |

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

## Installed release

A release built by the JavaScript installer can carry this binary as its daemon and MCP server
(`<R>/releases/<id>/dist/sidevoice-rust`, run as `--installed mcp` and `--installed connector` through `<R>/current`).
At startup it checks again that it is the current release's binary: the release record, the paths, both
executables' digests, the target and the pair identity. That layout is being replaced by the connector's own
installer (sidevoice/sidevoice-connector#66, decision 3).

## Tests

`cargo test` runs the unit tests and, in `tests/`, this binary against the sidevoice-core release pinned in
`core.pin` at the repository root (a conversation joins through MCP, input from a call is delivered, a reply is
saved, the core restarts and the link comes back), against the real Codex CLI in a disposable profile, and, on
macOS, as a login service under the real launchd. `cargo xtask fixtures` fetches the core and Codex; without them
those tests are skipped locally and fail in CI.

Codex 0.157.0 does not give the active thread ID in MCP request metadata, so queued delivery into a Codex
conversation needs `CODEX_THREAD_ID` in its environment. A queued message can be accepted before Codex reads it; the
connector sends `input.read` only after the same message appears in that thread's rollout.

The local speech outbox is synced to disk. Core's `text_saved: true` acknowledges admission into its current-process
journal, which the core keeps in memory; the connector removes an outbox item on that acknowledgement, so a core
crash immediately afterwards can lose that speech text ([Core issue #43](https://github.com/sidevoice/sidevoice-core/issues/43)).
A speech row copied from the JavaScript connector has no conversation reference; if the core later remints its
binding ID, it is retained and reported rather than assigned to another conversation
([Connector issue #41](https://github.com/sidevoice/sidevoice-connector/issues/41)).
