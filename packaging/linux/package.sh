#!/usr/bin/env bash
# Nightly x86_64 artifacts: a tarball and an AppImage. Both carry fell and
# fell-mcp (the agent server); the AppImage runs it as `fell mcp …`.
#
# fell links WebKitGTK dynamically (libwebkit2gtk in DT_NEEDED). The
# distro package is required to launch: libwebkit2gtk-4.1-0 (Debian/Ubuntu)
# or webkit2gtk-4.1 (Arch). The nightly apt dev package is
# https://github.com/predmore/fell/issues/8
# linuxdeploy excludes libwebkit2gtk and libjavascriptcoregtk, and this
# script fails if either artifact contains them.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${BIN:-"$root/target/release/fell"}
# The workflow builds only `-p fell`; build fell-mcp here unless MCP_BIN
# names a binary. Cargo does nothing when it is already fresh.
mcp_bin=${MCP_BIN:-}
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
# Static type2 runtime (musl, libfuse linked in). Not the classic runtime
# that dlopens libfuse.so.2, which Ubuntu 24.04 does not ship.
runtime_version=20251108
runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

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

if [[ -z $mcp_bin ]]; then
  (cd "$root" && cargo build --release --locked -p fell-mcp)
  mcp_bin=${CARGO_TARGET_DIR:-"$root/target"}/release/fell-mcp
fi
if [[ ! -x $mcp_bin ]]; then
  echo "missing fell-mcp binary: $mcp_bin" >&2
  exit 1
fi
# The agent server has no UI and must not pull in WebKitGTK.
mcp_needed=$(readelf -d "$mcp_bin" | grep NEEDED || true)
if grep -Eqi 'webkit|javascriptcore|gtk' <<<"$mcp_needed"; then
  echo "fell-mcp links a UI library; it must stay a plain stdio server" >&2
  exit 1
fi
# Only the UI may mint a send confirmation. fell-mcp builds fell-core without
# its `ui-confirm` feature, so fell-confirm must not be in its dependency
# graph, and its symbols must not be in the binary.
# Outputs are captured before `grep -q`: under pipefail, grep closing the
# pipe early would turn a match into a miss.
mcp_deps=$(cd "$root" && cargo tree --locked -p fell-mcp -e normal,build --prefix none)
if grep -q '^fell-confirm ' <<<"$mcp_deps"; then
  echo "fell-mcp depends on fell-confirm; it must not be able to confirm a send" >&2
  exit 1
fi
mkdir -p "$dist"
nm -C "$mcp_bin" >"$dist/fell-mcp-symbols.txt" 2>/dev/null || true
if [[ ! -s $dist/fell-mcp-symbols.txt ]]; then
  echo "fell-mcp has no symbol table; cannot check that it leaves out fell-confirm" >&2
  exit 1
fi
if grep -Eq 'fell_confirm|for_fell_app' "$dist/fell-mcp-symbols.txt"; then
  echo "fell-mcp links fell-confirm; it must not be able to confirm a send" >&2
  grep -E 'fell_confirm|for_fell_app' "$dist/fell-mcp-symbols.txt" | head >&2
  exit 1
fi
rm -f "$dist/fell-mcp-symbols.txt"

rm -rf "$stage" "$appdir" "$work"
mkdir -p "$stage" "$work" "$tools"

install -m 0755 "$bin" "$stage/fell"
install -m 0755 "$mcp_bin" "$stage/fell-mcp"
install -m 0644 "$here/dev.predmore.fell.desktop" "$stage/dev.predmore.fell.desktop"
install -m 0644 "$here/dev.predmore.fell.compose.desktop" "$stage/dev.predmore.fell.compose.desktop"
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

# Artifact check: both programs are in the tarball and executable.
tar -tvzf "$tarball" >"$work/tarball-files.txt"
for name in fell fell-mcp; do
  if ! grep -Eq "^-rwx.* fell-x86_64-linux/$name\$" "$work/tarball-files.txt"; then
    echo "tarball is missing an executable $name" >&2
    cat "$work/tarball-files.txt" >&2
    exit 1
  fi
done

linuxdeploy=$tools/linuxdeploy
if [[ ! -f $linuxdeploy ]] || ! echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c --status -; then
  wget -nv -O "$linuxdeploy" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/${linuxdeploy_version}/linuxdeploy-x86_64.AppImage"
  echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c -
  chmod +x "$linuxdeploy"
fi

runtime=$tools/runtime-x86_64
if [[ ! -f $runtime ]] || ! echo "$runtime_sha256  $runtime" | sha256sum -c --status -; then
  wget -nv -O "$runtime" \
    "https://github.com/AppImage/type2-runtime/releases/download/${runtime_version}/runtime-x86_64"
  echo "$runtime_sha256  $runtime" | sha256sum -c -
  chmod +x "$runtime"
fi

deploy_bin=$work/fell
cp "$bin" "$deploy_bin"
chmod +x "$deploy_bin"
deploy_mcp=$work/fell-mcp
cp "$mcp_bin" "$deploy_mcp"
chmod +x "$deploy_mcp"

# Do not bundle WebKitGTK. This binary has no WebKit SONAME. If a later
# build adds one, leave it out so the distro library is used at runtime.
# LDAI_RUNTIME_FILE is the static type2 runtime, so the AppImage does not
# dlopen libfuse.so.2.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64
export LDAI_OUTPUT=$appimage
export LDAI_NO_APPSTREAM=1
export LDAI_RUNTIME_FILE=$runtime

(
  cd "$work"
  "$linuxdeploy" \
    --appdir "$appdir" \
    --executable "$deploy_bin" \
    --executable "$deploy_mcp" \
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
# `fell mcp` runs the fell-mcp next to fell, so both must be in usr/bin.
for name in fell fell-mcp; do
  if ! grep -Eq "/usr/bin/$name\$" "$work/appimage-files.txt"; then
    echo "AppImage is missing usr/bin/$name" >&2
    exit 1
  fi
done

if readelf -l "$appimage" | grep -q 'Requesting program interpreter'; then
  echo "AppImage runtime is dynamically linked; it must be the static type2 runtime" >&2
  exit 1
fi
if strings -a "$appimage" | grep -F 'libfuse.so.2'; then
  echo "AppImage runtime requires libfuse.so.2" >&2
  exit 1
fi
runtime_version_text=$("$appimage" --appimage-version 2>&1 || true)
if ! grep -q 'type2-runtime' <<<"$runtime_version_text"; then
  echo "AppImage static runtime did not start:" >&2
  printf '%s\n' "$runtime_version_text" >&2
  exit 1
fi
echo "$runtime_version_text"

if ! readelf -d "$stage/fell" | grep NEEDED | grep -q 'libwebkit2gtk'; then
  echo "fell must dynamically link libwebkit2gtk; the newsletter view needs the distro package" >&2
  exit 1
fi
echo "fell dynamically links WebKitGTK; launch requires the distro package, artifacts do not bundle it"

# The packaged fell-mcp starts and lists its commands.
mcp_help=$("$stage/fell-mcp" --help || true)
if ! grep -q 'fell-mcp add HARNESS' <<<"$mcp_help"; then
  echo "packaged fell-mcp did not run" >&2
  exit 1
fi

echo "tarball:  $tarball"
echo "AppImage: $appimage"
