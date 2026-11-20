# Fell Linux build

Tagged GitHub Releases ship Linux builds for `x86_64` (amd64) and `aarch64` (arm64). The nightly Actions artifacts from `main` use unversioned x86_64 names. See the project README for release downloads and checksum verification. [Install from GitHub Releases](https://github.com/predmore/fell#install-from-github-releases)

## Tarball

Each `fell-<version>-<arch>-linux.tar.gz` release archive contains:

- `fell` — the binary
- `fell-mcp` — the MCP server for agents; `fell mcp …` runs it
- `dev.predmore.fell.desktop`
- `dev.predmore.fell.compose.desktop`: hidden; it gives the compose window (app id `dev.predmore.fell.compose`) the Fell icon
- `dev.predmore.fell.png`
- `LICENSE` and `README.md`

Run `./fell`. For a launcher, copy both binaries onto `PATH` (keep them in the same folder), both desktop files to `~/.local/share/applications/`, and the icon to `~/.local/share/icons/hicolor/256x256/apps/`.

## Hyprland

The compose window has its own app id, so a rule can open it floating and centred instead of tiled:

```lua
hl.window_rule({
    name  = "fell-compose",
    match = { class = "^dev\\.predmore\\.fell\\.compose$" },
    float = true,
    size = "1000 780",
    center = true,
})
```

## AppImage

Run `chmod +x fell-<version>-<arch>.AppImage` before launching. The file embeds the static type 2 runtime. On systems without FUSE mounting, set `APPIMAGE_EXTRACT_AND_RUN=1` when launching. The AppImage also carries `fell-mcp`. Run the AppImage with `mcp add codex` to connect an agent. The resulting entry starts the same AppImage with `mcp serve …`.

## WebKitGTK

`fell` dynamically links WebKitGTK. `readelf -d` on the binary has a `libwebkit2gtk` `NEEDED` entry, so the dynamic loader requires the distro package before `main`:

- Debian or Ubuntu: `libwebkit2gtk-4.1-0`
- Arch: `webkit2gtk-4.1`

Both package formats use the host's GTK and WebKitGTK libraries. Building on Ubuntu requires `libwebkit2gtk-4.1-dev`.
