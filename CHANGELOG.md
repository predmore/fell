# Changelog

## Unreleased

- Add `--version` to `fell` and `fell-mcp`. The inbox command palette's **About** entry shows the same build version as the logs.
- Focus on Proton Mail Bridge. Onboarding drops the Fastmail, Gmail, and iCloud presets; **Other IMAP account** goes straight to the server settings. `save_sent = "auto"` no longer detects Gmail hosts, so set `save_sent = "server"` on a server that files its own Sent copy.
- Build and test tagged Linux releases on amd64 and arm64. Each release includes an AppImage and a tarball per architecture, with a shared checksum manifest.

## 0.1.0

- Native Wayland inbox with threaded mail, vim navigation, and Proton Mail Bridge or generic IMAP sync.
- Markdown compose with human confirmation before sending.
- Local search and scoped agent access over MCP.
