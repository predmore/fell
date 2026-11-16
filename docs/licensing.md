# Licensing

Fell is MIT, copyright Caleb Predmore. See `LICENSE`.

CI runs `cargo deny check`. The allow list is permissive licenses only (MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unicode-DFS-2016, MPL-2.0, and Apache-2.0 WITH LLVM-exception). GPL, AGPL, and LGPL are not allowed; deny fails the build if they appear in the crate graph.

`Apache-2.0 WITH LLVM-exception` is the usual SPDX form for compiler-rt / wasi-related crates. It is permissive and not copyleft. `Unicode-DFS-2016` is the older Unicode data license (the request's "Unicode"); `unicode-ident` uses `Unicode-3.0`.

Two extra permissive licenses appear only as gpui 0.2.2 transitives, so they are crate exceptions rather than a blanket allow:

- `CC0-1.0` on `hexf-parse` (via naga / blade-graphics)
- `NCSA` on `libfuzzer-sys` (via rav1e / image)

Both are OSI/FSF-free, not copyleft. Drop the exceptions if a newer `gpui` removes those crates.

## Proton Mail Bridge (GPL-3.0)

Backend 1 talks to [Proton Mail Bridge](https://github.com/ProtonMail/proton-bridge) over local IMAP/SMTP. Bridge is GPL-3.0. It is a **separate process**, not linked into Fell, so it does not affect Fell's MIT license. Fell never vendors, statically links, or distributes Bridge.

## Do not use AGPL mail crates

Do **not** copy or depend on `ProtonMail/clients` `project/mail/rust` (formerly `ProtonMail/rust-mail`). That tree is AGPL-3.0. Reading it for API shapes is allowed; copying code or adding it as a Cargo dependency is not.

IMAP parsing uses `mail-parser` (Apache-2.0 OR MIT) and the Rustls stack. That code is ours plus those crates. It is not copied from the Proton tree.

## Allowed Proton / crypto references

Fine to depend on or study, as verified 2026-11-15:

- `ProtonMail/proton-crypto-rs` — MIT
- `rpgp/pgp` — Apache-2.0 or MIT
- `ProtonMail/go-proton-api` — MIT, Go, reference only (not linked)
- `gpui` — Apache-2.0

## GPUI / Zed

Use the Apache-2.0 `gpui` crate and its published dependencies only. Do not take Zed editor crates that are GPL or AGPL (the editor application, some feature crates, etc.). If `cargo deny` flags a GPUI transitive crate, drop or replace it rather than allowing copyleft.

`gpui` 0.2.2 depends on `zed-async-tar`, which pins `xattr` 0.2. That crate does not compile on Linux with current `libc` (`ENOATTR` is not a Linux constant). Fell vendors `vendor/xattr` (still MIT/Apache-2.0, version 0.2.3) and maps `ENOATTR` to `ENODATA` on Linux. Prefer dropping the patch if a newer `gpui` crates.io release fixes the graph.

## MCP and sending

The MCP server is MIT like the rest of the workspace. It speaks stdio only. Agent tools may search, read threads, list labels, triage, and save drafts for review. Sending mail is not an MCP capability; it always needs explicit UI confirmation (`UserSendConfirmation`). Scopes stay off until a person enables them. `fell-mcp` does not depend on `fell-confirm` and cannot mint a send token. A future network transport would have to bind to `127.0.0.1`, use a token file mode `0600`, and check `Origin`.

## WebKitGTK stays on the system

The current nightly `fell` binary does not link WebKitGTK. `readelf -d` shows no `libwebkit2gtk` `NEEDED` entry, and `wry` is not in the crate graph, so `cargo deny` still sees no LGPL. The tarball and the AppImage do not bundle the library. Once the newsletter fallback is compiled in, that view uses WebKitGTK via `wry`, dynamically linked, and the distro package (`libwebkit2gtk-4.1-0` on Debian/Ubuntu, `webkit2gtk-4.1` on Arch) is a runtime requirement. See [packaging/linux/README.md](../packaging/linux/README.md).
