# Fell Linux build

x86_64 CI build from `main`. This is not a GitHub Release.

## Tarball

`fell-x86_64-linux.tar.gz` contains:

- `fell` — the binary
- `fell-mcp` — the MCP server for agents; `fell mcp …` runs it
- `dev.predmore.fell.desktop`
- `dev.predmore.fell.compose.desktop`: hidden; it gives the compose window (app id `dev.predmore.fell.compose`) the Fell icon
- `dev.predmore.fell.png`

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

`fell-x86_64.AppImage` is the other CI artifact. `chmod +x` it after download; Actions stores artifacts in a zip and does not keep the executable bit. It embeds the static type 2 runtime, so it does not need `libfuse2`. It carries `fell-mcp` too: `./fell-x86_64.AppImage mcp add codex` connects an agent, and the entry it writes starts the AppImage with `mcp serve …`.

## WebKitGTK

`fell` dynamically links WebKitGTK. `readelf -d` on the binary has a `libwebkit2gtk` `NEEDED` entry, so the dynamic loader requires the distro package before `main`:

- Debian or Ubuntu: `libwebkit2gtk-4.1-0`
- Arch: `webkit2gtk-4.1`

The library is not in the tarball or the AppImage. Compiling the link needs `libwebkit2gtk-4.1-dev` on the nightly job ([issue #8](https://github.com/predmore/fell/issues/8)).
