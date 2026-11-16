# Fell

A fast, native Proton Mail client written in Rust. Threads are first-class. The UI is GPUI on Wayland. AI agents talk to the local store over MCP. They can search, read threads, list labels, triage, and save drafts for you to review. They cannot send mail. Sending stays a confirmation in the Fell window.

## Why this shape

- **Local store, always.** The UI reads SQLite (with FTS). Sync fills the store in the background. The window never waits on IMAP or Proton's API.
- **Bridge now, Proton API later.** Backend 1 is Proton Mail Bridge over local IMAP/SMTP. A later backend will use Proton's API directly, behind the same `MailBackend` trait.
- **Threads first.** Proton conversation ids when we have them, `References` / `In-Reply-To` when we don't.
- **Agents are guests.** MCP tools see the local index. Sending is a UI-only confirmation path.

Bridge is GPL-3.0 and runs as a **separate process**. Fell does not link it and stays MIT. Details in [docs/licensing.md](docs/licensing.md). Architecture: [docs/architecture.md](docs/architecture.md).

## Workspace

```text
crates/fell-core      accounts, backend trait, sync, SQLite+FTS, threading, search, local actions
crates/fell-confirm   UI-only token required to mint a send confirmation
crates/fell           GPUI app (Wayland window)
crates/fell-mcp       MCP server (search / read / triage / review drafts; no send)
```

## Build

Linux (Omarchy / Hyprland / other Wayland) is the first target. You need a recent stable Rust (the repo pins 1.100.0) plus Wayland, Vulkan, and fontconfig headers:

```sh
sudo apt install -y gcc g++ clang pkg-config \
  libfontconfig-dev libfreetype-dev \
  libwayland-dev wayland-protocols \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libvulkan-dev libvulkan1

cargo test --workspace
cargo run -p fell
```

`fell` opens an empty window against an in-memory store. Read-only IMAP sync lives in `fell-core` (`ImapBackend`, and `BridgeBackend` as one IMAP account). It does not send mail. Tests talk to a local IMAP server, not a Proton account.

## Agents

`fell-mcp` speaks MCP over stdio. Tools: `search`, `read_thread`, `list_labels`, `label`, `archive`, `mark_read`, `snooze`, `create_draft`, `list_drafts`, and `undo`. Triage and drafts are written to the local store, logged, and undoable. Drafts stay in Fell for review. There is no send tool.

Agents are off until you grant a scope:

```sh
fell-mcp grant --store /path/to/fell.sqlite --account ACCOUNT --agent AGENT --read --triage --draft
fell-mcp serve --store /path/to/fell.sqlite --account ACCOUNT --agent AGENT
```

`--read` is search, read, and list labels. `--triage` is label, archive, mark read, snooze, and undo of those. `--draft` is saving and listing review drafts, and undo of those. `fell-mcp revoke` turns the agent off again.

## Nightly Linux artifacts

A push to `main` builds a release x86_64 binary and uploads two GitHub Actions artifacts (not a GitHub Release): `fell-x86_64-linux.tar.gz` (binary, `.desktop` file, and icon) and `fell-x86_64.AppImage`.

This nightly binary does not link WebKitGTK. `readelf -d` on the release `fell` has no `libwebkit2gtk` `NEEDED` entry, and `wry` is not a dependency yet, so it launches without the distro package. Neither artifact bundles the library. Once the newsletter fallback is compiled in, `webkit2gtk` is a runtime requirement (`libwebkit2gtk-4.1-0` on Debian/Ubuntu, `webkit2gtk-4.1` on Arch) and must stay dynamically linked, so the LGPL library stays out of the crate graph and out of these artifacts. Details: [packaging/linux/README.md](packaging/linux/README.md).

## License

MIT, copyright Caleb Predmore. See [LICENSE](LICENSE).
