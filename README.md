<!-- Header: .github/assets/readme-header*.svg, from the Sidevoice brand's banner. Badges: shieldcn
     (https://shieldcn.dev), each a light/dark pair so the row follows the reader's GitHub theme. -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/assets/readme-header-on-dark.svg" />
  <img alt="Sidevoice — Give your coding agent a voice. Keep the conversation." src=".github/assets/readme-header.svg" width="750" />
</picture>

<p>
  <a href="https://www.npmjs.com/package/sidevoice"><picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/npm/v/sidevoice.svg?variant=secondary&size=sm&mode=dark" /><img alt="npm version" src="https://shieldcn.dev/npm/v/sidevoice.svg?variant=secondary&size=sm&mode=light" /></picture></a>
  <a href="https://github.com/sidevoice/sidevoice-connector/actions/workflows/release.yml"><picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/github/ci/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&workflow=release.yml&branch=main&mode=dark" /><img alt="release build status" src="https://shieldcn.dev/github/ci/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&workflow=release.yml&branch=main&mode=light" /></picture></a>
  <a href="LICENSE"><picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/github/license/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&mode=dark" /><img alt="licence" src="https://shieldcn.dev/github/license/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&mode=light" /></picture></a>
  <picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/badge/node-18+.svg?variant=secondary&size=sm&logo=nodedotjs&mode=dark" /><img alt="requires Node.js 18 or newer" src="https://shieldcn.dev/badge/node-18+.svg?variant=secondary&size=sm&logo=nodedotjs&mode=light" /></picture>
  <picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/badge/status-beta.svg?variant=secondary&size=sm&mode=dark" /><img alt="status: beta" src="https://shieldcn.dev/badge/status-beta.svg?variant=secondary&size=sm&mode=light" /></picture>
</p>

# sidevoice-connector

Reading your coding agent's plans, diffs and summaries all day is tiring. **Sidevoice** turns the conversation you
already have with your agent into a voice call. The agent keeps its context and keeps writing as usual; it also
speaks its replies, and you answer by voice and can interrupt it — from the sofa or on a walk, not only at your desk.

**sidevoice-connector** is what you install on the machine where your agents run, published on npm as
[`sidevoice`](https://www.npmjs.com/package/sidevoice). It gives each agent conversation its voice
tools (an MCP server), delivers what you say into that same conversation, and installs and starts the
machine's [core](https://github.com/sidevoice/sidevoice-core).

## How it fits

Your **machine** is the computer where your coding agents run; a **device** is what you call from (the desktop
app, a browser).

| Piece | Role |
|---|---|
| **sidevoice-connector** (this repository) | Installed next to your agents: their voice tools, and what installs and runs the core at login. |
| [sidevoice-core](https://github.com/sidevoice/sidevoice-core) | The conversations and the voice pipeline, next to the agents. |
| [sidevoice-desktop](https://github.com/sidevoice/sidevoice-desktop) | The app you call from. |
| [sidevoice-web](https://github.com/sidevoice/sidevoice-web) | The call interface the app bundles; it can also be served as a static site. |

## Get started

You need macOS on Apple silicon, or Linux on x64 or arm64 with glibc (see [Status](#status)), and Node.js 18 or
newer for `npx`. The npm package `sidevoice` is a small launcher: npm installs beside it the connector built for
your machine (`@sidevoice/sidevoice-<os>-<cpu>`, with the core inside); installing downloads nothing else.

1. **Install**, on the machine where your agents run:

   ```sh
   npx sidevoice install
   ```

   It installs and starts this machine's core, registers the voice tools with the agents it finds, and tells you
   what, if anything, needs a manual step (see [Supported agents](#supported-agents)).

   A release candidate, when there is one: `npx sidevoice@next install`.

2. **Pair your device.** Ask your agent to pair a device, or run `npx sidevoice pair-device`. You get a
   one-time code; enter it in the [desktop app](https://github.com/sidevoice/sidevoice-desktop). Pairing is always
   your act: nothing pairs on its own.

3. **Talk.** In any conversation, ask the agent to join the voice call. It shows up in the app; speak to it.

## Supported agents

| Agent | Setup | What you say reaches the conversation |
|---|---|---|
| Claude Code | Registered by `install`. | Delivered into the running session. |
| Codex | Registered by `install` through the Codex CLI (without it, `install` prints what to add to `~/.codex/config.toml`); then restart Codex. | Queued into the thread with `codex queue`. |
| Cursor (experimental) | Registered by `install` in `~/.cursor/mcp.json`; approve the new MCP server once. | Cursor offers no route of its own. The CLI receives it only in a chat started with `cursor-agent persist` (needs tmux); the editor, while the small Sidevoice card stays open in that chat. |

Any other agent can receive through an HTTP endpoint of its own (`SIDEVOICE_DELIVERY_URL`).

## Status

Beta. What works today: the install above on macOS and Linux, pairing devices with the machine, voice calls with
your agents' conversations from the desktop app, interrupting a reply, and replies spoken while the agent keeps
working. Reaching your machine from outside your network needs a relay, which is still being built.

The connector's own binaries, published on [GitHub Releases](https://github.com/sidevoice/sidevoice-connector/releases),
run on macOS on Apple silicon and on Linux x86_64 and arm64 with **glibc 2.28 or newer** (Debian 10, Ubuntu 20.04,
RHEL 8 and their later releases, and most other distributions since 2019; not musl-based ones such as Alpine).

## Commands

The package installs one command, `sidevoice` (the release archive's binary is `sidevoice-connector`; same
commands). With `--json`, a command prints one JSON object on stdout, and a failure as a stable key with an
English message.

| Command | What it does |
|---|---|
| `install` | Installs this package's connector and the core it carries (or updates to them), starts both, checks they answer (or goes back to the previous version), then registers the voice tools with the agents it finds. `--no-agents` registers none. |
| `uninstall` | The reverse of `install`: stops Sidevoice, removes its service jobs and the agent registrations it wrote, then what it installed and its data. Agent configuration it did not write is never edited. |
| `pair-device` | Prints a one-time code (and its QR) that pairs a device with this machine. |
| `pair <room-url> <code>` | Pairs this machine with a room, with the one-time code the room shows. |
| `agents` | The agents on this machine and whether they use Sidevoice; `agents connect\|disconnect\|dismiss <claude\|codex\|cursor>` changes one. |
| `service <install\|start\|stop\|restart\|status\|uninstall>` | Sidevoice at login: the core and the connector as two jobs of launchd or the systemd user manager. Without a service manager, Sidevoice runs on demand. |
| `mcp` | The MCP server an agent starts, one per conversation. You do not run it yourself. |
| `connector` | The per-machine process the MCP servers share; started on demand, gone when the last conversation leaves. |
| `--version` | The version; with `--json`, also the target and the source commit. |

[`connector/README.md`](connector/README.md) describes each command, where things are kept and how the pieces talk
to each other.

## Develop

The connector is one Rust binary, the crate in [`connector/`](connector); the build tooling is `cargo xtask`
([`xtask/`](xtask)). You need the Rust toolchain pinned in [`.github/actions/setup`](.github/actions/setup/action.yml).

```sh
cargo xtask fixtures   # what the tests run against: the sidevoice-core release pinned in core.pin, the pinned Codex CLI
cargo test --locked    # unit tests, and the connector against the real core, Codex and the service managers
cargo xtask dist       # this machine's release archive, built and verified as a release builds it
                       # (Linux: needs zig and cargo-zigbuild, to link against glibc 2.28; RELEASING.md)
cargo xtask npm target/dist/sidevoice-connector-<target>.tar.zst && cargo xtask npm-smoke
                       # its npm packages, installed into a temporary prefix and run (needs Node.js)
cargo xtask bench      # the test bench: talk to a real Claude Code or Codex session through this build, no app
                       # needed (connector/README.md, "Test bench")
```

How a version is built, verified and published (GitHub Releases and npm): [`RELEASING.md`](RELEASING.md).

## Contributing

Issues and pull requests are welcome. Read [`AGENTS.md`](AGENTS.md) first: it holds the rules for code, texts and
tests, for people and coding agents alike. Pull request titles follow
[Conventional Commits](https://www.conventionalcommits.org) (CI checks them) and become the squashed commit, from
which release notes are written ([`RELEASING.md`](RELEASING.md)).

## Licence

[Apache-2.0](LICENSE). The Sidevoice name and logo are trademarks: forks are welcome under their own name — see
[`TRADEMARKS.md`](TRADEMARKS.md).

## Third-party components

The package redistributes its dependencies and the core under their own licences: [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
