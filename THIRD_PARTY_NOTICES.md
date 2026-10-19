# Third-party components

Sidevoice's Apache-2.0 licence applies to its own code. The components it ships keep their original licences, and
every release carries their notices:

- **The connector binary** links Rust crates (among them Tokio, Tungstenite, rustls, rmcp, clap, serde and
  qrcodegen). Each release archive, and each npm platform package made from it, carries the licence expression and
  licence texts of every crate linked into the binary on that target, in `notices/` (`rust-dependencies.json` and
  `licenses/`, written by `cargo xtask dist`, `xtask/src/notices.rs`).
- **The core** ([sidevoice-core](https://github.com/sidevoice/sidevoice-core)) travels inside the package as the
  core release's own archive, unchanged. Its components, models and their notices are inside that archive and
  described by the core's repository.
- **The npm launcher** `sidevoice` is one script of ours with no dependencies.

The repository's Apache-2.0 licence does not replace those terms.
