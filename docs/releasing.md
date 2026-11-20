# Releasing Fell

Use SemVer and keep the major version at `0` until v1. From a clean, up-to-date `main`, update `[workspace.package].version` and the workspace path dependency versions in `Cargo.toml`. Refresh `Cargo.lock` with `cargo check --workspace`, update `CHANGELOG.md`, and land those changes on `main` before tagging.

Tag the release commit and push the tag. For example, with workspace version `0.1.0`:

```sh
git tag -a v0.1.0 -m 'Fell 0.1.0'
git push origin v0.1.0
```

Use a tag such as `v0.1.0-rc.1` for a release candidate and keep the workspace version at `0.1.0`. The tag suffix appears in the assets and build version. GitHub marks the release as a prerelease and leaves the latest stable release in place.

The **Linux release** workflow builds and tests both architectures on native Ubuntu runners. It smoke-tests the tarball and AppImage on a clean Arch installation with `webkit2gtk-4.1` installed. The GitHub Release includes generated notes from merged PRs, using the previous reachable `v*` tag on the first-parent history as the start when one exists. The build version uses the first 12 characters of the commit SHA and its UTC committer date.

## Dry run

A pull request that changes the release workflow or the packaging scripts builds a candidate from the branch’s workspace version. It uploads both architectures and skips publication.

Use **Run workflow** on **Linux release** and enter the candidate tag that matches the selected branch's workspace version. The workflow can build `v0.1.0-rc.1` before any tag exists. Download the combined `fell-0.1.0-rc.1-release` Actions artifact for the four packages and `SHA256SUMS`. The manual run uses read-only repository permissions and skips the publish job.

To check the plan locally, run `packaging/linux/release.sh plan v0.1.0-rc.1`. To build the matching assets locally on each architecture, export `FELL_RELEASE_VERSION=0.1.0-rc.1`, run `cargo build --release --locked -p fell`, then run `packaging/linux/package.sh`. Copy all four assets into one directory and run `packaging/linux/release.sh assemble 0.1.0-rc.1 PATH` to write and verify the checksum manifest.

## Asset names

Every release has `fell-<version>-<arch>-linux.tar.gz`, `fell-<version>-<arch>.AppImage`, and `SHA256SUMS`. The architecture is `x86_64` or `aarch64`. For scripts, resolve the latest stable tag with `gh release view --repo predmore/fell --json tagName --jq .tagName` and strip the leading `v` for the asset name. Fetch an exact URL with `https://github.com/predmore/fell/releases/download/<tag>/<asset>`. Use the explicit candidate tag for prereleases.
