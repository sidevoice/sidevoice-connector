# AGENTS.md

Rules for any coding agent (and person) working in this repository.

## Language of the code and of the product

- Code, identifiers, comments, commit messages and docs are in **English**.
- **The command line is English only and is not translated.** Its text for people (help, results, refusals on
  stderr) is English, written where it is said. No other language, Spanish included, is ever hard-coded.
- **What a program reads carries keys.** Machine-readable output (`--json`, answers over the connector socket and
  the core link) says a failure as a stable key, its parameters and an English message:
  `{"ok":false,"error":{"key","params","message"}}`. The client translates by key; the message is never parsed.
  The keys and their English texts live in `connector/messages/errors.json`; adding a keyed failure means adding
  its key there.
- What reaches the agent (MCP instructions, tool results, the `[Sidevoice]` trailer) is English; it is not UI.

## Before changing things

Read `connector/README.md` (what the binary does and where things are) and `RELEASING.md` (how it is built,
tested and published). The repository is Rust only: the connector is the crate in `connector/`, the build tooling
is `cargo xtask` (`xtask/`). The one JavaScript file, `xtask/npm/sidevoice.js`, is the npm launcher: plain, no
dependencies, it only runs the platform package's binary. Nothing else is written in JavaScript or Python. The web
client's move to English-keyed bundles is sidevoice/sidevoice-web#17.

## The protocol evolves like a production API

The core and the room, the connector daemon and an agent's `sidevoice mcp`, and the agents themselves run different
versions: a machine installed long ago, a conversation keeping an older `sidevoice mcp`, a newer core
(sidevoice/sidevoice-core#112). Every boundary the connector speaks (the core link, the IPC over `connector.sock`, the
MCP tools, the files another version may have written) follows these rules:

- **Allowed without a version bump:** new optional fields (requests, answers, frames, files), new methods and
  commands, new enum values with a default behaviour for readers that do not know them.
- **Never:** removing or renaming a field, method or command, changing what one means, making an optional field
  required. To change something: add the new one, mark the old one deprecated, remove it only when no supported
  version uses it.
- **Every reader tolerates what it does not know.** No `#[serde(deny_unknown_fields)]` on a wire or file type, no
  `additionalProperties: false` in a tool schema; an unknown field is ignored, and a whole message is never refused
  for one. An unknown enum value gets a defined default (`#[serde(other)]`, a default variant, a `_ =>` arm that does
  something sensible), written down where it is read: an unknown `channel` is delivered as `voice`, an unknown close
  reason reads as closed from the room, a delivery route kind a harness does not know joins the conversation with no
  route in (deliver unsupported; its speech still goes out), a speech answer that is neither saved nor a refusal it
  knows keeps the speech in the outbox, an unknown method or command gets a defined refusal
  (`connector.unknown-method`, `unknown IPC method`). A `null` optional field reads as an absent one.
- **Tool schemas name no closed lists.** An argument whose values may grow (a language) is a string whose description
  lists the values known now; a client that checks calls against the schema must not refuse the next one.
- **A library's closed types are not ours to impose.** Where a dependency reads a peer's message into a closed enum
  (rmcp's client info: an icon `theme` of `light` or `dark` only), the message is normalised to what Sidevoice reads
  before the library sees it (`mcp.rs`, `tolerant_message`).
- **What stays strict:** the fields a message cannot be acted on without, and the checks that protect someone (the
  JSON-RPC envelope, IDs this side never sent, the link's protocol version, an agent registration this connector
  did not write, file ownership and permissions). An unknown value there is refused, never guessed.
- **A protocol version number** (the link's `protocol`) changes only for a genuinely breaking change, so the other
  side can say "update this machine" instead of failing.
- **Contract tests** send each boundary's messages with extra fields, top level and nested, and unknown enum values,
  and assert they are accepted and behave per their known part (`src/daemon/contract_tests.rs`, `src/link.rs`,
  `tests/mcp_interop.rs`, `tests/ipc_interop.rs`). A new message or field gets its case there.

## Public product information

For changes to user-visible behavior, supported platforms/models, setup, security/privacy practices, availability, limitations, or release/download details, follow the shared [public-information process](https://github.com/sidevoice/landing/blob/main/AGENTS.md#keep-public-product-information-current). Record the landing change/PR or a linked `sidevoice/landing` issue in the PR checklist. Landing issues are the follow-up triage queue.
