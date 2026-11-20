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

### Desktop entry

The file `resources/io.saola.Files.desktop` is a desktop entry for Files.
Install it in `~/.local/share/applications/` for one user, or in
`/usr/share/applications/` for all users on the system.

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

# Keyboard shortcuts: action name = "chord" or ["chord", ...]. See
# "Keyboard shortcuts" below for the grammar, names and conflict rules.
[keys]
new_folder = "ctrl+alt+n"
```

Every knob above degrades independently: an unrecognized `surface`/`view`/
`sort` value, a non-boolean where a boolean is expected, or a malformed
`[[action]]`/`[[server]]` entry (missing `name`/`exec`/`uri`) prints one
named warning and falls back to that knob's default (or, for array-of-table
entries, skips just that entry) — every other knob and entry in the file
still takes effect.

## Keyboard shortcuts

Every binding resolves through the table in `src/keymap.rs` into an `Action`
enum; nothing downstream ever matches a raw key. Each row has a stable action
name, which is how `files.toml` refers to it. Cursor-left/right are a no-op in
list view (there is no "next column over").

| Action name | Default chord(s) | What it does |
|---|---|---|
| `move_cursor_up` | `up` | Move cursor up |
| `move_cursor_down` | `down` | Move cursor down |
| `move_cursor_left` | `left` | Move cursor left |
| `move_cursor_right` | `right` | Move cursor right |
| `move_cursor_home` | `home` | Move cursor to first item |
| `move_cursor_end` | `end` | Move cursor to last item |
| `move_cursor_page_up` | `pageup` | Move cursor up a page |
| `move_cursor_page_down` | `pagedown` | Move cursor down a page |
| `extend_selection_up` | `shift+up` | Extend selection up |
| `extend_selection_down` | `shift+down` | Extend selection down |
| `extend_selection_left` | `shift+left` | Extend selection left |
| `extend_selection_right` | `shift+right` | Extend selection right |
| `extend_selection_home` | `shift+home` | Extend selection to first item |
| `extend_selection_end` | `shift+end` | Extend selection to last item |
| `extend_selection_page_up` | `shift+pageup` | Extend selection up a page |
| `extend_selection_page_down` | `shift+pagedown` | Extend selection down a page |
| `toggle_cursor_selected` | `ctrl+space` | Toggle selection of the cursor item |
| `select_all` | `ctrl+a` | Select all |
| `descend` | `enter` | Open selection |
| `ascend` | `backspace`, `alt+up` | Go to parent folder |
| `history_back` | `alt+left` | Back |
| `history_forward` | `alt+right` | Forward |
| `edit_path` | `ctrl+l` | Edit location |
| `refresh` | `f5` | Refresh |
| `toggle_hidden` | `ctrl+h` | Show or hide hidden files |
| `set_view_list` | `ctrl+1` | List view |
| `set_view_grid` | `ctrl+2` | Grid view |
| `copy` | `ctrl+c` | Copy |
| `cut` | `ctrl+x` | Cut |
| `paste` | `ctrl+v` | Paste |
| `rename` | `f2` | Rename |
| `new_folder` | `ctrl+shift+n` | New folder |
| `delete` | `delete` | Move to Trash or delete |
| `permanent_delete` | `shift+delete` | Delete permanently |
| `undo` | `ctrl+z` | Undo |
| `properties` | `alt+enter` | Properties |
| `open_context_menu` | `menu`, `shift+menu`, `shift+f10` | Open context menu |

Not in the table, so not rebindable: text editing inside inputs (the focused
field consumes those keys), **Escape** closing the popover, rename box or
path editor in front, and type-ahead (a bare letter or digit jumps to the
entry that starts with it).

### Rebinding with `[keys]`

```toml
[keys]
new_folder = "ctrl+alt+n"          # one chord: replaces the default
rename = ["f2", "ctrl+r"]          # several chords for one action
undo = []                          # unbound (`""` works too)
```

* **Chords** are modifiers and one key joined by `+`, case-insensitive:
  `ctrl+shift+n`, `alt+up`, `f2`, `menu`, `shift+delete`. Modifiers are
  `ctrl`, `shift`, `alt` (each once; Super/Logo is not supported). Keys are
  a single character (`n`, `1`, `,`; write the plus key as `plus`) or a name:
  `up down left right home end pageup pagedown enter backspace delete insert
  tab space menu f1`..`f12` (also `return`, `del`, `pgup`, `pgdn`, `esc`).
* **Layout:** chords name the *logical* key, the character your layout
  produces, not a physical position. `ctrl+z` is whichever key types "z" for
  you, so Ctrl+C stays Ctrl+C on Dvorak or AZERTY. On a layout that cannot
  type Latin letters, bind named keys or digits.
* A character key must carry `ctrl` or `alt` (bare letters are type-ahead),
  and `escape` cannot be bound.
* **Shifted symbols:** the app is told the key *without* Shift, so write
  the unshifted key plus `shift`. On a US layout `ctrl+plus` can never fire
  (the event is `=` with Ctrl+Shift): write `ctrl+shift+=`. Likewise `?` is
  `ctrl+shift+/`.
* Ctrl, Shift and Alt are matched exactly: `ctrl+a` does not fire when Shift
  is also held. Super is ignored (the compositor owns Super chords), so
  `alt+x` also fires with Super held.
* A value **replaces** that action's defaults. Names are the first column
  above.
* **Problems warn once, by name, on stderr, and never stop the app.** An
  unknown action name, a value that is not a string or array of strings, or
  a chord that does not parse affects only that entry; if an action's whole
  value is unusable it keeps its defaults.
* **Conflicts:** if you bind a chord that is another action's *default*,
  yours wins and that default is unbound, with a warning naming both. If you
  bind one chord to two actions, the action whose name sorts first
  alphabetically keeps it and the other loses that chord, with a warning.
* Keys are read at start-up; restart to apply changes.

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
