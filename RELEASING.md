# Releasing

One version for the package, tagged `vX.Y.Z`. It lives in `pyproject.toml`; release-please moves it
(`release-please-config.json`). Never edit it by hand. For the beta the core is distributed as GitHub Releases of
this repository; PyPI comes with the public launch (#8).

## What each act means

| Act | Who | What happens |
|---|---|---|
| Open / update a PR | anyone | The unit tests (Linux) and **PR title is a conventional commit**. Nothing is packaged. |
| Squash-merge into `main` | reviewer | The PR title becomes the commit. `test` runs the tests and packages; when it is green, the `nightly` pre-release is replaced. release-please opens or updates the **release PR** ("chore(main): release X.Y.Z"). Nothing versioned is published. |
| Merge the release PR | a maintainer | **This is the release.** release-please tags `vX.Y.Z` and creates a draft GitHub Release whose notes are that version's changelog; `test` runs from the tag, attaches the assets and publishes the Release. |

Assets of a release:

- `sidevoice_core-X.Y.Z-py3-none-any.whl` and `sidevoice_core-X.Y.Z.tar.gz`: what the connector installs, by the
  release URL, with uv.
- `models-catalog.json`: the model catalogue (`src/sidevoice_core/models/catalog.json`, checked by its validator),
  what the web and the desktop app take at build time.
- `models-vectors.json`: the shared resolver vectors every client implementation must pass.
- `SHA256SUMS`.

The catalogue and the vectors are byte for byte the ones inside the wheel (the workflow checks it).

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
pre-release: the wheel and sdist, `models-catalog.json`, `models-vectors.json`, `SHA256SUMS`. Its notes give the
commit and its date. It is a snapshot, not a version: the wheel carries the version of the last release, it is
never latest, and release-please ignores the tag (it is not `vX.Y.Z`). Pin a `vX.Y.Z` release, never `nightly`.

Build artifacts on Actions runs are kept 7 days, for debugging only. Download from Releases.

## When something fails

- The build of a release fails: the Release stays a draft, its tag in place. Fix forward if needed, then re-run
  the failed jobs of that `release-please` run (Actions). Nothing is published until every job passed.
- A `nightly` run fails: the previous snapshot stays. The next green push replaces it.

## What this needs from the repository settings

- Settings → Actions → General → **Allow GitHub Actions to create and approve pull requests**: without it
  release-please cannot open its PR.
- Squash merging, with the PR title as the commit message.
- Required check **PR title is a conventional commit**. release-please's own PR gets it through a dispatched run
  (its pushes start no workflow by themselves).
