#!/usr/bin/env bash
# Nightly x86_64 artifacts: a tarball and an AppImage.
#
# The current release binary does not link WebKitGTK (no libwebkit2gtk in
# DT_NEEDED). wry is not a crate yet. linuxdeploy still excludes
# libwebkit2gtk and libjavascriptcoregtk, and this script fails if either
# artifact contains them, so a later fallback cannot start bundling the
# LGPL library. Once that fallback is compiled in, the distro package is a
# runtime requirement: libwebkit2gtk-4.1-0 (Debian/Ubuntu) or
# webkit2gtk-4.1 (Arch).
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${BIN:-"$root/target/release/fell"}
dist=$root/dist
stage=$dist/fell-x86_64-linux
appdir=$dist/AppDir
work=$dist/work
tools=$dist/tools
here=$root/packaging/linux
tarball=$dist/fell-x86_64-linux.tar.gz
appimage=$dist/fell-x86_64.AppImage
linuxdeploy_version=1-alpha-20251107-1
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d

if [[ $(uname -m) != x86_64 ]]; then
  echo "this packager builds x86_64 artifacts only (got $(uname -m))" >&2
  exit 1
fi

if [[ ! -x $bin ]]; then
  echo "missing release binary: $bin" >&2
  echo "build it with: cargo build --release --locked -p fell" >&2
  exit 1
fi

if ! readelf -d "$bin" | grep -q NEEDED; then
  echo "fell must be dynamically linked; refusing a static binary" >&2
  exit 1
fi

rm -rf "$stage" "$appdir" "$work"
mkdir -p "$stage" "$work" "$tools"

install -m 0755 "$bin" "$stage/fell"
install -m 0644 "$here/dev.predmore.fell.desktop" "$stage/dev.predmore.fell.desktop"
install -m 0644 "$here/dev.predmore.fell.png" "$stage/dev.predmore.fell.png"
install -m 0644 "$here/README.md" "$stage/README.md"

# Filenames only. The runtime note mentions WebKitGTK; that text is fine.
refuse_webkit() {
  local label=$1
  shift
  local hits
  hits=$(find "$@" \( \
    -iname '*webkit2gtk*' -o \
    -iname '*javascriptcoregtk*' -o \
    -iname 'WebKitNetworkProcess' -o \
    -iname 'WebKitWebProcess' -o \
    -iname 'WebKitGPUProcess' \
  \) -print)
  if [[ -n $hits ]]; then
    echo "$label contains WebKitGTK; depend on the system package instead" >&2
    printf '%s\n' "$hits" >&2
    exit 1
  fi
}

refuse_webkit "tarball" "$stage"
tar -C "$dist" -czf "$tarball" fell-x86_64-linux

linuxdeploy=$tools/linuxdeploy
if [[ ! -f $linuxdeploy ]] || ! echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c --status -; then
  wget -nv -O "$linuxdeploy" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/${linuxdeploy_version}/linuxdeploy-x86_64.AppImage"
  echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c -
  chmod +x "$linuxdeploy"
fi

deploy_bin=$work/fell
cp "$bin" "$deploy_bin"
chmod +x "$deploy_bin"

# Do not bundle WebKitGTK. This binary has no WebKit SONAME. If a later
# build adds one, leave it out so the distro library is used at runtime.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64
export LDAI_OUTPUT=$appimage
export LDAI_NO_APPSTREAM=1

(
  cd "$work"
  "$linuxdeploy" \
    --appdir "$appdir" \
    --executable "$deploy_bin" \
    --desktop-file "$here/dev.predmore.fell.desktop" \
    --icon-file "$here/dev.predmore.fell.png" \
    --exclude-library 'libwebkit2gtk*' \
    --exclude-library 'libjavascriptcoregtk*' \
    --output appimage
)

if [[ ! -f $appimage ]]; then
  echo "linuxdeploy did not write $appimage" >&2
  find "$dist" -name '*.AppImage' -print >&2 || true
  exit 1
fi
chmod +x "$appimage"

refuse_webkit "AppDir" "$appdir"

# The AppImage is an ELF runtime with a squashfs appended. The bytes "hsqs"
# also occur inside the runtime, so try each offset until unsquashfs accepts it.
: >"$work/appimage-files.txt"
listed=0
while IFS=: read -r offset _; do
  if unsquashfs -o "$offset" -l "$appimage" >"$work/appimage-files.txt" 2>/dev/null; then
    listed=1
    break
  fi
done < <(grep -aob $'hsqs' "$appimage" || true)
if [[ $listed -ne 1 ]]; then
  echo "could not list AppImage contents to confirm WebKitGTK was left out" >&2
  exit 1
fi
if grep -Ei 'webkit2gtk|javascriptcoregtk|WebKitNetworkProcess|WebKitWebProcess|WebKitGPUProcess' "$work/appimage-files.txt"; then
  echo "AppImage bundles WebKitGTK; refusing to redistribute LGPL code" >&2
  exit 1
fi

if readelf -d "$stage/fell" | grep NEEDED | grep -Ei 'webkit|javascriptcore'; then
  echo "fell dynamically links WebKitGTK; artifacts leave the .so on the system"
else
  echo "fell does not link WebKitGTK (no libwebkit2gtk in DT_NEEDED)"
fi

echo "tarball:  $tarball"
echo "AppImage: $appimage"
