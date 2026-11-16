# Fell Linux build

x86_64 CI build from `main`. This is not a GitHub Release.

## Tarball

`fell-x86_64-linux.tar.gz` contains:

- `fell` — the binary
- `dev.predmore.fell.desktop`
- `dev.predmore.fell.png`

Run `./fell`. For a launcher, copy the binary onto `PATH`, the desktop file to `~/.local/share/applications/`, and the icon to `~/.local/share/icons/hicolor/256x256/apps/`.

## AppImage

`fell-x86_64.AppImage` is the other CI artifact. `chmod +x` it after download; Actions stores artifacts in a zip and does not keep the executable bit. It embeds the static type 2 runtime, so it does not need `libfuse2`.

## WebKitGTK

`fell` dynamically links WebKitGTK. `readelf -d` on the binary has a `libwebkit2gtk` `NEEDED` entry, so the dynamic loader requires the distro package before `main`:

- Debian or Ubuntu: `libwebkit2gtk-4.1-0`
- Arch: `webkit2gtk-4.1`

The library is not in the tarball or the AppImage. Compiling the link needs `libwebkit2gtk-4.1-dev` on the nightly job ([issue #8](https://github.com/predmore/fell/issues/8)).
