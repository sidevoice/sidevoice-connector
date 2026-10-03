# AGENTS.md

Rules for any coding agent (and person) working in this repository.

## Language of the code and of the product

- Code, identifiers, comments, commit messages and docs are in **English**.
- **Every user-facing text goes through i18n**: a key looked up in per-language message bundles (one file per
  language), with **English as the fallback** when a key is missing. No literal sentence in any language in
  components, server refusals and errors shown to people, native UI (tray, windows, notifications, installer or CLI
  output meant for people). Adding a text means adding its key to the English bundle; other languages may lag.
- Language defaults: the device's system language when we support it, otherwise English. Never Spanish, or any
  other language, as a hard-coded default.
- What reaches the agent (MCP instructions, tool results, the `[Sidevoice]` trailer) is English; it is not UI.

## Before changing things

Read `packages/connector/README.md`. New text follows the rule above; the web client's move to English-keyed
bundles is sidevoice/sidevoice-web#17.

## Public product information

For changes to user-visible behavior, supported platforms/models, setup, security/privacy practices, availability, limitations, or release/download details, follow the shared [public-information process](https://github.com/sidevoice/landing/blob/main/AGENTS.md#keep-public-product-information-current). Record the landing change/PR or a linked `sidevoice/landing` issue in the PR checklist. Landing issues are the follow-up triage queue.
