# Releasing

asteria is released with [release-plz](https://release-plz.dev/). Photos and music, their
desktop apps, headless servers and crates share the workspace's one version
(`[workspace.package] version` in `Cargo.toml`) and one `v{version}` tag and GitHub
release. Nothing goes to crates.io: every package is in release-plz's git-only mode, which
reads the last version from the `v{version}` tags. `asteria-common-core` makes the tag and
the release and writes the one `CHANGELOG.md` for all of them (`release-plz.toml`).

## Normal flow

1. On every push to `main`, release-plz (`.github/workflows/release-plz.yml`) opens or
   updates a release PR (labelled `release`): the next version in `Cargo.toml`, the updated
   `Cargo.lock`, and a section for the version in `CHANGELOG.md`, from the conventional
   commits since the last release.
2. Review the release PR: the version and `CHANGELOG.md`. Edit the section by hand there
   if it needs it, just before merging: the next push to `main` rewrites the PR.
3. Merge it: release-plz tags `v{version}` and makes the GitHub release, its notes the
   version's `CHANGELOG.md` section (a pre-release when the version has a `-`). The same
   run then calls `.github/workflows/release.yml`, which builds the desktop packages
   (signed, with their update feeds) and the headless servers and attaches them to the
   release. An installed app finds the new version from there.

Pull requests do not touch `CHANGELOG.md`: their commit messages are what it is written
from, so write them for a reader of the changelog.

To build an existing tag again (a failed job, say), run Release (`release.yml`) by hand
with the tag. For a tag pushed by hand, without release-plz, it creates the release too.

## Version bump rules

release-plz infers the version from conventional commits (pre-1.0, `0.x.y`):

- `feat:`, `fix:` and other non-breaking changes → patch bump.
- `feat!:` / `fix!:`, or a `BREAKING CHANGE:` footer → minor bump.

A change that an older client or server cannot work with, or a library an older version
cannot open, is breaking.

The API crates (`asteria-common-api`, `asteria-photos-api`, `asteria-music-api`) are not
released by release-plz: their versions are the handshake's `app_api` (the major), bumped by
hand when the API changes.

For a pre-release, set the version by hand (`0.2.0-rc.1`) in a PR; the release PR then keeps
it. release-plz reads only `v{major}.{minor}.{patch}` tags as earlier releases, so the
version after `0.2.0-rc.1` is worked out from the last full release.

### The first release

0.1.0 follows the pre-releases `v0.1.0-rc.1` to `rc.3`, which were tagged by hand. The
first release PR keeps 0.1.0, the version `Cargo.toml` carries, and its `CHANGELOG.md`
section has only the latest commit (release-plz writes no history on a first release):
replace it in that PR, once, with a summary of what 0.1.0 is.
