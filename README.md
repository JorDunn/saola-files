# saola-files

The file manager for [Saola](https://github.com/JorDunn/saola-theme), a Linux
desktop environment built in Rust (iced 0.14 + zbus) targeting the niri
Wayland compositor.

An ordinary toplevel window — the first in the DE — drawn as an ivory (or,
by config, ink) window with a self-drawn 46 px header, following the
[Saola style guide](docs/SAOLA-STYLE-GUIDE.md).

## Status

v0.1.0-track. Places sidebar, breadcrumb navigation, list/grid views, full
file operations with progress and undo, freedesktop trash, thumbnails, SFTP
remote browsing, and `org.freedesktop.FileManager1`/`io.saola.Files1` D-Bus
integration (single-instance activation) are all in place. See
`docs/UPSTREAM-THEME-DEBT.md` for open design questions carried in the
shared `saola-theme` crate rather than fixed locally.

## Building

```bash
cargo build --release                 # default feature set (includes sftp)
cargo build --no-default-features     # local-filesystem-only binary
```

Protocol backends are feature-gated modules in `src/modules/` — pick your
set at build time with `--features`. `local` (plain filesystem browsing) is
always compiled in; `sftp` is a default feature and can be dropped with
`--no-default-features` for a smaller, network-free binary.

## Running

```bash
saola-files [OPTIONS] [PATH|URI]

  PATH|URI            directory: browse it; file: reveal it in its parent
                       — `saola-files /some/file` is a universal "reveal"
                       command, and a `scheme://` URI opens that backend
                       directly (e.g. an `sftp://` location already saved
                       under `[[server]]`, see below)
  --select <PATH>     reveal PATH (same as passing a file positional)
  --config-dir <DIR>  read files.toml from DIR instead of the standard
                       resolution chain
  -V, --version       print version and exit
  -h, --help          print usage and exit
```

**Single instance.** The first `saola-files` invocation in a session becomes
the primary instance and serves `org.freedesktop.FileManager1` (the
freedesktop file-manager activation spec — `ShowItems`/`ShowFolders`/
`ShowItemProperties`) and `io.saola.Files1` (a bare `Activate()`, for "just
raise the window") on the session bus. Every subsequent invocation forwards
its `PATH|URI`/`--select` to the running instance over D-Bus and exits
immediately — there is never a second window. This is why a second
`saola-files ~/Downloads` while one is already open just re-navigates the
existing window instead of opening another.

## Configuration

`files.toml`, hand-walked TOML (never `#[derive(Deserialize)]`, so one bad
knob only ever warns and falls back on that knob — the rest of the file
still applies). No file present means silent defaults; an unparseable file
prints one warning and starts with defaults anyway. The app never fails to
start over a bad config.

**Resolution chain** (first match wins, an env var set to the empty string
counts as unset):

1. `--config-dir <DIR>`
2. `$SAOLA_CONFIG_DIR`
3. `$XDG_CONFIG_HOME/saola`
4. `~/.config/saola`

The file itself is `<that directory>/files.toml`.

### Schema

```toml
# Terminal emulator for "Open in terminal" and Terminal=true desktop
# entries. Omit to fall through to $TERMINAL, then "alacritty".
terminal = "alacritty"

# Which ground the *window* draws on: "paper" (ivory, default) or "ink".
# Moves the window chrome, header, sidebar, toolbar and listing. Does NOT
# move popovers/context menus (always ink), the undo toast (always ink), or
# the four modal dialog cards (always ivory) — those are pinned by the
# style guide, not by this knob.
surface = "paper"

# Default directory presentation: "list" or "grid".
view = "list"

# Default sort column: "name" | "size" | "modified" | "type".
sort = "modified"
sort-descending = true

# Show dotfiles by default (toggle at runtime with Ctrl+H).
show-hidden = false

# Generate freedesktop-spec thumbnails for images, and the size cap (MiB)
# above which a source file is skipped rather than decoded.
thumbnails = true
thumbnail-max-mb = 64

# Ask before permanently emptying the trash.
confirm-empty-trash = true

# Zero or more custom context-menu actions. `exec` uses desktop-entry field
# codes (%f = one path, %F = many). Empty mimetypes/schemes means "applies
# everywhere".
[[action]]
name = "Edit as text"
exec = "alacritty -e nvim %f"
mimetypes = ["text/*", "application/json"]

# Zero or more saved remote locations, shown in the places sidebar and
# offered as a one-click target from the connect dialog (Ctrl+L or the
# sidebar's "Connect to Server…" row also take a bare URI ad hoc, without
# needing an entry here).
[[server]]
name = "homelab"
uri = "sftp://jordan@10.0.0.10/srv"
```

Every knob above degrades independently: an unrecognized `surface`/`view`/
`sort` value, a non-boolean where a boolean is expected, or a malformed
`[[action]]`/`[[server]]` entry (missing `name`/`exec`/`uri`) prints one
named warning and falls back to that knob's default (or, for array-of-table
entries, skips just that entry) — every other knob and entry in the file
still takes effect.

## Keyboard shortcuts

Every binding resolves through `src/keymap.rs`'s `resolve(key, modifiers)`
into an `Action` enum — nothing downstream ever matches a raw key. Rows
marked "grid only" are a no-op in list view (there's no notion of "the next
column over").

| Keys | Action |
|---|---|
| ↑ / ↓ | Move cursor up/down a row |
| ← / → | Move cursor a column (grid only) |
| Home / End | Move cursor to first/last entry |
| Page Up / Page Down | Move cursor a page |
| Shift + any of the above | Extend the selection instead of moving alone |
| Ctrl+Space | Toggle the cursor row's selection without touching the rest |
| Ctrl+A | Select all |
| Enter | Descend into the selected directory / open the selection |
| Backspace, or Alt+↑ | Go to the parent directory |
| Alt+← | Back through this view's own navigation history |
| Alt+→ | Forward through history |
| Alt+Enter | Open the properties dialog for the selection |
| Ctrl+H | Toggle showing hidden files |
| Ctrl+L | Edit the current location as a path/URI |
| Ctrl+1 | Switch to list view |
| Ctrl+2 | Switch to grid view |
| F5 | Refresh (manual re-list, for backends without live-watch) |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste |
| F2 | Rename the selection (single entry only) |
| Ctrl+Shift+N | New folder |
| Delete | Trash the selection (or permanently delete, if the backend has no trash) |
| Shift+Delete | Always permanently delete, regardless of trash support |
| Ctrl+Z | Undo the most recent invertible operation |
| Escape | Close whatever dialog/popover/inline editor is open |

The header's mouse-only controls (nav arrows, refresh, view switcher,
Hidden toggle, breadcrumb edit-pencil, the "⋯" overflow menu that opens the
context menu for the current selection) all dispatch the same `Action`
values as their keyboard equivalents — there's exactly one code path per
action, never a separate mouse-only branch.

## Architecture, for contributors

Three layers with a hard import rule: `core/` never imports `ui/`; `ui/`
never imports `integration/`; `integration/` reaches the app only through a
bounded event channel. See `CLAUDE.md` for the full binding conventions
(no-panic rule, OsString discipline, config resilience, one async runtime,
etc.) — that document, not this README, is the source of truth for
contributing code.

### Writing a protocol module

A new remote backend is one file in `src/modules/`, one `Cargo.toml`
feature, and one registry entry — `src/modules/sftp.rs` (`feature =
"sftp"`) is the reference implementation. The checklist:

1. **Implement [`core::vfs::Backend`](src/core/vfs.rs)** for a `Clone`-cheap
   struct (every field an `Arc` around the live session state).
   `#[async_trait]` on the `impl` block — the trait needs it for
   `dyn`-safety.
2. **`caps()` is where you're honest.** Set only the `Caps` bits your
   backend actually supports, and write down *why* each one is set or
   unset next to it — copy `SftpBackend::caps()`'s doc comment shape, not
   just its bits. No `WATCH` means no push notifications (the UI falls back
   to refresh-on-navigate + F5); no `TRASH` means "permanent delete" is
   worded as such, never disguised as a soft delete; no `LOCAL_PATH` means
   "open" downloads to a temp file with a read-only caveat.
3. **A real connection/handshake** (anything that isn't stateless like
   `local`) doesn't go behind `modules::resolve` directly. Write a
   `pub(crate) async fn connect(location: &Location, request:
   &remote::ConnectRequest, tx: &mut mpsc::Sender<remote::ConnectEvent>) ->
   Result<YourBackend, VfsError>` and wire it into
   `core::remote::dispatch_connect`'s `#[cfg(feature = "...")]` arm — one
   `if request.location.scheme == "..."` branch per protocol, feature-gated
   the same way the module declaration itself is gated in
   `modules::mod::resolve`.
4. **Non-UTF-8 filenames.** Check *before* picking a wire library whether it
   forces `String` (not raw bytes) for paths. If it does, that's a
   disclosed gap, not something to silently paper over — say so in the
   `Cargo.toml` survey comment and the module's own doc comment, and route
   every `PathBuf → String` conversion through exactly one named function
   (mirror `modules::sftp::wire_path`) so the gap has one auditable spot.
5. **Errors.** Map the protocol's own error/status codes onto `VfsError` at
   one function. Use specific wording where the protocol distinguishes it
   (not-found, permission-denied); `VfsError::Unavailable` for anything
   transport-shaped — a dead network mid-operation must surface as a worded
   error, never a panic.
6. **Tests.** Pure parsing/mapping logic (authority parsing, path/error
   conversion) is fully unit-testable without a network. An actual
   handshake against a live server usually isn't — no fake server exists in
   the test binary — so that stays a manually-verified, documented gap
   rather than a `#[tokio::test]` that can't run in CI.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
