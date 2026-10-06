# Releasing

One version for the connector, tagged `vX.Y.Z`. It lives in `packages/connector-rust/Cargo.toml` (the build tooling at
the workspace root carries the same one, and `Cargo.lock` both); release-please moves them together
(`release-please-config.json`). Never edit it by hand. The connector is distributed as GitHub Releases of this
repository: one binary per target.

The Node package (`packages/connector`, `@sidevoice/uplink` on npm) is no longer built or published here: it keeps
its tests in CI until the connector is one Rust binary, and npm per-platform packages of the binary come with that
migration.

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | `ci`: format and Clippy (Linux and macOS), then on every target the tests and the release packaging (`cargo xtask dist`), publishing nothing; and the Node package's `npm test` on Linux. **PR title is a conventional commit**. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `release` runs: per target it runs the tests, builds and packages the connector; then it attests the assets, attaches them to the `nightly` pre-release, reads them back, verifies them and publishes. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). |
| Merge the release PR | a maintainer | **This is the release.** release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `release` runs from the tag, attaches and verifies the assets, and publishes the Release. |

The tests are part of the build: an asset is only produced on a target where the whole suite passed. Everything
besides the GitHub steps is code in `xtask/` (`cargo xtask fixtures | dist | verify | manifest | publish`):

- `cargo xtask fixtures` fetches what the tests run against: sidevoice-core's published `nightly` for this machine
  (checked against that release's `SHA256SUMS`, manifest and attestation, signed by the core's `release.yml` on
  `main`) into `target/sidevoice-core`, and the pinned Codex CLI into `target/codex`. Without them the tests that
  need them are skipped locally and fail in CI.
- `cargo test --locked` runs the unit tests and the integration tests in `packages/connector-rust/tests/`: the
  connector against the real core, registration with the real Codex CLI, and (macOS) the login service under the
  real launchd.
- `cargo xtask dist` builds this machine's release binary and packages it exactly as the release does, then verifies
  the archive by unpacking it elsewhere and running the connector from there. A pull request already runs it on
  every target.

## Assets

- `sidevoice-connector-<version>-<target>.tar.zst` for `macos-aarch64`, `linux-x86_64` and `linux-aarch64` (on the
  nightly, `sidevoice-connector-nightly-<target>.tar.zst`, fixed names whose download URLs never change): the root
  `sidevoice-connector/` holds `bin/sidevoice-connector`, `LICENSE`, the licence notices of every crate linked into
  the binary (`notices/`), and `connector.json`, the inventory: version, target, source commit and every file with
  its size and digest.
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

The core is not inside: the connector is to install a pinned core release of its own (sidevoice/sidevoice-connector#66).

The changelog is written from the squashed PR titles. To change it, edit `CHANGELOG.md` in the release PR right
before merging it: any later merge into `main` regenerates the PR. After the release, fix the notes on the
Release itself.

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
- A release run is never cancelled half-way; nightlies queue behind each other.
- The tests fail because sidevoice-core's `nightly` changed under them: the core's nightly is what the tests run
  against, on purpose; fix the connector (or the core) forward.

## What this needs from the repository settings

- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- `ci` and **PR title is a conventional commit** run on every PR; release-please's own PR gets both through a
  dispatched run (its pushes start no workflow by themselves). Make both required in a ruleset to enforce them.
- The `npm` environment is no longer used by any workflow; delete it, or keep it for the npm packages that come with
  the migration.
