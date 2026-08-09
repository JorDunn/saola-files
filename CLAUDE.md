# saola-files — agent instructions

GUI file manager for Saola, a Linux desktop environment built in Rust
(iced 0.14 + zbus) targeting the niri Wayland compositor. This is the DE's
first **ordinary toplevel window** — every sibling (panel, capture,
lockscreen) is layer-shell or session-lock. There is no `iced_layershell`
here and never should be.

## Commands

```bash
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings   # CI gate — keep it green
cargo fmt --check                                          # CI gate
cargo build --no-default-features                          # CI gate — local-only build must stay green
cargo run                                                  # needs a Wayland session (niri)
```

## Architecture

Single binary crate, three layers with a hard import rule: `core/` never
imports `ui/`; `ui/` never imports `integration/`; `integration/` reaches the
app only through a bounded event channel. `core/` stays iced-free except
`core/thumbs.rs` (produces `iced::widget::image::Handle`s — the one
documented exception).

- `src/core/vfs.rs` — `Location`, `Caps`, the `Backend` trait. All file
  access goes through a `Backend`; the app never calls `std::fs` directly
  outside `src/modules/`.
- `src/modules/<name>.rs` — protocol backends, selected at build time via
  cargo features (`local` is always compiled; `sftp` behind
  `feature = "sftp"`). Adding a protocol = one module file + one feature
  entry + its optional deps. Registry in `modules/mod.rs`.
- `src/ui/explorer.rs` — sidebar + breadcrumbs + directory view composed
  behind one state/view. This is the seam a future saola-portal embeds as
  the file picker; keep it free of app-window concerns.
- `DirectoryView` (`src/ui/dirview/`) is self-contained per-directory state;
  the app holds `Vec<DirectoryView> + active` (the tabs seam — UI shows one
  for now). Shared caches (thumbs, mime, apps, undo, clipboard, ops engine)
  live on the App, never per-view.
- Capability-honest UI: a backend without `TRASH` gets permanent delete
  worded as such; without `WATCH`, refresh-on-navigate + F5; without
  `LOCAL_PATH`, open = download-to-temp with a read-only caveat.

The approved staged build plan lives with the session task list; stages are
one coherent PR each, CI green throughout.

## Design language (binding)

- `saola-theme` is a git dependency **pinned to a release tag**, never
  `branch = "main"`. `tag` and `version` move together. Bumping the tag is a
  deliberate, reviewed change.
- **Zero hardcoded colors or sizes.** Every value from `saola_theme::tokens`,
  every widget style from `saola_theme::style`. If a style is missing, add it
  to saola-theme (and note the pending tag bump at the definition site) —
  never restyle locally.
- Three colors, never a fourth: ink `#0C0A00`, paper `#FFFFF0`, terracotta
  `#C67139`. No danger/success/warning color — severity is carried by
  wording. Mimetype differentiation is **glyph shape only, never hue**.
- This app is a window: `Surface::Paper`, `container::paper_window` (24 px
  radius, 2 px border, window shadow), self-drawn 46 px header
  (`sizes.window_header`). **No minimise button** — niri has no taskbar.
- Icons are Lucide outline, 24×24 viewBox, `stroke-width="2.75"` baked into
  the asset, tinted at draw time. Copy the panel's `src/icons.rs` pattern
  (Icon enum + `include_bytes!` + stroke-width asset tests).
- Tabular numerals on size and date columns. Only sanctioned animations:
  hover, popover, notification, breathe. No spinners.
- `docs/SAOLA-STYLE-GUIDE.md` is a verbatim copy of the spec and wins over
  any implementation; run every new surface through its §11 checklist.

## Conventions

- **No-panic rule (binding):** no `panic!`/`unwrap`/`expect`/indexing on any
  runtime path. Missing services (udisks, D-Bus) degrade to rendering
  nothing or a worded empty state, never take the app down. EACCES is a
  normal Tuesday.
- **OsString discipline:** filenames are `OsString`/`PathBuf` end-to-end;
  `to_string_lossy` only at view time. SFTP names are bytes too.
- **Config:** `~/.config/saola/files.toml`, hand-walked `toml::Table` —
  never `#[derive(Deserialize)]` on the config struct — so each bad knob
  gets its own named warning. Resolution chain: `--config-dir` >
  `$SAOLA_CONFIG_DIR` > `$XDG_CONFIG_HOME/saola` > `~/.config/saola` (empty
  env vars count as unset). No file → silent defaults; unparseable → one
  warning + defaults, still start; one bad knob → warn on that knob only.
- **Signal, never poll:** inotify for local watching; backends that can't
  signal declare it via `Caps` and the UI refreshes on navigate instead.
  Nothing ticks without a documented exception.
- **One async runtime:** iced's tokio executor and zbus's tokio integration
  share it; never construct a second.
- **Messages:** nested enums per module (`Message::Sidebar(sidebar::Message)`);
  keyboard input resolves through `keymap.rs` to an `Action` enum — `update`
  consumes Actions, never raw key events.
- **Async → iced bridging:** `iced::futures::channel::mpsc`, bounded,
  `try_send`, never `tokio::sync` types inside messages. Blocking prompts
  (op conflicts, auth) use a capacity-1 reply `mpsc::Sender` (capture's
  pattern).
- **Testing:** pure logic unit-tested inline (`#[cfg(test)] mod tests`);
  D-Bus and compositor behind traits with fakes; **never `std::env::set_var`
  in a test** — resolve env at a thin wrapper and test the argument-taking
  half. Anything mapping surfaces or grabbing input is tested in nested niri.
- **Teaching notes:** Jordan is newer to Rust — comment non-obvious async
  ownership, macro behaviour, and stream bridging; prefer explicit code over
  clever abstraction.
- **Dependencies:** every non-trivial addition carries a dated survey
  comment in Cargo.toml (alternatives considered, why they lost).
- **Releases:** Conventional Commits; release-plz opens the release PR;
  never hand-edit the version or CHANGELOG.md. Tags are
  `saola-files-vX.Y.Z`.

## iced 0.14 gotchas (verified — don't re-derive)

The full list lives in saola-theme's CLAUDE.md; the ones that bite here:

- Copy token values into locals *before* a `move` style closure (E0700).
- `Element` is not `Clone`; consume `Vec<Element>` with `.into_iter()`.
- `button` has no `Status::Focused` — keyboard focus is app state, drawn
  with `style::focus_border`. A button without `.on_press` renders
  `Disabled` **and does not capture its press** — wrap must-swallow chrome
  in `mouse_area(...).on_press(noop)`.
- `pick_list` needs `.style(...)` *and* `.menu_style(...)`.
- No `horizontal_space`/`vertical_space` — use `Space`.
- Single-window Tasks: `window::latest().and_then(window::drag)` etc.
- With `decorations: false, transparent: true`, the app `Style` must set
  `background_color: Color::TRANSPARENT` or iced clears the surface to ink
  and the rounded corners render as square ink wedges.
- There is **no virtualized list widget** (`lazy` is memoization only):
  directory views render only the visible slice between two sized spacers.
  Never build 100k Elements.

## Boundaries (binding)

- No xdg-desktop-portal implementation here — a future saola-portal owns
  that; this repo only keeps `ui/explorer.rs` embeddable.
- No notifications daemon, no network-configuration UI, no archive
  extraction (future `archive://` backend), no drag-and-drop in v1.
- Never run `sudo`; never edit Jordan's niri or user config — print the
  commands and wait.
