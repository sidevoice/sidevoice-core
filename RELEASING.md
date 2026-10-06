# Releasing

One version for the core, tagged `vX.Y.Z`. It lives in `Cargo.toml` (and the root package entry of `Cargo.lock`);
release-please moves it (`release-please-config.json`). Never edit it by hand. The core is distributed as GitHub
Releases of this repository.

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | `ci`: format and Clippy, then on every target the tests and the release packaging (`cargo xtask dist`), publishing nothing. **PR title is a conventional commit**. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `release` runs: per target it runs the tests, builds and packages the native core; then it attests the assets, attaches them to the `nightly` pre-release, reads them back, verifies them and publishes. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). |
| Merge the release PR | a maintainer | **This is the release.** release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `release` runs from the tag, attaches and verifies the assets, and publishes the Release. |

The tests are part of the build: an asset is only produced on a target where the whole suite passed. Everything
besides the GitHub steps is code in `xtask/` (`cargo xtask models | dist | verify | manifest | publish`): run
`cargo xtask dist` on a machine to get that machine's archive, built, packaged and verified exactly as the release
one. A pull request already runs that packaging on every target.

## Assets

- `sidevoice-core-<version>-<target>.tar.zst` for `macos-aarch64`, `linux-x86_64` and `linux-aarch64` (on the nightly,
  `sidevoice-core-nightly-<target>.tar.zst`, fixed names whose download URLs never change): the relocatable native
  core per target (`bin/sidevoice-core-rust`, the pinned detector models, its libraries and licence notices). The
  source commit is in the manifest and in each archive's `native-core.json`.
- `native-core-manifest.json`: every archive with its digest and size, bound to the source commit.
- `SHA256SUMS`.
- `attestation.sigstore.json`: one SLSA provenance attestation whose subjects are every archive and the manifest.

The signer is the workflow `release.yml` on `main`, for nightlies and releases alike. Verify an asset with:

```sh
gh attestation verify sidevoice-core-0.2.0-linux-x86_64.tar.zst \
  --repo sidevoice/sidevoice-core \
  --bundle attestation.sigstore.json \
  --cert-identity 'https://github.com/sidevoice/sidevoice-core/.github/workflows/release.yml@refs/heads/main' \
  --deny-self-hosted-runners
```

The changelog is written from the squashed PR titles. To change it, edit `CHANGELOG.md` in the release PR right
before merging it: any later merge into `main` regenerates the PR. After the release, fix the notes on the
Release itself.

## Which version comes next

`fix:` → patch, `feat:` → minor. While the version is 0.x a breaking change (`feat!:` or a `BREAKING CHANGE:`
footer) bumps the minor, not the major. `docs:`, `chore:`, `ci:`, `test:`, `refactor:` alone make no release.

## A release candidate, or any explicit version

Put the footer as the **last line of a PR's description** (the squash commit takes the description as its body):

```
Release-As: 0.3.0-rc.1
```

The release PR then proposes exactly that version. A version with a `-` suffix is published as a **pre-release and
never as latest**. The next candidate is `Release-As: 0.3.0-rc.2`; the final one is `Release-As: 0.3.0` (say it:
after a candidate, do not leave the next version to the computation). With nothing else to merge, a PR with one
empty commit (`git commit --allow-empty`) carries the footer.

## Nightly

Every green `release` run on `main` moves the tag `nightly` to that commit and replaces every asset of the one `nightly`
pre-release. Its notes give the commit and its date. It is a snapshot, not a version: never latest, and
release-please ignores the tag. Pin a `vX.Y.Z` release, never `nightly`.

Build artifacts on Actions runs are kept 7 days, for debugging only. Download from Releases.

## When something fails

- A release build or its verification fails: the Release stays a draft, its tag in place. Fix forward if needed,
  then re-run the failed jobs of that `release-please` run. Nothing is published until every check passed.
- A `nightly` run fails: the previous snapshot stays. The next green push replaces it.
- A release run is never cancelled half-way; nightlies queue behind each other.

## Compatibility with the released clients

`compat` runs every Monday (and by hand): `cargo xtask compat` takes the latest published `vX.Y.Z` release of
sidevoice-connector and of sidevoice-web (never `nightly`), checks its assets against its `SHA256SUMS`, and runs
`tests/compat.rs` against them with this core built from `main`: the released connector (`sidevoice-uplink-*.tgz`,
run with Node) links to the core, takes a typed input and publishes a reply; every node route the released web
bundle (`sidevoice-web-*.tar.gz`) names exists. A failure opens an issue labelled `compat`, or comments on the open
one. A repository with no published release is skipped and said so in the log: today neither has one, so the
check waits for each one's first release. `SIDEVOICE_COMPAT_CONNECTOR_TAG` / `SIDEVOICE_COMPAT_WEB_TAG` check another
tag instead (a candidate, or `nightly`).

## What this needs from the repository settings

- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- `ci` and **PR title is a conventional commit** run on every PR; release-please's own PR gets both through a
  dispatched run (its pushes start no workflow by themselves). Make both required in a ruleset to enforce them.
