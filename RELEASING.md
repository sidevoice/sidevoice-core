# Releasing

One version for the package, tagged `vX.Y.Z`. It lives in `pyproject.toml`; release-please moves it
(`release-please-config.json`) and updates the root package entry in `uv.lock`. Never edit it by hand. For the beta
the core is distributed as GitHub Releases of this repository; PyPI comes with the public launch (#8).

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | Python tests and **PR title is a conventional commit** run. Rust input changes also run formatting, locked dependency, Clippy and Rust tests. Package, native bundle and manifest jobs do not run automatically on PRs. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `test` runs the tests and packages; when it is green, the `nightly` pre-release is replaced. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). Nothing versioned is published. |
| Merge the release PR | a maintainer | **This is the release.** release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `test` runs from the tag, attaches the assets and publishes the Release. |

For final candidate proof, explicitly dispatch `test.yml` and the relevant `rust-t*.yml` workflows on the candidate ref. These runs build and check package/native artifacts without publishing them. Only the `main` push and release paths publish assets.

Assets of a release:

- `sidevoice_core-X.Y.Z-py3-none-any.whl` and `sidevoice_core-X.Y.Z.tar.gz`: what the connector installs, by the
  release URL, with uv.
- `sidevoice-core-X.Y.Z-macos-aarch64.tar.zst`, `sidevoice-core-X.Y.Z-linux-x86_64.tar.zst` and
  `sidevoice-core-X.Y.Z-linux-aarch64.tar.zst`: relocatable CPython 3.12.14 runtimes with the core and its locked
  dependencies installed in that interpreter's `site-packages`.
- `core-manifest.json`: bundle download URLs, SHA-256 digests and compressed sizes, plus the wheel URL and digest.
- `models-catalog.json`: the model catalogue (`src/sidevoice_core/models/catalog.json`, checked by its validator),
  what the web and the desktop app take at build time.
- `models-vectors.json`: the shared resolver vectors every client implementation must pass.
- `SHA256SUMS`.
- `<asset>.sigstore.json` for each core bundle, the wheel and `core-manifest.json`.

The catalogue and the vectors are byte for byte the ones inside the wheel (the workflow checks it).
The checksum file covers the wheel, sdist, catalogue, vectors, bundles and manifest. The bundle job summary records
the measured compressed and unpacked byte counts per target. Each bundle keeps Sidevoice's `LICENSE` and
`TRADEMARKS.md`, the pinned python-build-standalone project license and upstream vendor notices from that source
commit, CPython's license from its upstream standalone distribution, and dependency license files supplied in
`.dist-info/licenses`; `DEPENDENCIES.lock.txt` records the exact dependency versions and hashes used for the build.

The `test.yml` build creates SLSA provenance from the public main-branch workflow and uploads the Sigstore bundles
with the release assets. It verifies the bytes read back from the release using the attached bundle, the exact
main-branch certificate identity and GitHub-hosted runner policy. After publication it downloads the public URLs
without a token and repeats the byte and attestation checks. A local verification command is:

```sh
gh attestation verify sidevoice-core-0.2.0-linux-x86_64.tar.zst \
  --repo sidevoice/sidevoice-core \
  --bundle sidevoice-core-0.2.0-linux-x86_64.tar.zst.sigstore.json \
  --cert-identity 'https://github.com/sidevoice/sidevoice-core/.github/workflows/test.yml@refs/heads/main' \
  --deny-self-hosted-runners
```

The exact certificate identity pins the reusable main-branch signer. GitHub CLI treats `--cert-identity` and
`--signer-workflow` as alternative identity checks, so use the full certificate identity alone. The workflow also
downloads the published assets anonymously after publication, then compares and verifies those public bytes with the
attached Sigstore bundles.

The manifest and its provenance are release-specific. Do not use the moving `nightly` tag as a version pin.

The changelog is written from the squashed PR titles. To change it, edit `CHANGELOG.md` in the release PR right
before merging it: any later merge into `main` regenerates the PR. After the release, fix the notes on the
Release itself.

## Which version comes next

`fix:` → patch, `feat:` → minor. While the version is 0.x a breaking change (`feat!:` or a `BREAKING CHANGE:`
footer) bumps the minor, not the major. `docs:`, `chore:`, `ci:`, `test:`, `refactor:` alone make no release.

Nothing is tagged yet: the manifest starts at 0.1.0, so the first release PR proposes 0.2.0 if there is a `feat`.
To publish the first one as 0.1.0, use `Release-As: 0.1.0` (below).

## A release candidate, or any explicit version

Put the footer as the **last line of a PR's description** (the squash commit takes the description as its body):

```
Release-As: 0.3.0-rc.1
```

The release PR then proposes exactly that version (the wheel says `0.3.0rc1`, its PEP 440 spelling). A version
with a `-` suffix is published as a **pre-release and never as latest**. The next candidate is
`Release-As: 0.3.0-rc.2`; the final one is `Release-As: 0.3.0` (say it: after a candidate, do not leave the next
version to the computation). With nothing else to merge, a PR with one empty commit (`git commit --allow-empty`)
carries the footer.

## Nightly

Every green `test` on `main` moves the tag `nightly` to that commit and replaces every asset of the one `nightly`
pre-release: the wheel and sdist, three core bundles, `core-manifest.json`, their Sigstore bundles,
`models-catalog.json`, `models-vectors.json` and `SHA256SUMS`. Its notes give the commit and its date. It is a
snapshot, not a version: the wheel carries the version of the last release, it is never latest, and release-please
ignores the tag (it is not `vX.Y.Z`). Pin a `vX.Y.Z` release, never `nightly`.

Build artifacts on Actions runs are kept 7 days, for debugging only. Download from Releases.

## When something fails

- The build or pre-publication verification of a release fails: the Release stays a draft, its tag in place. Fix
  forward if needed, then re-run the failed jobs of that `release-please` run (Actions). Nothing is published until
  every pre-publication check passed. The anonymous download check runs after publication, because GitHub draft
  assets are not public.
- A `nightly` run fails: the previous snapshot stays. The next green push replaces it.

## What this needs from the repository settings

- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- The **PR title is a conventional commit** check runs for ordinary PRs; release-please's own PR gets it through a
  dispatched run (its pushes start no workflow by themselves). GitHub currently has no branch protection or ruleset
  requiring this check; configure one if merge enforcement is needed.
