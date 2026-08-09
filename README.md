# saola-files

The file manager for [Saola](https://github.com/JorDunn/saola-theme), a Linux
desktop environment built in Rust (iced + niri/Wayland).

An ordinary toplevel window — the first in the DE — drawn as an ivory
`paper_window` with a self-drawn 46 px header, following the
[Saola style guide](docs/SAOLA-STYLE-GUIDE.md).

## Status

Early development. The staged build plan targets a v0.1.0 with: places
sidebar, breadcrumb navigation, list/grid views, full file operations with
progress and undo, freedesktop trash, thumbnails, SFTP remote browsing, and
`org.freedesktop.FileManager1` D-Bus integration.

## Building

```bash
cargo build --release                 # default feature set (includes sftp)
cargo build --no-default-features     # local-filesystem-only binary
```

Protocol backends are feature-gated modules in `src/modules/` — pick your set
at build time with `--features`.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
