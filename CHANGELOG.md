# Changelog

## [0.7.1](https://github.com/sidevoice/sidevoice-connector/compare/v0.7.0...v0.7.1) (2026-10-09)


### Bug Fixes

* **mcp:** answer current Claude Code's server/discover with valid lists (rmcp 3.5.1) ([#87](https://github.com/sidevoice/sidevoice-connector/issues/87)) ([134f993](https://github.com/sidevoice/sidevoice-connector/commit/134f993e6dfc87c62678c1fce7d67f672e78b204))

## [0.7.0](https://github.com/sidevoice/sidevoice-connector/compare/v0.6.0...v0.7.0) (2026-10-06)


### Features

* add host agent management to connector ([#33](https://github.com/sidevoice/sidevoice-connector/issues/33)) ([0a4c62b](https://github.com/sidevoice/sidevoice-connector/commit/0a4c62bd1a28e094d2986cc7c76043c3b0b858eb))
* add isolated Rust connector proof ([#40](https://github.com/sidevoice/sidevoice-connector/issues/40)) ([ea27002](https://github.com/sidevoice/sidevoice-connector/commit/ea27002d10498b4bf29df0f0dab7e527dcaa8b91))
* add proof-private Rust host agents ([#43](https://github.com/sidevoice/sidevoice-connector/issues/43)) ([02a2f06](https://github.com/sidevoice/sidevoice-connector/commit/02a2f06fedcfacb74ae5a4b139c4fa8881c2a3c6))
* add Rust conversation adapters for Claude and Cursor ([#52](https://github.com/sidevoice/sidevoice-connector/issues/52)) ([c0468ba](https://github.com/sidevoice/sidevoice-connector/commit/c0468ba8a705661dc30c5ebc2b743eec7e370fa5))
* **bench:** a test bench in Rust to talk to real agent sessions without the app ([#84](https://github.com/sidevoice/sidevoice-connector/issues/84)) ([d778ee3](https://github.com/sidevoice/sidevoice-connector/commit/d778ee36e43ad562242b0f5089814ae6bcd48a2d))
* **browser-audio:** the page's models come from sidevoice-core's catalogue, and its resolver in TypeScript ([1757c24](https://github.com/sidevoice/sidevoice-connector/commit/1757c24011ddb0bd80fd48641d9d39329078c8e0)), closes [#124](https://github.com/sidevoice/sidevoice-connector/issues/124)
* **connector:** Cursor CLI as a harness — the room hears it, and cannot talk to it ([b5eb9e4](https://github.com/sidevoice/sidevoice-connector/commit/b5eb9e4c9fbaef1541c3978c11a7033ff54dbdf5))
* **connector:** from a checkout, install the core wheel put beside the sources ([7df3bbc](https://github.com/sidevoice/sidevoice-connector/commit/7df3bbceac24e427039b3028d20feca1b58fbb25))
* **connector:** install and supervise this machine's core, and link to it over loopback ([23c23f7](https://github.com/sidevoice/sidevoice-connector/commit/23c23f7870085f9a5533f89b5a1a51ada36e46c5))
* **connector:** install and uninstall in Rust, with automatic rollback and agent registration ([#78](https://github.com/sidevoice/sidevoice-connector/issues/78)) ([bd1e64c](https://github.com/sidevoice/sidevoice-connector/commit/bd1e64c79147d0fc3aea6d0db520836815fcfbcc))
* **connector:** pair and pair-device in Rust, with the device code's QR ([#73](https://github.com/sidevoice/sidevoice-connector/issues/73)) ([85504f2](https://github.com/sidevoice/sidevoice-connector/commit/85504f25793d42c187b2b761e7407297d792d720))
* **connector:** pair this machine with a room when its core asks ([e99915d](https://github.com/sidevoice/sidevoice-connector/commit/e99915da7a3c01de9d4c373f13f389162baea51e))
* **connector:** service management in Rust (launchd, systemd, on demand) ([#71](https://github.com/sidevoice/sidevoice-connector/issues/71)) ([55568c8](https://github.com/sidevoice/sidevoice-connector/commit/55568c8fc25539a7062fe83858e310d7a22cd405))
* **connector:** ship source-built Rust Connector and Core pair ([435fd31](https://github.com/sidevoice/sidevoice-connector/commit/435fd315e657a4f1372fc524f773387797195f38))
* **connector:** Sidevoice at login as two native jobs; immutable releases with atomic switch and rollback (onboarding R1-b) ([#25](https://github.com/sidevoice/sidevoice-connector/issues/25)) ([249ca5e](https://github.com/sidevoice/sidevoice-connector/commit/249ca5e0ffb052a0832236981561f1c53afb088b))
* **connector:** the core travels inside the package, staged and self-tested at install ([#74](https://github.com/sidevoice/sidevoice-connector/issues/74)) ([72902e4](https://github.com/sidevoice/sidevoice-connector/commit/72902e4564c6389bee0a53168433ce1db7b1fc95))
* **cursor:** experimental delivery — tmux for persist chats, an MCP App view for the editor ([10466d7](https://github.com/sidevoice/sidevoice-connector/commit/10466d799225676e8e214553c431ba5d6f618b84))
* **cursor:** say why an editor chat has no card, and log what Cursor declared ([d30df94](https://github.com/sidevoice/sidevoice-connector/commit/d30df94cf4aaf8b384df6ab47e36e4b04ca7c4d1))
* **cursor:** several editor chats of one window join the room at once ([ae8d793](https://github.com/sidevoice/sidevoice-connector/commit/ae8d7935c26438d3dd46a7f6c5858a82f3270d98))
* **cursor:** two ticks for editor chats, and the room shows Cursor and its route ([cabd3ca](https://github.com/sidevoice/sidevoice-connector/commit/cabd3ca4944a8423cd57d7bf730524d68e99ccc2))
* **cursor:** voice into an editor chat by its id, through Cursor's Desktop Bridge ([2aab66a](https://github.com/sidevoice/sidevoice-connector/commit/2aab66a0726c8ca771bf91d6d918ddbb8a0dad71))
* implement Sigstore-verified Node SEA core installation ([#29](https://github.com/sidevoice/sidevoice-connector/issues/29)) ([c509c95](https://github.com/sidevoice/sidevoice-connector/commit/c509c95d2d54e933f3c1041f5ebbca4b68c9019d))
* **install:** install, start and health-check this machine's core before registering ([7d26d4f](https://github.com/sidevoice/sidevoice-connector/commit/7d26d4f512832e063ab889e797c4dd0d4e398d39))
* **mcp:** with no room paired, a conversation joins this machine's core ([4f383b3](https://github.com/sidevoice/sidevoice-connector/commit/4f383b3570bb4f7db84a28cabf302ddeb4be6034))
* **npm:** publish the connector as sidevoice with one package per platform ([#75](https://github.com/sidevoice/sidevoice-connector/issues/75)) ([f42cc7d](https://github.com/sidevoice/sidevoice-connector/commit/f42cc7d7c765a7f0cf5ac437ae8b2ddd56076b9f))
* **room, connector:** device pairing — relay it, pass the token, issue codes from the node ([8ec1657](https://github.com/sidevoice/sidevoice-connector/commit/8ec16576d0b19e5d8e7322715059b6e9175f9d21))
* **rust:** add private macOS service lifecycle proof ([#47](https://github.com/sidevoice/sidevoice-connector/issues/47)) ([f2589c7](https://github.com/sidevoice/sidevoice-connector/commit/f2589c7cbe8f5037e6f1d85e177f2d0cb4db196c))
* the microphone over WebRTC, signalled through the room, with the relay as the fallback ([9707e5d](https://github.com/sidevoice/sidevoice-connector/commit/9707e5d20e6f37330b66ab4103b6b5f29a0a0829))
* the room without the core — rendezvous and relay between browsers and machines ([d7da067](https://github.com/sidevoice/sidevoice-connector/commit/d7da067a085f27bd571814faea6bf69fcbce85d2))


### Bug Fixes

* **connector:** give voice_pair_device time for a core that is still starting ([#82](https://github.com/sidevoice/sidevoice-connector/issues/82)) ([1e39517](https://github.com/sidevoice/sidevoice-connector/commit/1e3951711e3ec5cc5e661d545c9d17f64dc15f5a))
* **connector:** pin core 0.2.1, which runs on glibc 2.28 ([#85](https://github.com/sidevoice/sidevoice-connector/issues/85)) ([2096803](https://github.com/sidevoice/sidevoice-connector/commit/2096803eb9c126eb2a316c9269131fed026eee05))
* **core:** create the core's environment with uv's own Python, never the machine's ([#20](https://github.com/sidevoice/sidevoice-connector/issues/20)) ([6d5e27e](https://github.com/sidevoice/sidevoice-connector/commit/6d5e27e92c314e0251f7092b8fac9601c5a0e627))
* **cursor:** a second chat of a window no longer takes the first one's voice ([782d2c1](https://github.com/sidevoice/sidevoice-connector/commit/782d2c1eb8891a7f0724179bce7750bacd3580b1))
* **cursor:** an editor chat can speak first; log every method Cursor calls ([c8609dd](https://github.com/sidevoice/sidevoice-connector/commit/c8609dd14d4ff4b77952a347bf184fa44dd37af2))
* **cursor:** identify the editor chat by its transcript, and log every bridge step ([c31657d](https://github.com/sidevoice/sidevoice-connector/commit/c31657d1d3ced5f5b4aea18a73d05ce104d2eeda))
* **cursor:** what the quick review found in speaking first ([e139d66](https://github.com/sidevoice/sidevoice-connector/commit/e139d66e2448195093ae861112bcfef92ace4bf7))
* **cursor:** what the quick review found in the Desktop Bridge route ([0178bce](https://github.com/sidevoice/sidevoice-connector/commit/0178bce666d2d140f3662a5c014617f96d67d781))
* **cursor:** what the review found — kept mcp.json, rejoin, rewrites, no retries ([ab6e407](https://github.com/sidevoice/sidevoice-connector/commit/ab6e407c086d0f371cfa29a1bec68a9ee673e790))
* **cursor:** what the review found in several chats per window ([787bfce](https://github.com/sidevoice/sidevoice-connector/commit/787bfce9a52b5ea168011e43c4f00f20d58f915a))
* **cursor:** what the review found in the experimental routes ([3ee97e0](https://github.com/sidevoice/sidevoice-connector/commit/3ee97e0c176db1138667a604944a618a897cae89))
* HTTPS-only pairing in the web, honest relay docs, explicit cluster allowlist in the connector ([#134](https://github.com/sidevoice/sidevoice-connector/issues/134)) ([9df6a53](https://github.com/sidevoice/sidevoice-connector/commit/9df6a535f8ec6b621d40e8a88df978873f0290a8))
* install dependencies before desktop pin verification ([#31](https://github.com/sidevoice/sidevoice-connector/issues/31)) ([86a0ab1](https://github.com/sidevoice/sidevoice-connector/commit/86a0ab1a5363af303b25f77fc7254aef5db1fea3))
* **install:** with no room paired, say that voice reaches this machine's own app, not that a room's code is needed ([fdb7d4a](https://github.com/sidevoice/sidevoice-connector/commit/fdb7d4ac1e8ba33c6c0528455bba0e0a6474a6cb))
* pin actual core attestation builder identity ([#30](https://github.com/sidevoice/sidevoice-connector/issues/30)) ([30c1757](https://github.com/sidevoice/sidevoice-connector/commit/30c175715d95d45d73b847a73328cb8dcf622b1a))
* protect queued speech outbox ([#44](https://github.com/sidevoice/sidevoice-connector/issues/44)) ([7c86f28](https://github.com/sidevoice/sidevoice-connector/commit/7c86f28766b7a844f23f0b369a0222101072c1c4))
* what the adversarial review confirmed ([0f70fbb](https://github.com/sidevoice/sidevoice-connector/commit/0f70fbb199b1590e584944950eaed084abd4faa8))
