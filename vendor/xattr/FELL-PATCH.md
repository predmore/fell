This is xattr 0.2.3 (MIT/Apache-2.0) vendored because gpui 0.2.2
pulls zed-async-tar → xattr 0.2.3, which does not compile on Linux
with current `libc` (no `ENOATTR`; Linux uses `ENODATA`).

Change: `src/sys/mod.rs` maps `ENOATTR` to `libc::ENODATA` on Linux/Android.
