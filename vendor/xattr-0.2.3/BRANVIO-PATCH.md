# Vendored xattr 0.2.3 (patched)

Copy of [`xattr` 0.2.3](https://crates.io/crates/xattr/0.2.3) (MIT OR Apache-2.0, see `LICENSE-MIT`
and `LICENSE-APACHE` in this folder) with one change in `src/sys/mod.rs`:

- On Linux and Android, `ENOATTR` is defined as `libc::ENODATA` instead of `libc::ENOATTR`.
  glibc defines `ENOATTR` as an alias of `ENODATA`, and recent `libc` releases (0.2.190 and newer,
  which `tokio`/`mio` require) no longer export the alias, so the unmodified crate fails to build.

Why it is needed: `gpui` 0.2.2 → `gpui_http_client` → `zed-async-tar` 0.5.0-zed depends on
`xattr = "0.2"` on Unix, and 0.2.3 is the last 0.2 release.

Wired in through `[patch.crates-io]` in the workspace `Cargo.toml`. Remove this folder and the patch
entry once `gpui` moves to `xattr` 1.x or a newer `async-tar`.
