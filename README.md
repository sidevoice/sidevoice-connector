<!-- Header: .github/assets/readme-header*.svg, from the Sidevoice brand's banner. Badges: shieldcn
     (https://shieldcn.dev), each a light/dark pair so the row follows the reader's GitHub theme. -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/assets/readme-header-on-dark.svg" />
  <img alt="Sidevoice — Give your coding agent a voice. Keep the conversation." src=".github/assets/readme-header.svg" width="750" />
</picture>

<p>
  <a href="https://www.npmjs.com/package/@sidevoice/uplink"><picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/npm/v/@sidevoice/uplink.svg?variant=secondary&size=sm&mode=dark" /><img alt="npm version" src="https://shieldcn.dev/npm/v/@sidevoice/uplink.svg?variant=secondary&size=sm&mode=light" /></picture></a>
  <a href="https://github.com/sidevoice/sidevoice-connector/actions/workflows/ci.yml"><picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/github/ci/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&workflow=ci.yml&branch=main&mode=dark" /><img alt="CI status" src="https://shieldcn.dev/github/ci/sidevoice/sidevoice-connector.svg?variant=secondary&size=sm&workflow=ci.yml&branch=main&mode=light" /></picture></a>
  <picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/badge/node-22+.svg?variant=secondary&size=sm&logo=nodedotjs&mode=dark" /><img alt="requires Node.js 22 or newer" src="https://shieldcn.dev/badge/node-22+.svg?variant=secondary&size=sm&logo=nodedotjs&mode=light" /></picture>
  <picture><source media="(prefers-color-scheme: dark)" srcset="https://shieldcn.dev/badge/status-beta.svg?variant=secondary&size=sm&mode=dark" /><img alt="status: beta" src="https://shieldcn.dev/badge/status-beta.svg?variant=secondary&size=sm&mode=light" /></picture>
</p>

# sidevoice-connector

Reading your coding agent's plans, diffs and summaries all day is tiring. **Sidevoice** turns the conversation you
already have with your agent into a voice call. The agent keeps its context and keeps writing as usual; it also
speaks its replies, and you answer by voice and can interrupt it — from the sofa or on a walk, not only at your desk.

**sidevoice-connector** is what you install on the machine where your agents run, published on npm as
[`@sidevoice/uplink`](https://www.npmjs.com/package/@sidevoice/uplink). It gives each agent conversation its voice
tools (an MCP server), delivers what you say into that same conversation, and installs, starts and supervises the
machine's [core](https://github.com/sidevoice/sidevoice-core).

## How it fits

Your **machine** is the computer where your coding agents run; a **device** is what you call from (the desktop
app, a browser).

| Piece | Role |
|---|---|
| **sidevoice-connector** (this repository) | Installed next to your agents: their voice tools and the core's supervisor. |
| [sidevoice-core](https://github.com/sidevoice/sidevoice-core) | The conversations and the voice pipeline, next to the agents. |
| [sidevoice-desktop](https://github.com/sidevoice/sidevoice-desktop) | The app you call from. |
| [sidevoice-web](https://github.com/sidevoice/sidevoice-web) | The call interface the app bundles; it can also be served as a static site. |

## Get started

You need Node.js 22 or newer and [uv](https://docs.astral.sh/uv/) (the core runs on Python, which uv provides).

1. **Install**, on the machine where your agents run:

   ```sh
   npx -y @sidevoice/uplink install
   ```

   It installs and starts this machine's core, registers the voice tools with the agents it finds, and tells you
   what, if anything, needs a manual step (see [Supported agents](#supported-agents)).

2. **Pair your device.** Ask your agent to pair a device, or run `npx @sidevoice/uplink pair-device`. You get a
   one-time code; enter it in the [desktop app](https://github.com/sidevoice/sidevoice-desktop). Pairing is always
   your act: nothing pairs on its own.

3. **Talk.** In any conversation, ask the agent to join the voice call. It shows up in the app; speak to it.

## Supported agents

| Agent | Setup | What you say reaches the conversation |
|---|---|---|
| Claude Code | Registered by `install`. | Delivered into the running session. |
| Codex | `install` prints the lines to add to `~/.codex/config.toml`; then restart Codex. | Queued into the thread with `codex queue`. |
| Cursor (experimental) | Registered by `install` in `~/.cursor/mcp.json`; approve the new MCP server once. | Cursor offers no route of its own. The CLI receives it only in a chat started with `cursor-agent persist` (needs tmux); the editor, while the small Sidevoice card stays open in that chat. |

Any other agent can receive through an HTTP endpoint of its own (`SIDEVOICE_DELIVERY_URL`).

## Status

Beta. What works today: the install above on macOS and Linux, pairing devices with the machine, voice calls with
your agents' conversations from the desktop app, interrupting a reply, and replies spoken while the agent keeps
working. Reaching your machine from outside your network needs a relay, which is still being built.

## Commands

The package installs one command, `sidevoice`:

| Command | What it does |
|---|---|
| `install` | Installs this machine's core, starts it, checks it answers, then registers the voice tools. `--no-core` skips the core. |
| `uninstall` | The reverse of `install`: unregisters the voice tools, stops the connector and removes what it installed. Agent configuration files it did not write are never edited; it prints what to remove. |
| `pair-device` | Prints a one-time code (and its QR) that pairs a device with this machine. |
| `mcp` | The MCP server an agent starts, one per conversation. You do not run it yourself. |
| `connector` | The per-machine process the MCP servers share; started on demand, gone when the last conversation leaves. |

[`packages/connector/README.md`](packages/connector/README.md) describes how the pieces talk to each other.

## Develop

```sh
npm ci
npm test               # node --test, on the source
npm run build          # the bundled dist/cli.mjs that npm publishes
```

The published package has no runtime dependencies: esbuild bundles everything into `dist/cli.mjs`.
`SIDEVOICE_CORE_WHEEL=<wheel> npm run build` embeds the pinned core's wheel; installing it still lets uv download
Python and the core's dependencies.

## Contributing

Issues and pull requests are welcome. Read [`AGENTS.md`](AGENTS.md) first: it holds the rules for code, texts and
tests, for people and coding agents alike. Pull request titles follow
[Conventional Commits](https://www.conventionalcommits.org) (CI checks them) and become the squashed commit, from
which release notes are written ([`RELEASING.md`](RELEASING.md)).

## Licence

To be decided — [sidevoice/.github#11](https://github.com/sidevoice/.github/issues/11).

## Third-party components

The bundle redistributes its dependencies under their own licences: [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
