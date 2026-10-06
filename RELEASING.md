# Releasing

One version for the connector, tagged `vX.Y.Z`. It lives in `connector/Cargo.toml` (the build tooling at
the workspace root carries the same one, and `Cargo.lock` both); release-please moves them together
(`release-please-config.json`). Never edit it by hand. The connector is distributed as GitHub Releases of this
repository, one archive per target, and on npm as the package `sidevoice` with one package per platform built from
those very archives ([npm](#npm-what-is-published-and-how-to-add-a-platform)).

The earlier Node package, `@sidevoice/uplink` on npm, is deprecated and no longer in this repository.

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | `ci`: format and Clippy (Linux and macOS), then on every target the tests, the release packaging (`cargo xtask dist`) and its npm packages, installed and run (`cargo xtask npm`, `npm-smoke`), publishing nothing. **PR title is a conventional commit**. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `release` runs: per target it runs the tests, builds and packages the connector; then it attests the assets, attaches them to the `nightly` pre-release, reads them back, verifies them and publishes. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). |
| Merge the release PR | a maintainer | **This is the release.** release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `release` runs from the tag, attaches and verifies the assets, and publishes the Release; then it publishes the platform packages to npm and **stages** the launcher `sidevoice`. |
| Approve the staged `sidevoice` on npmjs.com | a maintainer (2FA) | The version reaches people: `npx sidevoice` installs it (dist-tag `latest`, or `next` for a candidate). |

The tests are part of the build: an asset is only produced on a target where the whole suite passed. Everything
besides the GitHub steps is code in `xtask/` (`cargo xtask fixtures | dist | verify | verify-floor | manifest |
publish | npm | npm-smoke | npm-publish`):

- `cargo xtask fixtures` fetches what the tests run against: the sidevoice-core release whose version is in
  `core.pin` (one line, `X.Y.Z`), its archive for this machine checked against that release's `SHA256SUMS`,
  `native-core-manifest.json` and `attestation.sigstore.json` (signed by the core's `release.yml` on `main`), into
  `target/sidevoice-core` (unpacked, with the checked archive beside it); and the pinned Codex CLI into
  `target/codex`. Without them the tests that need them are skipped locally and fail in CI.
- `cargo test --locked` runs the unit tests and the integration tests in `connector/tests/`: the
  connector against the real core, registration with the real Codex CLI, and (macOS) the login service under the
  real launchd.
- `cargo xtask dist` builds this machine's release binary and packages it exactly as the release does, with the
  pinned core release's archive for this machine (fetched and checked as `fixtures` checks it), then verifies the
  archive by unpacking it elsewhere and running the connector from there: its version and build identity, and
  `stage-core`, which stages the core it carries into a release where the core passes its own self-test. A pull
  request already runs it on every target. On Linux it needs [zig](https://ziglang.org) and
  [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) on the `PATH` (CI installs the versions pinned in
  `.github/actions/setup`): see [Linux: the glibc floor](#linux-the-glibc-floor).
- `cargo xtask verify-floor <archive>` (Linux, needs Docker) verifies a Linux archive again, running the connector
  in a container of the oldest distribution it supports. CI runs it after `dist` on both Linux targets.
- `cargo xtask npm <archive>...` makes the npm packages of those archives and the launcher, in `target/npm`;
  `cargo xtask npm-smoke` installs the launcher and this machine's package from there into a temporary prefix, as a
  person installs them, and runs `npx sidevoice --version --json`: it must be that package's build. It also checks
  the exit status passes through and that another platform gets a clear refusal. CI runs both after `dist` on every
  target. See [npm](#npm-what-is-published-and-how-to-add-a-platform).

## Linux: the glibc floor

A Linux binary only starts where the system's C library is at least as new as the newest glibc symbol it was linked
against. Built plainly on the `ubuntu-24.04` runners, the connector would need glibc 2.39 and refuse to start on
most systems still in use. So the floor is fixed in `xtask/src/glibc.rs` (`FLOOR`, today **glibc 2.28**: Debian 10,
Ubuntu 20.04, RHEL/AlmaLinux 8, Amazon Linux 2023 and every later release) and enforced:

- `dist` links the Linux binary with `cargo zigbuild --target <arch>-unknown-linux-gnu.2.28` (zig's glibc 2.28
  stubs instead of the runner's library) and records the floor in the archive's inventory (`"glibc": "2.28"`).
- `verify` reads the binary's GLIBC symbol versions (`readelf --version-info`) and fails if any is newer than the
  floor the inventory records; its report gives the floor and the newest version the binary really needs.
- `verify-floor` runs the connector in `almalinux:8` (pinned by digest in `glibc.rs`), checking first that the
  container's glibc is exactly the floor.

Raising or lowering the floor is a change of `FLOOR` and `FLOOR_IMAGE` together, and of the requirements in the
README. Only the connector binary is covered: the tests still run on the `ubuntu-24.04` runners because the
sidevoice-core release they run against (`core.pin`) has its own floor. For the same reason `verify-floor` does not
stage or run the core the archive carries; `verify` does, on the build machine. Until a core release with the same
floor is pinned, the package as a whole needs the core's floor.

## Assets

- `sidevoice-connector-<version>-<target>.tar.zst` for `macos-aarch64`, `linux-x86_64` and `linux-aarch64` (on the
  nightly, `sidevoice-connector-nightly-<target>.tar.zst`, fixed names whose download URLs never change): the root
  `sidevoice-connector/` holds `bin/sidevoice-connector`, `LICENSE`, the licence notices of every crate linked into
  the binary (`notices/`), `core/sidevoice-core-<core version>-<target>.tar.zst`, the pinned core release's archive
  for the same target exactly as the core published it (its own licence notices are inside it), and
  `connector.json`, the inventory: version, target, source commit, on Linux the glibc floor, every file with its
  size and digest, and `core`: the core's version, archive, digest, size and source commit.
- `sidevoice-connector-manifest.json`: every archive with its digest and size, bound to the version and the source
  commit.
- `SHA256SUMS`.
- `attestation.sigstore.json`: one SLSA provenance attestation whose subjects are every archive and the manifest.

The signer is the workflow `release.yml` on `main`, for nightlies and releases alike. Verify an asset with:

```sh
gh attestation verify sidevoice-connector-0.7.0-linux-x86_64.tar.zst \
  --repo sidevoice/sidevoice-connector \
  --bundle attestation.sigstore.json \
  --cert-identity 'https://github.com/sidevoice/sidevoice-connector/.github/workflows/release.yml@refs/heads/main' \
  --deny-self-hosted-runners
```

The core is inside (sidevoice/sidevoice-connector#66, decision 2): nothing is downloaded at install. Installing
stages it from the package into the release and runs its self-test there (`connector/src/core_package.rs`).

The changelog is written from the squashed PR titles. To change it, edit `CHANGELOG.md` in the release PR right
before merging it: any later merge into `main` regenerates the PR. After the release, fix the notes on the
Release itself.

## npm: what is published and how to add a platform

### What is published

Every `vX.Y.Z` release (never the nightly) is also published to npm, as four packages of the same version
(`xtask/src/npm.rs`, the pattern esbuild and Biome use):

| Package | What it carries | dist-tag | How it is published |
|---|---|---|---|
| `@sidevoice/sidevoice-darwin-arm64`, `@sidevoice/sidevoice-linux-x64`, `@sidevoice/sidevoice-linux-arm64` | That target's release archive unpacked as it is (`bin/sidevoice-connector`, `connector.json`, the core archive under `core/`, `LICENSE`, `notices/`), a `package.json` with `os`, `cpu` and on Linux `libc: ["glibc"]`, and a README. **No `bin`**: the binary just lives in the package. | `platform` | directly, with provenance |
| `sidevoice` | `optionalDependencies` on the three at **exactly** its own version (no ranges), and one script, `bin/sidevoice.js` (`xtask/npm/sidevoice.js`, plain JavaScript, no dependencies): it runs the installed platform package's binary with the same arguments, standard streams and exit status, and says clearly when the platform is unsupported or its package is missing. The only package with a `bin`: `sidevoice`. | `latest` for `X.Y.Z`, `next` for `X.Y.Z-rc.N` | **staged**, with provenance; a maintainer approves it |

npm installs, of the launcher's optional dependencies, only the one matching the machine. The connector finds its
package by its own path (the directory above its `bin/`), so it runs from `node_modules` exactly as from the archive.

### Who publishes, and how

Only the release workflow publishes, by npm's **trusted publishing** (OIDC): no npm token exists, and the packages
are set to require 2FA and disallow tokens. The job `npm` in `release.yml` runs after the GitHub Release is
published, for versioned releases only, as one step, `cargo xtask npm-publish vX.Y.Z`:

1. It downloads the Release's archives, `SHA256SUMS` and attestation and checks every archive against both: npm gets
   the bytes GitHub Releases has, nothing rebuilt.
2. It packs the packages with the npm CLI pinned in `xtask/src/npm.rs` (`NPM_VERSION`; trusted publishing needs
   11.5.1 or later), run with a configuration of its own: no `.npmrc`, no token from the environment.
3. It checks that trusted publishing accepts this run for **every** package (the same OIDC exchange `npm publish`
   makes) before publishing any. If one is not configured the job fails, naming the packages and what to set, and
   nothing is published. There is no token to fall back to.
4. It publishes the three platform packages directly, under the dist-tag `platform`, with `--provenance`; then
   **stages** `sidevoice` (`npm stage publish`, `--provenance`, dist-tag `latest` or `next`). A version already
   published with the same bytes is skipped, so a re-run carries on; with other bytes it fails.

Success means **"platform packages published, launcher staged awaiting approval"**; the job's last line says so,
with the stage id. A maintainer approves it on npmjs.com (`sidevoice` → staged versions) or with
`npm stage approve <id>`, both with 2FA. Until then `npx sidevoice` keeps installing the previous version.

Why this shape:

- **One approval per release.** Only the launcher is staged. It pins the platform packages' exact version, so
  approving it approves every byte it will run; a range would let it pick up a platform version nobody approved.
- **A new platform version reaches nobody until the launcher that pins it is approved.** The platform packages are
  published directly but under `platform`, never `latest` or `next`, and declare no `bin`: `npm i
  @sidevoice/sidevoice-<os>-<cpu>` resolves nothing new by default. Only the launcher brings them.
- **npm versions are immutable.** A published version can never be replaced (only deprecated), so every check runs
  before the first publish, and a bad release is fixed by the next version. A staged launcher can still be rejected.

The trusted publisher of each package (npmjs.com → the package → Settings → Trusted publisher → GitHub Actions):
organisation `sidevoice`, repository `sidevoice-connector`, no environment, and the workflow filename npm checks.
**npm checks the workflow that starts the run, not one it calls**: a version is released by `release-please.yml`,
which calls `release.yml` (`workflow_call`), so the filename npm sees is `release-please.yml`. The job prints it
before publishing, and its error names it when it does not match. Platform packages: "Allow npm publish";
`sidevoice`: staged publishing. Every workflow on the way grants `id-token: write`, and the job sets up Node.js 24
(trusted publishing needs 22.14 or later).

### Adding a platform

Say Windows x64 or macOS x86_64:

1. **Reserve the name.** Publish `@sidevoice/sidevoice-<os>-<cpu>` (npm's `process.platform` and `process.arch`:
   `win32-x64`, `darwin-x64`) once by hand as a public `0.0.1` placeholder, so the name is ours.
2. **Configure its trusted publisher** as above: `sidevoice` / `sidevoice-connector` / the workflow filename npm
   checks (see above), no environment, "Allow npm publish" checked; in its access settings, require 2FA and
   disallow tokens.
3. **Add the target to `TARGETS` in `xtask/src/main.rs`**, the one list the tooling reads: its name (as in archive
   names, `<os>-<arch>`), the Rust `std::env::consts` OS and ARCH of the machine that builds it, and npm's `os` and
   `cpu`. The manifest, the npm packages and the launcher's `optionalDependencies` follow from it.
4. **Add its runner** to the matrices of `ci.yml` (`test`) and `release.yml` (`dist`), and to `lint` in `ci.yml` if
   it compiles code no other runner does.
5. **What is specific to the platform**, each a change of its own:
   - the connector's own target name (`connector/src/identity.rs`) and its service manager
     (`src/service/`: launchd and systemd today; Windows needs its own);
   - a pinned sidevoice-core release built for that target (`core.pin`, `xtask/src/core.rs`);
   - linking: on Linux the glibc floor (`xtask/src/glibc.rs`, cargo-zigbuild in `.github/actions/setup`); on
     musl, a separate target with `libc: ["musl"]`; the libraries a binary may load (`xtask/src/verify.rs`);
   - the archive: `bin/sidevoice-connector` gets `.exe` on Windows, which the inventory's `entrypoint`, `verify`
     and the launcher (it reads `entrypoint` from `connector.json`) must follow;
   - the README's supported platforms and the launcher package's README (`xtask/src/npm.rs`).
## Which version comes next

`fix:` → patch, `feat:` → minor. While the version is 0.x a breaking change (`feat!:` or a `BREAKING CHANGE:`
footer) bumps the minor, not the major. `docs:`, `chore:`, `ci:`, `test:`, `refactor:` alone make no release.

The manifest starts at 0.6.0, the version the npm package carried. release-please finds the tag `v0.6.0` and reads
the history from it; without that tag it reads from the commit where this repository became the client alone
(`bootstrap-sha`).

## A release candidate, or any explicit version

Put the footer as the **last line of a PR's description** (the squash commit takes the description as its body):

```
Release-As: 0.7.0-rc.1
```

The release PR then proposes exactly that version. A version with a `-` suffix is published as a **pre-release and
never as latest**. The next candidate is `Release-As: 0.7.0-rc.2`; the final one is `Release-As: 0.7.0` (say it:
after a candidate, do not leave the next version to the computation). With nothing else to merge, a PR with one
empty commit (`git commit --allow-empty`) carries the footer.

## Nightly

Every green `release` run on `main` moves the tag `nightly` to that commit and replaces every asset of the one
`nightly` pre-release. Its notes give the commit. It is a snapshot, not a version: never latest, and release-please
ignores the tag. Pin a `vX.Y.Z` release, never `nightly`.

Build artifacts on Actions runs are kept 7 days, for debugging only. Download from Releases.

## When something fails

- A release build or its verification fails: the Release stays a draft, its tag in place. Fix forward if needed,
  then re-run the failed jobs of that `release-please` run. Nothing is published until every check passed.
- A `nightly` run fails: the previous snapshot stays. The next green push replaces it.
- The npm job fails: the GitHub Release is already published and stays. Fix the cause (usually the trusted
  publisher settings: the error names them) and re-run that job; packages already published with the same bytes are
  skipped. If the launcher is already staged, approve or reject it on npmjs.com instead.
- A release run is never cancelled half-way; nightlies queue behind each other.
- Moving to a new core release is a PR that changes `core.pin` (by hand or by a dependency bot); its CI is the
  connector's tests against that core.

## What this needs from the repository settings

- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- `ci` and **PR title is a conventional commit** run on every PR; release-please's own PR gets both through a
  dispatched run (its pushes start no workflow by themselves). Make both required in a ruleset to enforce them.
- The `npm` environment is not used: publishing to npm runs in no environment, and its approval is the staged
  launcher's (see [npm](#npm-what-is-published-and-how-to-add-a-platform)). Delete it.
- On npmjs.com, a trusted publisher for each npm package ([Trusted publishing](#who-publishes-and-how)).
