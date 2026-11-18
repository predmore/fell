# Licensing

Fell is MIT, copyright Caleb Predmore. See `LICENSE`.

CI runs `cargo deny check`. The allow list is permissive licenses only (MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unicode-DFS-2016, MPL-2.0, and Apache-2.0 WITH LLVM-exception). GPL, AGPL, and LGPL are not allowed; deny fails the build if they appear in the crate graph.

`Apache-2.0 WITH LLVM-exception` is the usual SPDX form for compiler-rt / wasi-related crates. It is permissive and not copyleft. `Unicode-DFS-2016` is the older Unicode data license (the request's "Unicode"); `unicode-ident` uses `Unicode-3.0`.

Two extra permissive licenses appear only as gpui 0.2.2 transitives, so they are crate exceptions rather than a blanket allow:

- `CC0-1.0` on `hexf-parse` (via naga / blade-graphics)
- `CC0-1.0` on `notify` (inotify watcher for the compose draft folder; the spec names this crate)
- `NCSA` on `libfuzzer-sys` (via rav1e / image)

Both are OSI/FSF-free, not copyleft. Drop the exceptions if a newer `gpui` removes those crates.

## Local embedding model

The runtime is `tract-onnx` 0.23 and its `tract-*` crates (MIT OR Apache-2.0). They are pure Rust, link no ONNX Runtime library, and download nothing at build time; `tract-linalg` assembles its bundled kernels with `cc`. The tokenizer is ours, on `unicode-normalization` and `unicode-properties` (MIT OR Apache-2.0). `fell embeddings install` uses `ureq` (MIT OR Apache-2.0) over the existing rustls/ring stack and the system trust store. It does not use `webpki-roots`, whose CDLA-Permissive-2.0 license is not on the allow list.

The weights are not in the repository or in any package. They are downloaded only by `fell embeddings install`, after the person agrees:

- `sentence-transformers/all-MiniLM-L6-v2` at commit `1110a243fdf4706b3f48f1d95db1a4f5529b4d41`. The model card says `license: apache-2.0`. Its base model, `nreimers/MiniLM-L6-H384-uncased`, is MIT. Verified 2026-11-17.
- `onnx/model_qint8_arm64.onnx`, 23,026,053 bytes, SHA-256 `4278337fd0ff3c68bfb6291042cad8ab363e1d9fbc43dcb499fe91c871902474`. The repository publishes the same bytes as `model_qint8_avx512.onnx` and `model_qint8_avx512_vnni.onnx`. The ops are portable, so x86_64 runs it too.
- `vocab.txt`, 231,508 bytes, SHA-256 `07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3`.

Before pinning a different model, re-read its card at the exact commit. Weights under CC-BY-NC, OpenRAIL non-commercial, or any other non-commercial grant are not allowed, even when the surrounding code is MIT.

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

The newsletter view links WebKitGTK dynamically via `wry`. `readelf -d` on `fell` has a `NEEDED` entry for `libwebkit2gtk-4.1.so.0`. The dynamic loader resolves that before `main`, so the distro package is required to launch: `libwebkit2gtk-4.1-0` on Debian/Ubuntu, `webkit2gtk-4.1` on Arch. The crate graph stays MIT/Apache; cargo-deny still rejects LGPL. The tarball and the AppImage do not bundle the library.

Building that link needs the dev package (`libwebkit2gtk-4.1-dev`), including on the nightly artifact job. That install is tracked in [issue #8](https://github.com/predmore/fell/issues/8). See [packaging/linux/README.md](../packaging/linux/README.md).
