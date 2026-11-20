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

## Live GUI verification (proven recipe — use it, don't re-derive)

When a change needs proof in the running app (not just tests), Jordan's live
niri session can be driven directly; opening a window on screen is fine.

1. Build a scratch directory tree under the session scratchpad (never the
   repo) and launch against it: `target/debug/saola-files <dir>` in a
   background Bash call.
2. Find the window with `niri msg --json windows` (app-id contains "saola");
   `niri msg action fullscreen-window --id <ID>` makes coordinates
   predictable. `niri msg --json outputs` gives logical size + scale.
3. Screenshot with `grim shot.png` and **Read the PNG — look at it**; map
   screenshot pixels to pointer coordinates by comparing the image size to
   the output's logical size (scale factor).
4. Inject input with `ydotool` (daemon is enabled):
   `ydotool mousemove --absolute -x X -y Y`, `ydotool click 0xC0` (left),
   `ydotool click --repeat 2 --next-delay 80 0xC0` (double).
5. Screenshot again and verify the *content* changed as claimed —
   before/after pairs are the evidence. Then kill the app.

Never use `slurp` (blocks waiting for a human). Two windows with the same
app-id means a stale instance survived — kill both and relaunch.

The live check is **serialized**: one niri session, one pointer. Only the
orchestrator runs it, on `main` after merge — never a parallel task agent.
Screenshots stay in the local scratchpad; the repo is public, so issues and
PRs carry worded results only.

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

## Task workflow

- **The board is the plan.** Work is GitHub issues on the "Saola" user
  Project (https://github.com/users/JorDunn/projects/2): epics are
  milestones with sub-issues, ordering is "blocked by" links. PLAN.md is
  retired and gitignored — don't recreate it.
- One task = one issue = one branch `task/<issue>-<slug>` = one PR. Never
  push to `main`; PRs land by squash merge only.
- The PR title is the Conventional Commit: it becomes the squash commit and
  the changelog line, and CI's `pr-title` check enforces it. The body has
  `Closes #<n>` and the notes the next task needs; a `BREAKING CHANGE:`
  footer goes in the PR body.
- Never hand-edit the version or CHANGELOG.md (release-plz owns both).
- The toolchain is pinned: `rust-toolchain.toml` and the workflows'
  `toolchain:` inputs move together, by PR.
- Dispatching, review and merge mechanics: `docs/DISPATCH.md`.

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
- This app is a window: `container::window(t, s)` (24 px radius, 2 px
  border, window shadow), self-drawn 46 px header (`sizes.window_header`).
  The surface is the `surface` config knob (default `Surface::Paper`),
  threaded from `App` through every view. **No minimise button** — niri has
  no taskbar.
- Icons are Lucide outline, 24×24 viewBox, `stroke-width="2.75"` baked into
  the asset, tinted at draw time. The assets and the `Icon` enum live in
  `saola_theme::icon`; `src/icons.rs` here only maps domain types onto it.
- Tabular numerals on size and date columns. Only sanctioned animations:
  hover, popover, notification, breathe. No spinners.
- `docs/SAOLA-STYLE-GUIDE.md` is a verbatim copy of the spec and wins over
  any implementation; run every new surface through its §11 checklist.
- **One planned exception, pending on saola-theme:** the embedded terminal's
  viewport may render a full ANSI palette once saola-theme ships the
  style-guide exception and its tokens. Until that release exists it is not
  available — don't hardcode a palette. All chrome around the viewport stays
  three-colour.

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
  The window's ground is one of those knobs — `surface = "paper"|"ink"`,
  default paper, resolved to a `saola_theme::Surface` once on `App` and
  threaded through window/explorer; popovers, the undo toast and the four
  modal dialogs never follow it (their surfaces are spec-pinned).
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
- **Pure Rust only (binding):** no C-library bindings — protocols, codecs
  and decoders alike. Where no suitable crate exists, it is written in its
  own repository outside the Saola project; this repo carries a brief under
  `docs/external-crates/` and a `blocked:external` task, and the feature
  waits for the crate.
- **Releases:** Conventional Commits; release-plz opens the release PR;
  never hand-edit the version or CHANGELOG.md. Tags are
  `saola-files-vX.Y.Z`.

## Merge-safe conventions

Several agents work in parallel, so shared choke points are laid out to
merge cleanly. **These take effect with the hot-file split** (the
`main.rs` / `dirview/mod.rs` refactor), which has **not landed yet** — until
it does, the code does not look like this; follow the existing layout.

- One enum variant, struct field, initialiser line and `update` arm per
  feature, each placed under an alphabetical marker comment — never
  reorder or regroup neighbours.
- Each feature exposes its own `subscription()` and `layer()`; the app
  batches them, one line per feature.
- One `read_<feature>(&Table)` per config feature; the top-level config
  reader only calls them.
- Module lists (`mod` / `pub mod`) and Cargo dependencies alphabetical, one
  per line.
- Context-menu entries are an ordered table, one line per entry.

Already true today, independent of the split: every new icon glyph is a
saola-theme release (`src/icons.rs` only maps onto `saola_theme::icon::Icon`).
Request it there; never inline an asset.

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
- No notifications daemon. Calling `org.freedesktop.Notifications` as a
  client is allowed.
- No network-configuration UI.
- Archives arrive as an `archive://` backend module, like any protocol.
- The app itself never elevates. An admin helper, when built, is a separate
  polkit-authorised binary.
- Never run `sudo`; never edit Jordan's niri or user config — print the
  commands and wait.
