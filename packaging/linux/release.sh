#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
number='(0|[1-9][0-9]*)'
base="$number\.$number\.$number"
version_pattern="($base)(-rc(\.$number)?)?"

case "${1:-}" in
  plan)
    tag=$2
    if [[ ! $tag =~ ^v$version_pattern$ ]]; then
      echo "expected vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-rc.N, got $tag" >&2
      exit 1
    fi
    workspace=$(python3 - "$root/Cargo.toml" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as manifest:
    print(tomllib.load(manifest)["workspace"]["package"]["version"])
PY
)
    if [[ $workspace != "${BASH_REMATCH[1]}" ]]; then
      echo "tag $tag does not match workspace version $workspace" >&2
      exit 1
    fi
    prerelease=false
    if [[ $tag == *-rc* ]]; then prerelease=true; fi
    printf 'version=%s\nprerelease=%s\n' "${tag#v}" "$prerelease"
    ;;
  assemble)
    version=$2
    if [[ ! $version =~ ^$version_pattern$ ]]; then
      echo "invalid release version: $version" >&2
      exit 1
    fi
    cd "$3"
    assets=(
      "fell-$version-aarch64.AppImage"
      "fell-$version-aarch64-linux.tar.gz"
      "fell-$version-x86_64.AppImage"
      "fell-$version-x86_64-linux.tar.gz"
    )
    # Write the manifest only after all four checksums have been calculated.
    sums=$(sha256sum "${assets[@]}")
    printf '%s\n' "$sums" > SHA256SUMS
    sha256sum -c SHA256SUMS
    ;;
  *)
    echo "usage: $0 plan TAG | assemble VERSION ASSET_DIR" >&2
    exit 2
    ;;
esac
