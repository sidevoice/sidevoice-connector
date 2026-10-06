# AGENTS.md

Rules for any coding agent (and person) working in this repository.

## Language of the code and of the product

- Code, identifiers, comments, commit messages and docs are in **English**.
- **The command line is English only and is not translated.** Its text for people (help, results, refusals on
  stderr) is English, written where it is said. No other language, Spanish included, is ever hard-coded.
- **What a program reads carries keys.** Machine-readable output (`--json`, answers over the connector socket and
  the core link) says a failure as a stable key, its parameters and an English message:
  `{"ok":false,"error":{"key","params","message"}}`. The client translates by key; the message is never parsed.
  The keys and their English texts live in `packages/connector-rust/messages/errors.json`; adding a keyed failure
  means adding its key there.
- **UI keeps per-language bundles.** Text shown in an interface we draw (the Cursor card) goes through a key looked
  up in per-language message bundles (`packages/connector-rust/messages/card/`), with English as the fallback when
  a key is missing; other languages may lag. Language default: the device's language when we support it,
  otherwise English.
- What reaches the agent (MCP instructions, tool results, the `[Sidevoice]` trailer) is English; it is not UI.

## Before changing things

Read `packages/connector-rust/README.md`. The web client's move to English-keyed bundles is
sidevoice/sidevoice-web#17.
