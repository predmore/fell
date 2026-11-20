#!/usr/bin/env bash
# Native Linux tarball and AppImage. Both carry fell and fell-mcp.
#
# fell links WebKitGTK dynamically (libwebkit2gtk in DT_NEEDED). The
# distro package is required to launch: libwebkit2gtk-4.1-0 (Debian/Ubuntu)
# or webkit2gtk-4.1 (Arch). The nightly apt dev package is
# https://github.com/predmore/fell/issues/8
# linuxdeploy excludes libwebkit2gtk and libjavascriptcoregtk, and this
# script fails if either artifact contains them.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
target=${CARGO_TARGET_DIR:-"$root/target"}
bin=${BIN:-"$target/release/fell"}
# The nightly builds only `-p fell`; build fell-mcp here unless MCP_BIN
# names a binary. Cargo does nothing when it is already fresh.
mcp_bin=${MCP_BIN:-}
dist=${DIST_DIR:-"$root/dist"}
mkdir -p "$dist"
dist=$(cd "$dist" && pwd)
arch=$(uname -m)
version=${FELL_RELEASE_VERSION:-}
if [[ -n $version && ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc(\.[0-9]+)?)?$ ]]; then
  echo "invalid release version: $version" >&2
  exit 1
fi
name=fell${version:+-$version}-$arch
stage=$dist/$name-linux
appdir=$dist/AppDir
work=$dist/work
tools=$dist/tools
here=$root/packaging/linux
tarball=$stage.tar.gz
appimage=$dist/$name.AppImage
linuxdeploy_version=1-alpha-20251107-1
# Static type2 runtime (musl, libfuse linked in). Not the classic runtime
# that dlopens libfuse.so.2, which Ubuntu 24.04 does not ship.
runtime_version=20251108
case "$arch" in
  x86_64)
    linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
    runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    linuxdeploy_sha256=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
    runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *)
    echo "unsupported Linux architecture: $arch" >&2
    exit 1
    ;;
esac

if [[ ! -x $bin ]]; then
  echo "missing release binary: $bin" >&2
  echo "build it with: cargo build --release --locked -p fell" >&2
  exit 1
fi

fell_dynamic=$(readelf -d "$bin")
if ! grep -Eq 'NEEDED.*libwebkit2gtk-4\.1' <<<"$fell_dynamic"; then
  echo "fell must dynamically link libwebkit2gtk-4.1" >&2
  exit 1
fi

if [[ -z $mcp_bin ]]; then
  (cd "$root" && cargo build --release --locked -p fell-mcp)
  mcp_bin=$target/release/fell-mcp
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

version_text=$("$bin" --version)
if [[ -n $version ]]; then
  commit=$(TZ=UTC git -C "$root" show -s --format='%H %cd' --date=format-local:%Y-%m-%d HEAD)
  read -r sha date <<<"$commit"
  expected="fell $version (${sha:0:12}, $date)"
  if [[ $version_text != "$expected" ]]; then
    echo "expected $expected, got $version_text" >&2
    exit 1
  fi
fi
if [[ -z $version_text || $("$mcp_bin" --version) != "$version_text" ]]; then
  echo "fell and fell-mcp report different versions" >&2
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
rm -f "$appimage"
mkdir -p "$stage" "$work" "$tools"
install -Dm 0644 "$root/LICENSE" "$appdir/usr/share/doc/fell/LICENSE"

install -m 0755 "$bin" "$stage/fell"
install -m 0755 "$mcp_bin" "$stage/fell-mcp"
install -m 0644 "$here/dev.predmore.fell.desktop" "$stage/dev.predmore.fell.desktop"
install -m 0644 "$here/dev.predmore.fell.compose.desktop" "$stage/dev.predmore.fell.compose.desktop"
install -m 0644 "$here/dev.predmore.fell.png" "$stage/dev.predmore.fell.png"
install -m 0644 "$here/README.md" "$stage/README.md"
install -m 0644 "$root/LICENSE" "$stage/LICENSE"

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
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$(git -C "$root" show -s --format=%ct HEAD)
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$dist" -czf "$tarball" "$name-linux"

# Artifact check: both programs are in the tarball and executable.
mkdir "$work/tarball"
tar -C "$work/tarball" -xzf "$tarball"
for program in fell fell-mcp; do
  if [[ $("$work/tarball/$name-linux/$program" --version) != "$version_text" ]]; then
    echo "tarball $program did not report $version_text" >&2
    exit 1
  fi
done

linuxdeploy=$tools/linuxdeploy-$arch.AppImage
if [[ ! -f $linuxdeploy ]] || ! echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c --status -; then
  curl -fsSL --retry 3 -o "$linuxdeploy" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/${linuxdeploy_version}/linuxdeploy-$arch.AppImage"
  echo "$linuxdeploy_sha256  $linuxdeploy" | sha256sum -c -
  chmod +x "$linuxdeploy"
fi

runtime=$tools/runtime-$arch
if [[ ! -f $runtime ]] || ! echo "$runtime_sha256  $runtime" | sha256sum -c --status -; then
  curl -fsSL --retry 3 -o "$runtime" \
    "https://github.com/AppImage/type2-runtime/releases/download/${runtime_version}/runtime-$arch"
  echo "$runtime_sha256  $runtime" | sha256sum -c -
  chmod +x "$runtime"
fi

deploy_bin=$work/fell
cp "$bin" "$deploy_bin"
chmod +x "$deploy_bin"
deploy_mcp=$work/fell-mcp
cp "$mcp_bin" "$deploy_mcp"
chmod +x "$deploy_mcp"

# Use the system GTK stack as a whole. A bundled GLib, GTK or ICU from the
# build distro can be incompatible with the host's WebKitGTK.
# LDAI_RUNTIME_FILE is the static type2 runtime, so the AppImage does not
# dlopen libfuse.so.2.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=$arch
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
    --exclude-library '*' \
    --output appimage
)

if [[ ! -f $appimage ]]; then
  echo "linuxdeploy did not write $appimage" >&2
  find "$dist" -name '*.AppImage' -print >&2 || true
  exit 1
fi
chmod +x "$appimage"

refuse_webkit "AppDir" "$appdir"

offset=$("$appimage" --appimage-offset)
if ! unsquashfs -o "$offset" -l "$appimage" >"$work/appimage-files.txt"; then
  echo "could not list AppImage contents to confirm WebKitGTK was left out" >&2
  exit 1
fi
if grep -Ei 'webkit2gtk|javascriptcoregtk|WebKitNetworkProcess|WebKitWebProcess|WebKitGPUProcess' "$work/appimage-files.txt"; then
  echo "AppImage bundles WebKitGTK; refusing to redistribute LGPL code" >&2
  exit 1
fi
# `fell mcp` runs the fell-mcp next to fell, so both must be in usr/bin.
for program in fell fell-mcp; do
  if ! grep -Eq "/usr/bin/$program\$" "$work/appimage-files.txt"; then
    echo "AppImage is missing usr/bin/$program" >&2
    exit 1
  fi
done

runtime_headers=$(readelf -l "$appimage")
if grep -q 'Requesting program interpreter' <<<"$runtime_headers"; then
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

echo "fell dynamically links WebKitGTK; launch requires the distro package, artifacts do not bundle it"

for args in --version 'mcp --version'; do
  read -ra command <<<"$args"
  if [[ $("$appimage" "${command[@]}") != "$version_text" ]]; then
    echo "AppImage $args did not report $version_text" >&2
    exit 1
  fi
done
echo "$version_text"

echo "tarball:  $tarball"
echo "AppImage: $appimage"
