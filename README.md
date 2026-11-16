# Fell

A fast, native Proton Mail client written in Rust. Threads are first-class. The UI is GPUI on Wayland. AI agents will talk to a local store over MCP — they can search, summarize, triage, and draft, but they cannot send mail without you confirming it in the UI.

## Why this shape

- **Local store, always.** The UI reads SQLite (with FTS). Sync fills the store in the background. The window never waits on IMAP or Proton's API.
- **Bridge now, Proton API later.** Backend 1 is Proton Mail Bridge over local IMAP/SMTP. A later backend will use Proton's API directly, behind the same `MailBackend` trait.
- **Threads first.** Proton conversation ids when we have them, `References` / `In-Reply-To` when we don't.
- **Agents are guests.** MCP tools see the local index. Sending is a UI-only confirmation path.

Bridge is GPL-3.0 and runs as a **separate process**. Fell does not link it and stays MIT. Details in [docs/licensing.md](docs/licensing.md). Architecture: [docs/architecture.md](docs/architecture.md).

## Workspace

```text
crates/fell-core   accounts, backend trait, sync, SQLite+FTS, threading, search
crates/fell        GPUI app (Wayland window)
crates/fell-mcp    MCP server stub (search / summarize / triage / draft)
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

`fell` opens an empty window against an in-memory store. There is no live IMAP yet; the Bridge backend is a typed stub.

## License

MIT, copyright Caleb Predmore. See [LICENSE](LICENSE).
