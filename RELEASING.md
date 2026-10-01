# Releasing

One version for the client, tagged `vX.Y.Z`: the npm package `@sidevoice/uplink`. It lives in
`packages/connector/package.json`, the root `package.json` and `package-lock.json`; release-please moves them
together (`release-please-config.json`). Never edit them by hand.

The core the client installs is a separate pin, `CORE_VERSION` in `packages/connector/core.mjs`, bumped by hand in
a PR like any other change.

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | `CI`: the client's suite (Linux) and a dry run of the package. **PR title is a conventional commit**. Nothing is packaged. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `CI` runs the suite and packs the client with sidevoice-core's `nightly` wheel inside; when it is green, the `nightly` pre-release is replaced. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). Nothing versioned is published, nothing goes to npm. |
| Merge the release PR | a maintainer | release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `CI` runs from the tag, packs the client with the wheel of sidevoice-core's `vCORE_VERSION` release inside, and attaches it to the draft. |
| Approve the `npm` deployment | a maintainer | **This is the publication.** The run waits on the `npm` environment; once approved, the attached tarball, byte for byte, goes to npm (`latest`, or `next` for a pre-release), then the GitHub Release is published. Rejecting it leaves the Release a draft and npm untouched. |

Assets of a release:

- `sidevoice-uplink-X.Y.Z.tgz`: what `npm pack` makes, with `dist/core/sidevoice_core-CORE_VERSION-py3-none-any.whl`
  inside, so `sidevoice install` installs the core from beside the bundle and fetches nothing from an index. The
  same file goes to npm.
- `SHA256SUMS`.

The changelog is written from the squashed PR titles. To change it, edit `CHANGELOG.md` in the release PR right
before merging it: any later merge into `main` regenerates the PR. After the release, fix the notes on the
Release itself.

## Which version comes next

`fix:` → patch, `feat:` → minor. While the version is 0.x a breaking change (`feat!:` or a `BREAKING CHANGE:`
footer) bumps the minor, not the major. `docs:`, `chore:`, `ci:`, `test:`, `refactor:` alone make no release.

The manifest starts at 0.6.0, the version the package carries. With no GitHub Release yet, release-please finds
the tag of that version, `v0.6.0` (from the history before the client had a repository of its own, like `v0.4.3`
and `v0.5.0` here), and reads the history from it: the first release PR proposes 0.7.0, and its changelog also
lists commits after that tag that were about other parts (trim them in the release PR). Without that tag it would read from the
commit where this repository became the client alone (`bootstrap-sha`). npm has 0.5.0 as its latest: 0.6.0 was
never published there. To publish 0.6.0 itself, use `Release-As: 0.6.0` (below).

## A release candidate, or any explicit version

Put the footer as the **last line of a PR's description** (the squash commit takes the description as its body):

```
Release-As: 0.7.0-rc.1
```

The release PR then proposes exactly that version. A version with a `-` suffix goes to npm under `next` and is a
GitHub **pre-release, never latest**. The next candidate is `Release-As: 0.7.0-rc.2`; the final one is
`Release-As: 0.7.0` (say it: after a candidate, do not leave the next version to the computation). With nothing
else to merge, a PR with one empty commit (`git commit --allow-empty`) carries the footer.

## Nightly

Every green `CI` on `main` moves the tag `nightly` to that commit and replaces every asset of the one `nightly`
pre-release: `sidevoice-uplink-X.Y.Z-nightly.tgz` (X.Y.Z is the last release's version, which the package still
carries), with sidevoice-core's nightly wheel inside, and `SHA256SUMS`. Its notes give the commit and its date. It
is a snapshot, not a version: never on npm, never latest, and release-please ignores the tag (it is not `vX.Y.Z`).
Pin a `vX.Y.Z` release, never `nightly`.

Install a nightly by its file: `npm i -g ./sidevoice-uplink-X.Y.Z-nightly.tgz`.

Build artifacts on Actions runs are kept 7 days, for debugging only. Download from Releases.

## When something fails

- **The core's wheel does not match the pin.** The tarball must carry `sidevoice_core-CORE_VERSION-…whl`. A
  release takes it from sidevoice-core's `vCORE_VERSION` release, so that release must exist first; a nightly
  takes it from the core's `nightly`, which carries the core's last released version. When the core releases a
  new version, nightlies fail until a PR here moves `CORE_VERSION` to it. The error says which.
- The build of a release fails: the Release stays a draft, its tag in place. Fix forward if needed, then re-run
  the failed jobs of that `release-please` run (Actions). Nothing is published until every job passed.
- npm refuses the publication: the Release stays a draft with its assets. Re-run the `npm` job once fixed.
- A `nightly` run fails: the previous snapshot stays. The next green push replaces it.

## What this needs (repository settings)

- **Environment `npm`** (Settings → Environments) with **required reviewers** (the maintainers), deployment branches
  limited to `main` (release-please runs there). The workflow checks it is there and protected before anything
  reaches npm, and refuses otherwise. Note: GitHub documents required reviewers on a private repository as an
  Enterprise feature; on the Free plan they exist once the repository is public. Until then no release reaches npm
  through this workflow.
- **The trusted publisher on npm**: npmjs.com → `@sidevoice/uplink` → Settings → Trusted publishing → GitHub
  Actions: organisation or user `sidevoice`, repository `sidevoice-connector`, workflow filename
  `release-please.yml`, environment `npm`. Then, under publishing access, disallow tokens. Until it is registered,
  `npm publish` fails for lack of credentials, and nothing is published.
- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- Required check **PR title is a conventional commit**. release-please's own PR gets it through a dispatched run
  (its pushes start no workflow by themselves).
