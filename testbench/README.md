# Test bench

`cargo xtask testbench` (or `cargo xtask bench`) drives this build and the pinned core from a page on loopback, without the desktop app, so that a real Claude Code or Codex session can be talked to by typing. It is a developer tool
(this package, `testbench/`, binary `sidevoice-testbench`, `publish = false`), never part of a release. It runs on macOS and Linux only (it stands in for a person's login shell with a script, which Windows does not read).

When it starts, it:

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

Pointing an agent at it (the paths are printed by the test bench; `P` is the profile). The agent runs with the profile's
`bench/env.sh` sourced, so that its own home (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) is the one the connector reads, and
its MCP server is the installation's command with the profile's variables:

- **Claude Code**: `. P/bench/env.sh && claude --mcp-config P/bench/mcp.json --strict-mcp-config`. The
  profile's Claude home starts empty: log in once there (`/login`), and accept the folder's trust prompt.
  `--strict-mcp-config` keeps any other Sidevoice registration out of the session. Then ask it to join the voice
  call. If `voice_connect` reports `inbound.ok: false`, Claude Code will not take messages from another session in
  this permission mode: follow its remedy in the profile's settings or use a prompting permission mode.
- **Codex**: `. P/bench/env.sh && codex login` once, then register the server once with the `codex mcp add sidevoice
  --env ... -- .../current/bin/sidevoice-connector mcp` line the test bench prints (Codex passes an MCP server only the
  variables it is given), then `. P/bench/env.sh && codex`. If `voice_connect` says it cannot tell which conversation
  this is, this Codex is too old to name its thread in each call: note the thread ID (`/status`), quit, register again
  with `--env CODEX_THREAD_ID=<id>` added, and `codex resume <id>`.

Nothing outside the profile is touched: the operator's own `~/.claude`, `~/.codex` and Sidevoice installation are
neither read nor written, and an installed Sidevoice keeps running beside it (the test bench's core takes any free port).

To open the page from another device, the `frp-tunnel` skill can publish the port; start the test bench with
`--allow-host <the tunnel's host name>` so that the page answers to it (it refuses any other `Host`). The tunnel's URL
is public and unauthenticated: whoever has it can type into the agents that joined. Close it when done.

`tests/testbench.rs` runs the test bench's own path in `cargo test`: the test bench installs into a private profile, a
conversation (with an HTTP receiver as its harness) joins through the installation's MCP server, what is typed on
the test bench is delivered to it, and its reply and the input's receipt show on the test bench.
