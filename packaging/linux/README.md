# Fell Linux build

x86_64 CI build from `main`. This is not a GitHub Release.

## Tarball

`fell-x86_64-linux.tar.gz` contains:

- `fell` — the binary
- `dev.predmore.fell.desktop`
- `dev.predmore.fell.png`

Run `./fell`. For a launcher, copy the binary onto `PATH`, the desktop file to `~/.local/share/applications/`, and the icon to `~/.local/share/icons/hicolor/256x256/apps/`.

## AppImage

`fell-x86_64.AppImage` is the other CI artifact. `chmod +x` it after download; Actions stores artifacts in a zip and does not keep the executable bit.

## WebKitGTK

WebKitGTK is not in the tarball or the AppImage. The newsletter web view uses `wry`, dynamically linked against the system library, so these builds do not redistribute LGPL code and `cargo deny` stays clean.

Install the distro package:

- Debian or Ubuntu: `libwebkit2gtk-4.1-0`
- Arch: `webkit2gtk-4.1`

The native GPUI UI does not load WebKitGTK. The web view fallback does.
