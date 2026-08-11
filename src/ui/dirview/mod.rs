//! `DirectoryView` — self-contained per-directory state (the tabs seam):
//! the app holds `Vec<DirectoryView> + active` (UI shows one for now, see
//! `main.rs`). This module never navigates itself for a *new* location —
//! clicks, Enter, Backspace/Alt+Up, and breadcrumb clicks all resolve to
//! an [`Event`] the owner decides whether to act on, so a future tabs
//! feature can open a new view instead of reusing this one without
//! touching this file.
//!
//! **Per-view history (Stage 4).** `back`/`forward` are browser-style
//! stacks of previously-visited [`Location`]s. [`DirectoryView::navigate`]
//! (the owner's response to `Event::OpenDirectory`) pushes the *old*
//! location onto `back` and clears `forward` — visiting anywhere new
//! invalidates the redo stack, same as a browser. `Action::HistoryBack`/
//! `HistoryForward` are the one exception to "the view never navigates
//! itself": back/forward within a tab's own history is inherently
//! per-view, never a "maybe open in a new tab" situation the way
//! descending into a row is, so [`DirectoryView::go_back`]/`go_forward`
//! call the private `load()` directly instead of bubbling an `Event`.
//!
//! **Header/breadcrumbs (Stage 4).** `ui::header` and `ui::breadcrumbs`
//! sit outside this module's privacy boundary (like `ui::explorer`) and
//! only ever construct [`Message`] values — a click on the header's Back
//! button sends exactly the `Message::Action(Action::HistoryBack)` that
//! Alt+Left resolves to, so there is one code path per behavior regardless
//! of input device. `ui::dirview::grid` (the tile presentation) and
//! `ui::dirview::typeahead` (type-to-select) are the other two Stage 4
//! additions; both are private submodules with the same field-visibility
//! access `list.rs`/`selection.rs` already had.
//!
//! **Live updates (Stage 5).** [`Self::subscription`] wraps `watch.rs`,
//! which resolves the current `Location`'s backend and — if it can signal
//! (`Backend::watch` returns `Some`) — debounces its raw `DirEvent` stream
//! into `Vec<DirEvent>` batches delivered as `Message::Watch`.
//! [`Self::apply_watch_events`] applies a batch incrementally: removals
//! and the "from" half of a rename mutate `entries` synchronously (no
//! network/disk round trip needed to delete a row), while creations,
//! changes, and the "to" half of a rename need one `Backend::metadata`
//! call each (a watch event only carries a name, not size/kind/mtime) —
//! that fetch comes back as `Message::WatchRefreshed`, guarded against
//! staleness exactly like `Message::Listed`. Either way, `recompute_visible`
//! runs at most twice per batch (once for the synchronous half, once for
//! the async one) rather than once per event — the "debounced re-sort" the
//! stage calls for. `selection` is name-keyed (`selection::Selection`)
//! specifically so it survives all of this: `Selection::forget`/`rename`
//! keep a selected entry selected across a rename, or drop it cleanly if
//! it was deleted, without either method needing to know the entry's
//! *position* at the time.

mod grid;
mod list;
mod rename;
mod selection;
mod typeahead;
mod watch;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::SystemTime;

use futures::SinkExt;
use iced::widget::scrollable;
use iced::{Element, Subscription, Task, keyboard};
use saola_theme::Theme;

use crate::config::{Config, CustomAction, SortKey, View};
use crate::core::apps::AppsDb;
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::fs::sort;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::core::vfs::{Caps, DirEvent, Location, VfsError};
use crate::keymap::{self, Action};
use crate::ui::breadcrumbs;
use crate::ui::menus;

/// Cursor rows a single PageUp/PageDown moves — a fixed placeholder rather
/// than the real viewport-height/row-height math, which needs a theme
/// lookup `update()` doesn't otherwise take. Stage 4's navigation-chrome
/// pass can wire this to `DirectoryView::scroll` once it threads sizing
/// through; the keymap contract (`Action::MoveCursorPageUp/Down`) already
/// doesn't change either way.
const PAGE_ROWS: isize = 10;

/// Row-height guess used only to decide which entries are "near the
/// viewport" for thumbnail-request scheduling (Stage 11) — the same
/// placeholder posture as `PAGE_ROWS` just above: `update()` has no
/// `Theme` to read the real `sizes.list_row`/grid tile height from (only
/// `view()` does — see `list.rs`/`grid.rs`'s own pixel-exact virtualization
/// math, which this never touches). Deliberately generous: a miss here
/// only ever costs a few thumbnails requested a little early or late,
/// never a wrong row acted on the way `PAGE_ROWS` being off would be.
const THUMB_ROW_HEIGHT_GUESS: f32 = 40.0;
/// Rows' worth of candidates requested before the first `Scrolled` event
/// lands — mirrors `list.rs`/`grid.rs`'s own pre-viewport `INITIAL_ROWS`
/// fallback.
const THUMB_INITIAL_ROWS: usize = 48;
/// Extra rows beyond the visible band, both sides — generous on purpose,
/// see `THUMB_ROW_HEIGHT_GUESS`'s doc comment.
const THUMB_OVERSCAN_ROWS: usize = 8;

/// Messages this view's `update` consumes. `Listed`/`TargetResolved`
/// arrive from the backend calls `load`/`open_target`/`open_select` kick
/// off; the rest come from `list.rs`'s rendering or the app's keyboard
/// subscription.
#[derive(Debug, Clone)]
pub enum Message {
    /// A `list()` response for `location` — guarded against staleness in
    /// `update`: if `location` no longer matches `self.location` (a
    /// second navigation raced ahead of this one), it's dropped.
    Listed(Location, Result<Vec<FileEntry>, VfsError>),
    /// The combined "probe, then list" response `open_target` kicks off:
    /// the resolved location to actually show (the target itself if it
    /// was a directory, its parent if it was a file), an optional entry
    /// name to select once the listing lands, and the listing itself.
    TargetResolved(Location, Option<OsString>, Result<Vec<FileEntry>, VfsError>),
    /// A click on `visible[index]`. Ctrl/Shift come from `self.modifiers`
    /// (tracked via `Message::Keyboard`'s `ModifiersChanged`, since a
    /// mouse click message carries no modifier state of its own).
    ///
    /// Double-clicks are detected here in `update`, not in the widget
    /// tree: the row is a press-capturing `button` (it must be, for the
    /// themed hover/press styling — see `list.rs::entry_row`), and iced's
    /// `MouseArea` forwards events to its child *first* and returns early
    /// once the child captures them, so an outer
    /// `mouse_area(...).on_double_click(...)` around a button with
    /// `on_press` never sees a single press and can never fire. Instead a
    /// second plain click on the same row within [`DOUBLE_CLICK_WINDOW`]
    /// of the last one (`DirectoryView::last_click`, paired by
    /// [`is_double_click`]) activates the selection.
    RowClicked(usize),
    HeaderClicked(SortKey),
    Scrolled(scrollable::Viewport),
    Keyboard(keyboard::Event),
    /// A keymap [`Action`] arriving from something other than the keyboard
    /// subscription — today, only `ui::header`'s buttons (Back/Forward/Up/
    /// Refresh/the view-mode switcher/the hidden toggle/the breadcrumb
    /// edit pencil). Deliberately reuses the exact same `Action` vocabulary
    /// `handle_keyboard` resolves raw key events into, so a mouse click and
    /// its keyboard equivalent (e.g. the Back button and Alt+Left) drive
    /// the identical `apply_action` arm — one behavior, two input paths.
    Action(Action),
    /// A breadcrumb pill (or the remote-authority pill) was clicked:
    /// request navigating straight there. Bubbles as `Event::OpenDirectory`
    /// like every other "go somewhere new" request in this module.
    BreadcrumbClicked(Location),
    /// The path/URI editor's `on_input`, while `path_edit` is `Some`.
    PathInputChanged(String),
    /// The path/URI editor's `on_submit` (Enter): parse the buffer and
    /// request navigating there.
    PathSubmitted,
    /// A debounced batch of raw watch events for the current location
    /// (`watch.rs`) — see the module docs' "Live updates" section.
    /// Delivered even for a location whose backend can't watch (an empty
    /// stream just never produces this), so `update` never needs to guard
    /// on `Caps::WATCH` itself.
    Watch(Vec<DirEvent>),
    /// The `Backend::metadata` refetch `apply_watch_events` kicks off for
    /// each name a `Watch` batch's creations/changes/rename-destinations
    /// touched. `location` is guarded against staleness exactly like
    /// `Message::Listed`'s: a result for a location that's no longer
    /// `self.location` (a navigation raced ahead of the fetch) is dropped.
    /// `None` per-name means the entry vanished again before the fetch
    /// completed (a create-then-immediate-delete race) — not an error
    /// worth surfacing, just skip inserting it.
    WatchRefreshed(Location, Vec<(OsString, Option<FileEntry>)>),

    // ── Stage 6: context menu / Open-with popover (`ui::menus`) ─────────
    /// The header's overflow button: opens the context menu.
    OpenMenu,
    /// The scrim, Escape, or a menu item that finished its job: closes
    /// whichever of the context menu/Open-with popover is open.
    CloseMenu,
    /// "Open" inside the context menu — the same activation
    /// double-click/Enter already drive, reached from the menu instead.
    MenuOpenSelected,
    /// "Open with…" inside the context menu: swaps it for the Open-with
    /// popover.
    MenuOpenWithRequested,
    /// "Open in terminal" inside the context menu.
    MenuOpenTerminalRequested,
    /// A config-defined `[[action]]` clicked in the context menu, by its
    /// index into `DirectoryView::actions`.
    MenuCustomActionRequested(usize),
    /// An app chosen in the Open-with popover, by its desktop-id.
    OpenWithChosen(String),

    // ── Stage 8: clipboard / rename / new folder / new file ─────────────
    /// The header/context-menu equivalents of `Action::Copy`/`Cut`/`Paste`
    /// — mouse parity for the Ctrl+C/X/V keyboard path, same shape
    /// `Message::Action` already gives every other action a mouse
    /// equivalent through. Closes the context menu first.
    MenuCopyRequested,
    MenuCutRequested,
    MenuPasteRequested,
    /// "Rename…" in the context menu — the mouse path to the same
    /// `start_rename_named` F2 uses.
    MenuRenameRequested,
    MenuNewFolderRequested,
    MenuNewFileRequested,
    /// "Delete"/"Move to Trash" in the context menu (Stage 9) — always
    /// `DeleteMode::ToTrash`; the menu never offers a permanent-delete row
    /// of its own, that's Shift+Delete's job (see `ui::menus`'s doc
    /// comment on why one capability-honestly-worded row is enough).
    MenuDeleteRequested,
    /// The inline rename field's `on_input`, while `self.rename` is `Some`.
    RenameChanged(String),
    /// Enter: commit the inline rename.
    RenameSubmitted,
    /// Escape: abandon the inline rename without touching the backend.
    RenameCancelled,
    /// The `Backend::rename` call `Message::RenameSubmitted` kicks off.
    /// `location` is the directory it was issued against — guarded against
    /// staleness exactly like `Message::Listed`'s (a navigation racing
    /// ahead of a slow rename drops the stale result rather than mutating
    /// a directory that's no longer on screen).
    RenameResult(Location, OsString, Result<(), VfsError>),
    /// The `Backend::mkdir`/`write` call `create_new` kicks off for a New
    /// Folder/New File. Same staleness guard as `RenameResult`; on success
    /// the view reloads and — via `pending_select`/`pending_rename` — both
    /// selects and starts renaming the freshly created entry, so the human
    /// can type its real name immediately instead of "New Folder" then a
    /// second F2.
    CreateResult(Location, OsString, Result<(), VfsError>),

    // ── Stage 13: properties ─────────────────────────────────────────────
    /// "Properties" in the context menu — the mouse path to the same
    /// dialog Alt+Enter opens (`Action::Properties`).
    MenuPropertiesRequested,
}

/// What the owner (the app, via `ui::explorer`) decides to act on. The
/// view only ever *requests* these — see the module docs.
#[derive(Debug, Clone)]
pub enum Event {
    /// Descend into a directory or ascend to its parent. The caller
    /// decides whether this reuses the current view or opens a new tab
    /// (a future stage); Stage 3's `explorer.rs` always reuses it.
    OpenDirectory(Location),
    /// Enter/double-click on non-directory entries: open them (Stage 6:
    /// the owner resolves each `Location`'s default app via `MimeDb`/
    /// `AppsDb` and spawns it — the "files open in the right app" done
    /// criterion).
    Activated(Vec<Location>),
    /// The Open-with popover's choice: open every `Location` with this
    /// desktop-id specifically, bypassing the resolved default.
    OpenWith(Vec<Location>, String),
    /// "Open in terminal": `location` is where the terminal's `cwd` should
    /// land — already resolved to "the directory itself" (`is_dir`) or
    /// "its parent" (a file was targeted) by the view, per the stage's own
    /// wording ("a file opens the terminal in its parent dir").
    OpenTerminal(Location, bool),
    /// A config-defined `[[action]]` was invoked: its raw `exec` string
    /// (field-code expansion is the owner's job, same as `Activated`) and
    /// the targets it should run against.
    RunCustomAction(String, Vec<Location>),
    /// Ctrl+C / the context menu's Copy: put these locations on the app's
    /// internal clipboard (`core::fs::ops::Clipboard`, which lives on
    /// `App` — this view never touches it directly, same "shared caches
    /// live on the App" split `mime_db`/`apps_db` already follow).
    CopyRequested(Vec<Location>),
    /// Ctrl+X / the context menu's Cut.
    CutRequested(Vec<Location>),
    /// Ctrl+V / the context menu's Paste: paste the clipboard's contents
    /// into `location` (always this view's current directory — there is no
    /// "paste into a specific selected folder" this stage).
    PasteRequested(Location),
    /// Delete / Shift+Delete / the context menu's Delete row (Stage 9):
    /// mirrors `CopyRequested`/`CutRequested`'s shape exactly, per the
    /// Stage 8 handoff's own suggestion. `App` decides trash-vs-permanent
    /// per location (`Caps::TRASH`), except when `mode` is
    /// `DeleteMode::Permanent`, which always skips the trash regardless of
    /// capability — see [`DeleteMode`]'s doc comment.
    DeleteRequested(Vec<Location>, DeleteMode),
    /// A successful inline rename (F2 / "Rename…") just landed: `from` is
    /// where the entry was, `to` is where it is now (Stage 10). Bubbled
    /// purely so `App` can push a `core::fs::undo::UndoEntry::Rename` onto
    /// its (App-owned, not per-view — CLAUDE.md) undo stack; this view
    /// already applied the rename to its own `entries`/`selection` before
    /// this fires (`Message::RenameResult`'s `Ok` arm), so nothing here
    /// changes what's on screen.
    Renamed(Location, Location),
    /// A successful New Folder/New File (Ctrl+Shift+N / the context menu)
    /// just landed at `Location` (Stage 10) — the undo counterpart to
    /// [`Event::Renamed`], pushing a `core::fs::undo::UndoEntry::New`.
    Created(Location),
    /// Ctrl+Z: pop and invert the most recent invertible op. Bubbled
    /// rather than handled here because the undo stack itself is
    /// App-owned, shared state (CLAUDE.md: "Shared caches … live on the
    /// App, never per-view") — this view never sees an `UndoEntry` at all,
    /// the same "just ask the owner" shape `Event::DeleteRequested` uses
    /// for the shared clipboard/ops-engine state it can't touch directly
    /// either.
    UndoRequested,
    /// One or more regular files near the viewport have no cached
    /// thumbnail yet (Stage 11) — bubbled so `App` can check `files.toml`'s
    /// `thumbnails`/`thumbnail-max-mb` knobs and dispatch background
    /// generation onto the shared `core::thumbs::ThumbCache`/semaphore/
    /// registry, none of which this view holds (CLAUDE.md: shared caches
    /// live on the App). See [`Self::thumbnail_candidates`] for exactly
    /// which entries qualify and when this fires.
    ThumbnailsNeeded(Vec<ThumbCandidate>),
    /// Alt+Enter / the context menu's "Properties" (Stage 13): open the
    /// properties dialog for the current selection. Carries each selected
    /// entry's `Location` paired with the [`FileEntry`] snapshot already
    /// sitting in this view's own `entries` at the moment of the request —
    /// the dialog's name/mime/modified/permissions rows read straight from
    /// that snapshot (App has no `Backend` round trip to make for data this
    /// view already has in memory); only the live size count is fetched
    /// fresh, by `App`, via `core::fs::size::run`.
    PropertiesRequested(Vec<(Location, FileEntry)>),
}

/// One thumbnail candidate bubbled via [`Event::ThumbnailsNeeded`] —
/// everything `App` needs to gate on `thumbnail-max-mb` and build a
/// `core::thumbs::ThumbRequest`, without reaching back into this view's
/// private `entries`. Mimetype isn't included: this view has no `MimeDb`
/// in `update()` (only `view()` receives one — see `DirectoryView::view`'s
/// signature), so `App`, which owns `MimeDb`, guesses it from `location`'s
/// name instead.
#[derive(Debug, Clone)]
pub struct ThumbCandidate {
    pub location: Location,
    pub size_bytes: u64,
    pub modified: SystemTime,
}

/// Whether a `DeleteRequested` should try the trash first, or always skip
/// it. This view never itself decides *whether* a location's backend can
/// actually trash something (`Caps::TRASH` lives on the backend, resolved
/// by `App`, mirroring how this module never resolves a `Backend` for
/// anything but its own `load`/rename/create calls) — it only distinguishes
/// "the Delete key/menu row was used" from "Shift+Delete was used", which
/// is the one thing genuinely decided at the keyboard/menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Delete key, or the context menu's Delete row: trash where the
    /// backend can (`Caps::TRASH`), permanent delete worded as such
    /// otherwise.
    ToTrash,
    /// Shift+Delete: always permanent, regardless of `Caps::TRASH`.
    Permanent,
}

pub struct DirectoryView {
    location: Location,
    entries: Vec<FileEntry>,
    /// Sorted, hidden-filtered indices into `entries` — the single source
    /// of row order. Rebuilt by `recompute_visible` whenever `entries`,
    /// `sort`/`sort_descending`, or `show_hidden` changes.
    visible: Vec<usize>,
    selection: selection::Selection,
    sort: SortKey,
    sort_descending: bool,
    show_hidden: bool,
    view_mode: View,
    /// Back-stack of previously-visited locations, most recent last. See
    /// the module docs' "Per-view history" section for the full
    /// back/forward/navigate semantics.
    history: Vec<Location>,
    /// Redo stack for [`Action::HistoryForward`] — populated only by
    /// [`Self::go_back`], and cleared by [`Self::navigate`] the moment the
    /// user goes anywhere new.
    forward: Vec<Location>,
    /// `Some(buffer)` while the breadcrumb trail is swapped for an
    /// editable path/URI field (`Action::EditPath`, Ctrl+L); `None` shows
    /// the ordinary breadcrumb pills. The buffer is the field's live text,
    /// updated by `Message::PathInputChanged` on every keystroke.
    path_edit: Option<String>,
    /// Type-to-select state — see `typeahead`'s module docs.
    type_ahead: typeahead::TypeAhead,
    scroll: Option<scrollable::Viewport>,
    loading: bool,
    /// Set when the last `list()`/`open_*` call failed; `view()` renders
    /// this as a worded empty state instead of the row list — the
    /// capability-honest posture (EACCES is a normal Tuesday), never a
    /// panic or a dialog.
    error: Option<VfsError>,
    /// An entry name to select once the in-flight listing lands —
    /// `--select`'s "reveal" behavior.
    pending_select: Option<OsString>,
    /// Tracked from `Message::Keyboard`'s `ModifiersChanged`: a click
    /// message carries no modifier state of its own, so row clicks read
    /// this to tell a plain click from Ctrl/Shift+click.
    modifiers: keyboard::Modifiers,
    /// The last *plain* (unmodified) `RowClicked` — `(visible index, when)`
    /// — consulted by the next one to decide whether the pair is a
    /// double-click (see `Message::RowClicked`'s doc comment for why this
    /// lives in `update` rather than in the widget tree). Cleared by
    /// `recompute_visible` because row indices shift whenever `visible` is
    /// rebuilt (navigation, sort, watch refresh, hidden toggle) — a stale
    /// index must never pair with a fresh click on whatever row now sits
    /// at that position — and by Ctrl/Shift clicks, which are selection
    /// edits, not activation attempts.
    last_click: Option<(usize, std::time::Instant)>,
    /// Config-defined `[[action]]`s, cloned once at construction —
    /// `ui::menus`'s context menu filters/renders these by index
    /// (`Message::MenuCustomActionRequested`). Small and static for the
    /// life of the view, unlike `MimeDb`/`AppsDb` (real per-App caches —
    /// see `Self::view`'s parameters), so a per-view clone is fine.
    actions: Vec<CustomAction>,
    /// Whether the header overflow button's context menu is open —
    /// `ui::menus::overlay` reads this to decide whether to stack it over
    /// the ordinary content.
    menu_open: bool,
    /// Whether the Open-with popover is open, in place of the context
    /// menu (never both — `apply_action`-style mutual exclusion is
    /// enforced at every transition site, not by construction).
    open_with_open: bool,
    /// Inline-rename state (Stage 8) — `Some` while a row is mid-edit. See
    /// `rename`'s module docs.
    rename: Option<rename::RenameState>,
    /// Set by `create_new` (New Folder/New File) and consumed by
    /// `apply_pending_select` the moment the freshly reloaded listing
    /// contains the new entry: selecting it isn't enough on its own, this
    /// also starts inline-renaming it immediately, so creating a folder
    /// reads as "type its name" rather than "see 'New Folder', press F2,
    /// then type its name".
    pending_rename: bool,
}

impl DirectoryView {
    /// A view pointed at `location`, with `config`'s defaults, not yet
    /// loaded — call [`Self::load`] (or use [`Self::open_target`]/
    /// [`Self::open_select`]) to actually fetch its listing.
    pub fn new(location: Location, config: &Config) -> Self {
        DirectoryView {
            location,
            entries: Vec::new(),
            visible: Vec::new(),
            selection: selection::Selection::new(),
            sort: config.sort,
            sort_descending: config.sort_descending,
            show_hidden: config.show_hidden,
            view_mode: config.view,
            history: Vec::new(),
            forward: Vec::new(),
            path_edit: None,
            type_ahead: typeahead::TypeAhead::new(),
            scroll: None,
            loading: false,
            error: None,
            pending_select: None,
            modifiers: keyboard::Modifiers::empty(),
            last_click: None,
            actions: config.actions.clone(),
            menu_open: false,
            open_with_open: false,
            rename: None,
            pending_rename: false,
        }
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// The presentation mode (list vs. grid) `files.toml`/the sidebar
    /// picked for this view. Stage 3 only ever renders the list; Stage 4's
    /// grid view reads this to decide which of `list::view`/`grid::view`
    /// to call.
    pub fn view_mode(&self) -> View {
        self.view_mode
    }

    /// How many rows are currently selected — a future status bar's "N
    /// items selected" readout.
    pub fn selected_count(&self) -> usize {
        self.selection.len()
    }

    /// Whether `Action::HistoryBack`/the header's Back button would do
    /// anything right now.
    pub fn can_go_back(&self) -> bool {
        !self.history.is_empty()
    }

    /// Whether `Action::HistoryForward`/the header's Forward button would
    /// do anything right now.
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Whether dotfiles are currently shown — `ui::header`'s hidden
    /// toggle reads this to draw its on/off state.
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// The live buffer of the path/URI editor, or `None` when the
    /// breadcrumb trail (not the editor) is what's shown —
    /// `ui::breadcrumbs` reads this to decide which to render.
    pub fn path_edit(&self) -> Option<&str> {
        self.path_edit.as_deref()
    }

    /// What the backend serving this view's current location can do.
    /// `ui::header` reads this to decide whether to show the manual
    /// refresh button (only when `Caps::WATCH` is unset — a backend that
    /// can signal changes itself doesn't need one). Resolving a backend is
    /// cheap (`modules::resolve`'s docs: backends are stateless, built
    /// fresh per call) so this is a plain accessor, not something cached
    /// on the view.
    pub fn caps(&self) -> Caps {
        crate::modules::resolve(&self.location)
            .map(|backend| backend.caps())
            .unwrap_or(Caps::empty())
    }

    /// This view's live-update subscription (Stage 5) — the app batches
    /// this alongside its own keyboard subscription (`main.rs`). Identified
    /// by `self.location` (see `watch::subscription`'s docs), so iced tears
    /// down and rebuilds the underlying watch the moment `navigate`/
    /// `go_back`/`go_forward` points this view somewhere else; a backend
    /// that can't watch (`Caps::WATCH` unset) just never produces a
    /// `Message::Watch` off of it.
    pub fn subscription(&self) -> Subscription<Message> {
        watch::subscription(&self.location)
    }

    /// Whether the context menu is open — `ui::menus::overlay` reads this.
    pub fn menu_open(&self) -> bool {
        self.menu_open
    }

    /// Whether the Open-with popover is open — `ui::menus::overlay` reads
    /// this.
    pub fn open_with_open(&self) -> bool {
        self.open_with_open
    }

    /// The config-defined `[[action]]`s — `ui::menus` filters/renders
    /// these by index.
    pub fn actions(&self) -> &[CustomAction] {
        &self.actions
    }

    /// The inline-rename state, if a row is mid-edit — `list.rs`/`grid.rs`
    /// read this to swap that row's label for a `text_input`, and
    /// `ui::menus` reads it to decide whether "Rename…" should read as
    /// already-in-progress.
    pub fn rename_state(&self) -> Option<&rename::RenameState> {
        self.rename.as_ref()
    }

    /// The currently-selected entries, resolved from names back to
    /// `FileEntry`s — what the context menu/Open-with popover act on.
    /// Empty when nothing is selected (`ui::menus` falls back to
    /// "act on the current directory" for the entries that make sense
    /// without a selection, e.g. "Open in terminal").
    pub fn selected_entries(&self) -> Vec<&FileEntry> {
        self.selection
            .selected_names()
            .filter_map(|name| self.entries.iter().find(|entry| &entry.name == name))
            .collect()
    }

    /// Resolve a `visible` row index to its `FileEntry`, or `None` if it's
    /// out of range — the one indirection every row-index lookup in this
    /// module goes through, so a stale index (a race with a resort/
    /// refresh, or a message that outlived the row it named) degrades to
    /// "do nothing" rather than a panic. CLAUDE.md's no-panic rule bans
    /// indexing on any runtime path; this is the `.get()`-based
    /// replacement for `entries[visible[i]]`.
    fn entry_at(&self, visible_index: usize) -> Option<&FileEntry> {
        self.visible
            .get(visible_index)
            .and_then(|&entry_index| self.entries.get(entry_index))
    }

    /// Open `location` directly and kick off its listing — the plain
    /// "no CLI target, no `--select`" startup path, and the general
    /// "build a view pointed here" constructor `explorer`/future-tabs
    /// code can reuse.
    pub fn open(location: Location, config: &Config) -> (Self, Task<Message>) {
        let mut view = Self::new(location, config);
        let task = view.load();
        (view, task)
    }

    /// Open a CLI positional target (`saola-files PATH`). `PATH` may name
    /// either a directory (browse it) or a file (reveal it in its
    /// parent) — telling those apart means asking the backend, never
    /// `std::fs` directly (CLAUDE.md: all file access goes through a
    /// `Backend`), so this kicks off an async probe rather than deciding
    /// synchronously. `fallback` is used if the probe errors (e.g. the
    /// path doesn't exist).
    pub fn open_target(
        target: OsString,
        fallback: Location,
        config: &Config,
    ) -> (Self, Task<Message>) {
        let probed = Location::local(PathBuf::from(&target));
        let view = Self::new(probed.clone(), config);

        let task = Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&probed) else {
                    let listing = list_with_fallback(&fallback).await;
                    return (fallback, None, listing);
                };
                match backend.metadata(&probed).await {
                    Ok(entry) if entry.kind == EntryKind::Directory => {
                        let listing = backend.list(&probed).await;
                        (probed, None, listing)
                    }
                    Ok(entry) => {
                        let parent = probed.parent().unwrap_or_else(|| probed.clone());
                        let listing = backend.list(&parent).await;
                        (parent, Some(entry.name), listing)
                    }
                    Err(_) => {
                        let listing = backend.list(&fallback).await;
                        (fallback, None, listing)
                    }
                }
            },
            |(location, select, result)| Message::TargetResolved(location, select, result),
        );

        (view, task)
    }

    /// Open `--select PATH`: always reveal PATH's parent with PATH
    /// selected, whether PATH itself is a file or a directory (per
    /// `cli.rs`'s grammar) — no dir-vs-file probe needed, unlike
    /// [`Self::open_target`].
    pub fn open_select(
        select: PathBuf,
        fallback: Location,
        config: &Config,
    ) -> (Self, Task<Message>) {
        let (location, pending_select) = match (select.parent(), select.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => (
                Location::local(parent.to_path_buf()),
                Some(name.to_os_string()),
            ),
            _ => (fallback, None),
        };
        let mut view = Self::new(location, config);
        view.pending_select = pending_select;
        let task = view.load();
        (view, task)
    }

    /// The owner's response to `Event::OpenDirectory`: point this view at
    /// `location`, clear per-directory state, and kick off the listing.
    /// Never called from within `update()` itself — see the module docs.
    pub fn navigate(&mut self, location: Location) -> Task<Message> {
        self.history.push(self.location.clone());
        // Visiting anywhere new invalidates the redo stack — same as a
        // browser: you can't "forward" into a page you reached by
        // following a fresh link instead of going back from it.
        self.forward.clear();
        self.location = location;
        self.selection.clear();
        self.pending_select = None;
        self.error = None;
        self.path_edit = None;
        self.type_ahead.clear();
        self.menu_open = false;
        self.open_with_open = false;
        self.rename = None;
        self.pending_rename = false;
        self.load()
    }

    /// `Action::HistoryBack`: pop the most recent `back` entry, push the
    /// current location onto `forward` so `go_forward` can return to it,
    /// and load the popped location. A no-op `Task::none()` at the start
    /// of history (`history` empty) rather than anything the caller has
    /// to check first — matches every other guarded-index method in this
    /// file (CLAUDE.md's no-panic rule).
    fn go_back(&mut self) -> Task<Message> {
        let Some(target) = self.history.pop() else {
            return Task::none();
        };
        self.forward.push(self.location.clone());
        self.enter(target)
    }

    /// The redo side of [`Self::go_back`].
    fn go_forward(&mut self) -> Task<Message> {
        let Some(target) = self.forward.pop() else {
            return Task::none();
        };
        self.history.push(self.location.clone());
        self.enter(target)
    }

    /// Shared tail of `go_back`/`go_forward`: point this view at `target`
    /// and reload, without touching either history stack (the caller
    /// already did that) — unlike `navigate`, which is the "went somewhere
    /// new" path and always clears `forward`.
    fn enter(&mut self, target: Location) -> Task<Message> {
        self.location = target;
        self.selection.clear();
        self.pending_select = None;
        self.error = None;
        self.path_edit = None;
        self.type_ahead.clear();
        self.menu_open = false;
        self.open_with_open = false;
        self.rename = None;
        self.pending_rename = false;
        self.load()
    }

    /// Fetch `self.location`'s listing.
    fn load(&mut self) -> Task<Message> {
        self.loading = true;
        let location_for_task = self.location.clone();
        let location_for_message = self.location.clone();
        Task::perform(
            async move {
                let backend = crate::modules::resolve(&location_for_task);
                match backend {
                    Some(backend) => backend.list(&location_for_task).await,
                    None => Err(VfsError::Other {
                        message: format!("no backend for scheme \"{}\"", location_for_task.scheme),
                    }),
                }
            },
            move |result| Message::Listed(location_for_message.clone(), result),
        )
    }

    /// Applies one debounced batch of watch events (Stage 5) — see the
    /// module docs' "Live updates" section for the split between what
    /// happens synchronously here vs. in `Message::WatchRefreshed`.
    fn apply_watch_events(&mut self, events: Vec<DirEvent>) -> Task<Message> {
        // Any `Overflow` in the batch means the backend can no longer
        // promise it reported every change (see `DirEvent::Overflow`'s
        // docs) — the rest of the batch is superseded by a full re-list,
        // not worth applying first.
        if events
            .iter()
            .any(|event| matches!(event, DirEvent::Overflow))
        {
            return self.load();
        }

        // Deduplicated: a rapid create-then-modify (e.g. `touch`, or a
        // download landing) can produce two events for the same name
        // within one batch, and there's no reason to fetch its metadata
        // twice.
        let mut to_fetch: std::collections::HashSet<OsString> = std::collections::HashSet::new();
        for event in events {
            match event {
                DirEvent::Created(name) | DirEvent::Changed(name) => {
                    to_fetch.insert(name);
                }
                DirEvent::Removed(name) => {
                    self.remove_entry_by_name(&name);
                    self.selection.forget(&name);
                }
                DirEvent::Renamed { from, to } => {
                    self.selection.rename(&from, to.clone());
                    self.remove_entry_by_name(&from);
                    to_fetch.insert(to);
                }
                // Already handled by the early return above, which scans
                // the same `events` this loop was built from — a no-op
                // here rather than `unreachable!()`, per CLAUDE.md's
                // no-panic rule: if that invariant is ever wrong, silently
                // skipping one event is a far better failure mode than
                // taking the app down.
                DirEvent::Overflow => {}
            }
        }
        // One resort for every synchronous removal/rename in the batch,
        // not one per event — the "debounced re-sort" the stage calls
        // for. `Message::WatchRefreshed` below does the second (and last)
        // one, for whatever this call kicks off fetching.
        self.recompute_visible();

        if to_fetch.is_empty() {
            return Task::none();
        }

        let location = self.location.clone();
        let message_location = self.location.clone();
        Task::perform(
            async move {
                let backend = crate::modules::resolve(&location);
                let mut results = Vec::with_capacity(to_fetch.len());
                for name in to_fetch {
                    let entry = match &backend {
                        Some(backend) => backend.metadata(&location.join(&name)).await.ok(),
                        None => None,
                    };
                    results.push((name, entry));
                }
                results
            },
            move |results| Message::WatchRefreshed(message_location.clone(), results),
        )
    }

    /// Removes `name` from `entries` if present — the synchronous half of
    /// applying a `DirEvent::Removed`/rename's "from" half. A name that
    /// isn't there (already removed, or never listed) is a no-op, not an
    /// error: watch events and the in-flight listing they're layered on
    /// top of can race harmlessly.
    fn remove_entry_by_name(&mut self, name: &OsStr) {
        self.entries.retain(|entry| entry.name.as_os_str() != name);
    }

    /// Inserts `entry`, or overwrites the existing row with the same name
    /// — the shared tail of applying a `DirEvent::Created`/`Changed`/
    /// rename's "to" half once its metadata has come back.
    fn upsert_entry(&mut self, entry: FileEntry) {
        match self
            .entries
            .iter_mut()
            .find(|existing| existing.name == entry.name)
        {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Event>) {
        match message {
            Message::Listed(location, result) => {
                if location != self.location {
                    // A later navigation raced ahead of this response.
                    return (Task::none(), None);
                }
                self.loading = false;
                match result {
                    Ok(entries) => {
                        self.entries = entries;
                        self.recompute_visible();
                        let task = self.apply_pending_select();
                        (task, self.thumbnail_event())
                    }
                    Err(err) => {
                        self.entries.clear();
                        self.visible.clear();
                        self.error = Some(err);
                        (Task::none(), None)
                    }
                }
            }
            Message::TargetResolved(location, select, result) => {
                self.location = location.clone();
                self.pending_select = select;
                self.update(Message::Listed(location, result))
            }
            Message::RowClicked(index) => {
                let Some(name) = self.entry_at(index).map(|entry| entry.name.clone()) else {
                    return (Task::none(), None);
                };
                if self.modifiers.control() {
                    self.last_click = None;
                    self.selection.toggle_click(index, name);
                    (Task::none(), None)
                } else if self.modifiers.shift() {
                    self.last_click = None;
                    self.extend_to(index);
                    (Task::none(), None)
                } else {
                    // The pairing *decision* is the pure `is_double_click`
                    // (unit-tested with synthetic instants); this arm is
                    // the thin wrapper that supplies the real clock — the
                    // same split CLAUDE.md prescribes for env lookups.
                    let now = std::time::Instant::now();
                    let is_double = is_double_click(self.last_click, index, now);
                    self.selection.click(index, name);
                    if is_double {
                        // Consumed: a third quick click starts a fresh
                        // pair rather than chaining activations.
                        self.last_click = None;
                        (Task::none(), self.activate_selection())
                    } else {
                        self.last_click = Some((index, now));
                        (Task::none(), None)
                    }
                }
            }
            Message::HeaderClicked(key) => {
                if self.sort == key {
                    self.sort_descending = !self.sort_descending;
                } else {
                    self.sort = key;
                    self.sort_descending = false;
                }
                self.recompute_visible();
                (Task::none(), self.thumbnail_event())
            }
            Message::Scrolled(viewport) => {
                self.scroll = Some(viewport);
                (Task::none(), self.thumbnail_event())
            }
            Message::Keyboard(event) => self.handle_keyboard(event),
            Message::Action(action) => self.apply_action(action),
            Message::BreadcrumbClicked(location) => {
                self.path_edit = None;
                (Task::none(), Some(Event::OpenDirectory(location)))
            }
            Message::PathInputChanged(value) => {
                self.path_edit = Some(value);
                (Task::none(), None)
            }
            Message::PathSubmitted => {
                let Some(buffer) = self.path_edit.take() else {
                    return (Task::none(), None);
                };
                let trimmed = buffer.trim();
                if trimmed.is_empty() {
                    return (Task::none(), None);
                }
                (
                    Task::none(),
                    Some(Event::OpenDirectory(parse_typed_location(trimmed))),
                )
            }
            Message::Watch(events) => (self.apply_watch_events(events), None),
            Message::WatchRefreshed(location, results) => {
                if location != self.location {
                    // A later navigation raced ahead of this fetch.
                    return (Task::none(), None);
                }
                for (name, entry) in results {
                    match entry {
                        Some(entry) => self.upsert_entry(entry),
                        // Vanished again before the fetch completed (a
                        // create-then-immediate-delete race) — nothing to
                        // insert, and any watched `Removed` for it either
                        // already ran or is still in flight and will.
                        None => self.remove_entry_by_name(&name),
                    }
                }
                self.recompute_visible();
                (Task::none(), self.thumbnail_event())
            }

            // ── Stage 6: context menu / Open-with popover ───────────────
            Message::OpenMenu => {
                self.menu_open = true;
                self.open_with_open = false;
                (Task::none(), None)
            }
            Message::CloseMenu => {
                self.menu_open = false;
                self.open_with_open = false;
                (Task::none(), None)
            }
            Message::MenuOpenSelected => {
                self.menu_open = false;
                (Task::none(), self.activate_selection())
            }
            Message::MenuOpenWithRequested => {
                self.menu_open = false;
                self.open_with_open = true;
                (Task::none(), None)
            }
            Message::MenuOpenTerminalRequested => {
                self.menu_open = false;
                (Task::none(), Some(self.terminal_target()))
            }
            Message::MenuCustomActionRequested(index) => {
                self.menu_open = false;
                let Some(action) = self.actions.get(index) else {
                    return (Task::none(), None);
                };
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (
                    Task::none(),
                    Some(Event::RunCustomAction(action.exec.clone(), targets)),
                )
            }
            Message::OpenWithChosen(desktop_id) => {
                self.open_with_open = false;
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (Task::none(), Some(Event::OpenWith(targets, desktop_id)))
            }

            // ── Stage 8: clipboard / rename / new folder / new file ─────
            Message::MenuCopyRequested => {
                self.menu_open = false;
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (Task::none(), Some(Event::CopyRequested(targets)))
            }
            Message::MenuCutRequested => {
                self.menu_open = false;
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (Task::none(), Some(Event::CutRequested(targets)))
            }
            Message::MenuPasteRequested => {
                self.menu_open = false;
                (
                    Task::none(),
                    Some(Event::PasteRequested(self.location.clone())),
                )
            }
            Message::MenuRenameRequested => {
                self.menu_open = false;
                let selected = self.selected_entries();
                if let [entry] = selected.as_slice() {
                    let name = entry.name.clone();
                    (self.start_rename_named(name), None)
                } else {
                    (Task::none(), None)
                }
            }
            Message::MenuNewFolderRequested => {
                self.menu_open = false;
                (self.create_new(NewKind::Folder), None)
            }
            Message::MenuNewFileRequested => {
                self.menu_open = false;
                (self.create_new(NewKind::File), None)
            }
            Message::MenuDeleteRequested => {
                self.menu_open = false;
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (
                    Task::none(),
                    Some(Event::DeleteRequested(targets, DeleteMode::ToTrash)),
                )
            }
            Message::RenameChanged(value) => {
                if let Some(state) = self.rename.as_mut() {
                    state.buffer = value;
                    state.error = None;
                }
                (Task::none(), None)
            }
            Message::RenameSubmitted => (self.submit_rename(), None),
            Message::RenameCancelled => {
                self.rename = None;
                (Task::none(), None)
            }
            Message::RenameResult(dir_location, old_name, result) => {
                if dir_location != self.location {
                    // A later navigation raced ahead of this response.
                    return (Task::none(), None);
                }
                match result {
                    Ok(()) => {
                        if let Some(state) = self.rename.take() {
                            let new_name = OsString::from(state.buffer.trim());
                            let from = dir_location.join(&old_name);
                            let to = dir_location.join(&new_name);
                            self.rename_entry_by_name(&old_name, new_name.clone());
                            self.selection.rename(&old_name, new_name);
                            self.recompute_visible();
                            (Task::none(), Some(Event::Renamed(from, to)))
                        } else {
                            (Task::none(), None)
                        }
                    }
                    Err(err) => {
                        if let Some(state) = self.rename.as_mut() {
                            state.error = Some(err.to_string());
                        }
                        (Task::none(), None)
                    }
                }
            }
            Message::CreateResult(dir_location, name, result) => {
                if dir_location != self.location {
                    return (Task::none(), None);
                }
                match result {
                    Ok(()) => {
                        let created = dir_location.join(&name);
                        self.pending_select = Some(name);
                        self.pending_rename = true;
                        (self.load(), Some(Event::Created(created)))
                    }
                    Err(err) => {
                        eprintln!(
                            "saola-files: could not create {name:?} in {dir_location}: {err}"
                        );
                        (Task::none(), None)
                    }
                }
            }

            // ── Stage 13: properties ─────────────────────────────────────
            Message::MenuPropertiesRequested => {
                self.menu_open = false;
                (Task::none(), self.properties_event())
            }
        }
    }

    pub fn view<'a>(
        &'a self,
        theme: &'a Theme,
        mime_db: &'a MimeDb,
        thumb_cache: &'a ThumbCache,
        apps_db: &'a AppsDb,
        clipboard_has_contents: bool,
    ) -> Element<'a, Message> {
        let content = match self.view_mode {
            View::List => list::view(self, theme, mime_db, thumb_cache),
            View::Grid => grid::view(self, theme, mime_db, thumb_cache),
        };
        menus::overlay(
            theme,
            self,
            mime_db,
            apps_db,
            clipboard_has_contents,
            content,
        )
    }

    // ── keyboard/action handling ────────────────────────────────────────

    fn handle_keyboard(&mut self, event: keyboard::Event) -> (Task<Message>, Option<Event>) {
        match event {
            keyboard::Event::KeyPressed { key, modifiers, .. } => {
                // While the context menu/Open-with popover is open, the
                // global keyboard subscription must not also drive row
                // cursor/selection actions underneath it — same posture as
                // the path/URI editor guard just below, and for the same
                // reason (one input surface owns the keyboard at a time).
                // Escape is the one thing this guard itself handles.
                if self.menu_open || self.open_with_open {
                    if matches!(
                        key,
                        iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
                    ) && modifiers.is_empty()
                    {
                        self.menu_open = false;
                        self.open_with_open = false;
                    }
                    return (Task::none(), None);
                }
                // While a row is being inline-renamed, same posture as the
                // path/URI editor guard just below: the global keyboard
                // subscription must not also drive row cursor/selection
                // actions from keystrokes the rename `text_input` is
                // already consuming. Escape cancels; everything else is
                // the text_input's job via `Message::RenameChanged`/
                // `RenameSubmitted`.
                if self.rename.is_some() {
                    if matches!(
                        key,
                        iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
                    ) && modifiers.is_empty()
                    {
                        self.rename = None;
                    }
                    return (Task::none(), None);
                }
                // While the path/URI editor is open, the global keyboard
                // subscription must not *also* drive row cursor/selection
                // actions from the same keystrokes the text_input is
                // already consuming for typing (arrow keys, Ctrl+A, ...).
                // Escape is the one thing this module still owns while
                // editing; everything else is the text_input's job via
                // `Message::PathInputChanged`/`PathSubmitted`.
                if self.path_edit.is_some() {
                    if matches!(
                        key,
                        iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
                    ) && modifiers.is_empty()
                    {
                        self.path_edit = None;
                    }
                    return (Task::none(), None);
                }
                match keymap::resolve(&key, modifiers) {
                    Some(action) => self.apply_action(action),
                    None => {
                        self.try_type_ahead(&key, modifiers);
                        (Task::none(), None)
                    }
                }
            }
            keyboard::Event::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                (Task::none(), None)
            }
            keyboard::Event::KeyReleased { .. } => (Task::none(), None),
        }
    }

    /// A key `keymap::resolve` doesn't own: if it's an unmodified printable
    /// character, feed it to type-ahead and jump the cursor to whatever it
    /// matches. Ctrl/Alt/Logo combinations are left alone here (they're
    /// either already an `Action`, or genuinely not this module's
    /// business — e.g. a future global shortcut).
    fn try_type_ahead(&mut self, key: &iced::keyboard::Key, modifiers: keyboard::Modifiers) {
        if modifiers.control() || modifiers.alt() || modifiers.logo() {
            return;
        }
        let iced::keyboard::Key::Character(text) = key else {
            return;
        };
        let Some(ch) = text.as_str().chars().next() else {
            return;
        };
        if !ch.is_alphanumeric() {
            return;
        }

        let names = self
            .visible
            .iter()
            .filter_map(|&i| self.entries.get(i).map(|entry| entry.name.as_os_str()));
        let Some(index) = self.type_ahead.feed(ch, std::time::Instant::now(), names) else {
            return;
        };
        if let Some(name) = self.entry_at(index).map(|entry| entry.name.clone()) {
            self.selection.click(index, name);
        }
    }

    fn apply_action(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
        match action {
            Action::MoveCursorUp => {
                self.move_cursor(-self.row_step());
                (Task::none(), None)
            }
            Action::MoveCursorDown => {
                self.move_cursor(self.row_step());
                (Task::none(), None)
            }
            // A no-op in list view (there is no "column"); grid view steps
            // one tile left/right within the current row.
            Action::MoveCursorLeft => {
                if self.view_mode == View::Grid {
                    self.move_cursor(-1);
                }
                (Task::none(), None)
            }
            Action::MoveCursorRight => {
                if self.view_mode == View::Grid {
                    self.move_cursor(1);
                }
                (Task::none(), None)
            }
            Action::MoveCursorHome => {
                self.move_cursor_to(0);
                (Task::none(), None)
            }
            Action::MoveCursorEnd => {
                self.move_cursor_to(self.visible.len().saturating_sub(1));
                (Task::none(), None)
            }
            Action::MoveCursorPageUp => {
                self.move_cursor(-PAGE_ROWS * self.row_step());
                (Task::none(), None)
            }
            Action::MoveCursorPageDown => {
                self.move_cursor(PAGE_ROWS * self.row_step());
                (Task::none(), None)
            }
            Action::ExtendSelectionUp => {
                self.extend_cursor(-self.row_step());
                (Task::none(), None)
            }
            Action::ExtendSelectionDown => {
                self.extend_cursor(self.row_step());
                (Task::none(), None)
            }
            Action::ExtendSelectionLeft => {
                if self.view_mode == View::Grid {
                    self.extend_cursor(-1);
                }
                (Task::none(), None)
            }
            Action::ExtendSelectionRight => {
                if self.view_mode == View::Grid {
                    self.extend_cursor(1);
                }
                (Task::none(), None)
            }
            Action::ExtendSelectionHome => {
                self.extend_to(0);
                (Task::none(), None)
            }
            Action::ExtendSelectionEnd => {
                self.extend_to(self.visible.len().saturating_sub(1));
                (Task::none(), None)
            }
            Action::ExtendSelectionPageUp => {
                self.extend_cursor(-PAGE_ROWS * self.row_step());
                (Task::none(), None)
            }
            Action::ExtendSelectionPageDown => {
                self.extend_cursor(PAGE_ROWS * self.row_step());
                (Task::none(), None)
            }
            Action::ToggleCursorSelected => {
                if let Some(cursor) = self.selection.cursor()
                    && let Some(name) = self.entry_at(cursor).map(|entry| entry.name.clone())
                {
                    self.selection.toggle_click(cursor, name);
                }
                (Task::none(), None)
            }
            Action::SelectAll => {
                let names = self
                    .visible
                    .iter()
                    .filter_map(|&i| self.entries.get(i).map(|entry| entry.name.clone()))
                    .collect::<Vec<_>>();
                self.selection.select_all(names);
                (Task::none(), None)
            }
            Action::Descend => (Task::none(), self.activate_selection()),
            Action::Ascend => (
                Task::none(),
                self.location.parent().map(Event::OpenDirectory),
            ),
            Action::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.recompute_visible();
                (Task::none(), self.thumbnail_event())
            }
            Action::HistoryBack => (self.go_back(), None),
            Action::HistoryForward => (self.go_forward(), None),
            Action::Refresh => (self.load(), None),
            Action::SetViewList => {
                self.view_mode = View::List;
                (Task::none(), None)
            }
            Action::SetViewGrid => {
                self.view_mode = View::Grid;
                (Task::none(), None)
            }
            Action::EditPath => {
                self.path_edit = Some(self.location.to_string());
                self.type_ahead.clear();
                (
                    Task::batch([
                        iced::widget::operation::focus(breadcrumbs::PATH_INPUT_ID),
                        iced::widget::operation::select_all(breadcrumbs::PATH_INPUT_ID),
                    ]),
                    None,
                )
            }

            // ── Stage 8: clipboard / rename / new folder ─────────────────
            Action::Copy => {
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (Task::none(), Some(Event::CopyRequested(targets)))
            }
            Action::Cut => {
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (Task::none(), Some(Event::CutRequested(targets)))
            }
            Action::Paste => (
                Task::none(),
                Some(Event::PasteRequested(self.location.clone())),
            ),
            Action::Rename => {
                let selected = self.selected_entries();
                if let [entry] = selected.as_slice() {
                    let name = entry.name.clone();
                    (self.start_rename_named(name), None)
                } else {
                    (Task::none(), None)
                }
            }
            Action::NewFolder => (self.create_new(NewKind::Folder), None),

            // ── Stage 9: trash / permanent delete ─────────────────────────
            Action::Delete => {
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (
                    Task::none(),
                    Some(Event::DeleteRequested(targets, DeleteMode::ToTrash)),
                )
            }
            Action::PermanentDelete => {
                let targets = self.selection_targets();
                if targets.is_empty() {
                    return (Task::none(), None);
                }
                (
                    Task::none(),
                    Some(Event::DeleteRequested(targets, DeleteMode::Permanent)),
                )
            }

            // ── Stage 10: undo ────────────────────────────────────────────
            Action::Undo => (Task::none(), Some(Event::UndoRequested)),

            // ── Stage 13: properties ───────────────────────────────────────
            Action::Properties => (Task::none(), self.properties_event()),
        }
    }

    /// How many `visible` positions one Up/Down/PageUp/PageDown step
    /// covers: one item in list view, one full tile-row in grid view — so
    /// "Down" always means "the next thing spatially below", not "the next
    /// name alphabetically", regardless of presentation. Left/Right
    /// (grid-only) always step by exactly one, independent of this.
    ///
    /// The grid arm asks `grid::columns_for_scroll` rather than reading a
    /// constant, so a resized window moves the cursor by however many tiles
    /// are *actually* on a row now. `update()` can't see the `responsive`
    /// closure's measured `Size` that `grid::view` uses, so that helper
    /// takes the width from `self.scroll` instead — see its doc comment for
    /// the one-frame staleness that buys and why it's harmless.
    fn row_step(&self) -> isize {
        match self.view_mode {
            View::List => 1,
            View::Grid => grid::columns_for_scroll(self.scroll) as isize,
        }
    }

    // ── cursor/selection helpers ────────────────────────────────────────

    fn clamp_index(&self, index: isize) -> Option<usize> {
        if self.visible.is_empty() {
            return None;
        }
        let max = self.visible.len() as isize - 1;
        Some(index.clamp(0, max) as usize)
    }

    /// The cursor's current position for movement math, as if it sat one
    /// *step* before the first when nothing is selected/cursored yet — not
    /// `0`, which would make the very first `MoveCursorDown` land one step
    /// past row 0 (skipping it) instead of landing on it.
    ///
    /// Parametrized by `step` (the magnitude of the delta about to be
    /// applied) rather than hardcoded to `-1`: in list view a "step" is
    /// one item, so the sentinel `-1` plus a `+1` delta lands on `0` — the
    /// original Stage 3 behavior. In grid view a vertical "step" is a
    /// whole `row_step()`-sized row; using the same `-1` sentinel there
    /// would land the very first `MoveCursorDown` on row *index*
    /// `row_step() - 1` instead of `0` (verified against
    /// `grid_view_steps_the_cursor_by_a_full_row`'s first assertion,
    /// which is what caught this). Using `-step` generalizes both: the
    /// first step in *either* direction always resolves to `0` (a
    /// negative starting point clamps there regardless of which way it
    /// was pushed).
    fn current_cursor_or_before_start(&self, step: isize) -> isize {
        self.selection.cursor().map_or(-step, |c| c as isize)
    }

    fn move_cursor(&mut self, delta: isize) {
        self.move_cursor_to_signed(self.current_cursor_or_before_start(delta.abs()) + delta);
    }

    fn move_cursor_to(&mut self, index: usize) {
        self.move_cursor_to_signed(index as isize);
    }

    fn move_cursor_to_signed(&mut self, index: isize) {
        let Some(target) = self.clamp_index(index) else {
            return;
        };
        let Some(name) = self.entry_at(target).map(|entry| entry.name.clone()) else {
            return;
        };
        self.selection.click(target, name);
    }

    fn extend_cursor(&mut self, delta: isize) {
        self.extend_to_signed(self.current_cursor_or_before_start(delta.abs()) + delta);
    }

    fn extend_to(&mut self, index: usize) {
        self.extend_to_signed(index as isize);
    }

    fn extend_to_signed(&mut self, index: isize) {
        let Some(target) = self.clamp_index(index) else {
            return;
        };
        let anchor = self
            .selection
            .anchor()
            .or(self.selection.cursor())
            .unwrap_or(target);
        let (lo, hi) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        let names = (lo..=hi)
            .filter_map(|i| self.entry_at(i).map(|entry| entry.name.clone()))
            .collect::<Vec<_>>();
        self.selection.range_select(target, names);
    }

    /// The rows Enter/double-click should act on: the current selection,
    /// falling back to the cursor row alone if nothing's selected. A lone
    /// selected directory descends into it; anything else (files, or a
    /// mixed/multi selection) activates the whole set.
    fn activate_selection(&self) -> Option<Event> {
        // `Selection` is name-keyed, so resolving it back to `FileEntry`s
        // means a lookup per selected name (typically a handful of rows,
        // never the full directory) rather than a `visible`-order scan.
        let entries: Vec<&FileEntry> = if self.selection.is_empty() {
            self.selection
                .cursor()
                .and_then(|cursor| self.entry_at(cursor))
                .into_iter()
                .collect()
        } else {
            self.selection
                .selected_names()
                .filter_map(|name| self.entries.iter().find(|entry| &entry.name == name))
                .collect()
        };

        let (first, rest) = entries.split_first()?;
        if rest.is_empty() && first.kind == EntryKind::Directory {
            return Some(Event::OpenDirectory(self.location.join(&first.name)));
        }
        let locations = entries
            .iter()
            .map(|entry| self.location.join(&entry.name))
            .collect();
        Some(Event::Activated(locations))
    }

    /// The current selection's locations, in whatever order `Selection`
    /// happens to iterate them — the targets for a context-menu action
    /// (`RunCustomAction`, `OpenWith`). Empty when nothing is selected.
    fn selection_targets(&self) -> Vec<Location> {
        self.selection
            .selected_names()
            .map(|name| self.location.join(name))
            .collect()
    }

    /// `Event::PropertiesRequested`'s payload for the current selection —
    /// shared by `Action::Properties` (Alt+Enter) and
    /// `Message::MenuPropertiesRequested` (the context menu row), the same
    /// "one behavior, two input paths" split every other action/menu-row
    /// pair in this module already takes. `None` when nothing is selected
    /// — there is nothing to show properties for.
    fn properties_event(&self) -> Option<Event> {
        let selected = self.selected_entries();
        if selected.is_empty() {
            return None;
        }
        let items = selected
            .iter()
            .map(|entry| (self.location.join(&entry.name), (*entry).clone()))
            .collect();
        Some(Event::PropertiesRequested(items))
    }

    /// `Event::OpenTerminal`'s target for "Open in terminal": exactly one
    /// selected entry targets that entry itself (a directory opens *in*
    /// itself, a file opens in its parent — CLAUDE.md's stated behavior);
    /// zero or multiple selected entries fall back to the current
    /// directory, which has no such ambiguity.
    fn terminal_target(&self) -> Event {
        let selected = self.selected_entries();
        if let [entry] = selected.as_slice() {
            let is_dir = entry.kind == EntryKind::Directory;
            return Event::OpenTerminal(self.location.join(&entry.name), is_dir);
        }
        Event::OpenTerminal(self.location.clone(), true)
    }

    // ── Stage 8: rename / new folder / new file helpers ─────────────────

    /// Starts inline-renaming `name`, if nothing is already being renamed
    /// (defensive — no known call site can currently trigger this while
    /// `self.rename` is already `Some`, but "start a second rename mid-edit"
    /// has no sane meaning, so this degrades to a no-op rather than
    /// clobbering the in-flight one). Returns the focus/select-all task the
    /// same way `Action::EditPath` does for the breadcrumb editor.
    fn start_rename_named(&mut self, name: OsString) -> Task<Message> {
        if self.rename.is_some() {
            return Task::none();
        }
        self.rename = Some(rename::RenameState::new(name));
        self.menu_open = false;
        self.open_with_open = false;
        Task::batch([
            iced::widget::operation::focus(rename::RENAME_INPUT_ID),
            iced::widget::operation::select_all(rename::RENAME_INPUT_ID),
        ])
    }

    /// `Message::RenameSubmitted`'s handling: validates the typed name,
    /// and — if it's both non-empty and actually different from the
    /// original — kicks off the `Backend::rename` call. An empty/`.`/`..`/
    /// path-separator-containing name re-opens the field with an inline
    /// error instead of ever reaching the backend (those would either be
    /// silently meaningless or, for `/`, attempt to rename into a
    /// different directory entirely, which "Rename" must never do —
    /// that's what cut+paste is for). Typing the same name back commits to
    /// a silent no-op, matching every mainstream file manager.
    fn submit_rename(&mut self) -> Task<Message> {
        let Some(mut state) = self.rename.take() else {
            return Task::none();
        };
        let trimmed = state.buffer.trim().to_owned();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.contains('/') {
            state.error = Some("Enter a valid name".to_owned());
            self.rename = Some(state);
            return Task::none();
        }
        if trimmed == state.original.to_string_lossy() {
            return Task::none();
        }

        let from = self.location.join(&state.original);
        let to = self.location.join(&trimmed);
        let dir_location = self.location.clone();
        let old_name = state.original.clone();
        // Kept `Some` (not cleared) while the async call is in flight, so
        // the row stays in edit mode showing what was just submitted —
        // `Message::RenameResult` is what finally clears it, on success.
        self.rename = Some(state);

        Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&from) else {
                    return Err(VfsError::Other {
                        message: format!("no backend for scheme \"{}\"", from.scheme),
                    });
                };
                backend.rename(&from, &to).await
            },
            move |result| Message::RenameResult(dir_location.clone(), old_name.clone(), result),
        )
    }

    /// Renames `old_name` to `new_name` in `entries` in place, if present
    /// — the optimistic local half of a successful `Message::RenameResult`
    /// (no re-list round trip needed; a later watch event reconciling the
    /// same change, if the backend emits one, is a harmless no-op layered
    /// on top).
    fn rename_entry_by_name(&mut self, old_name: &OsStr, new_name: OsString) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.name.as_os_str() == old_name)
        {
            entry.name = new_name;
        }
    }

    /// `Action::NewFolder`/the context menu's New Folder/New File: picks an
    /// unclaimed name (`rename::unique_name`) and kicks off the
    /// `Backend::mkdir`/`write` call. `Message::CreateResult` is what
    /// reloads the listing and starts renaming the new entry on success.
    fn create_new(&mut self, kind: NewKind) -> Task<Message> {
        let base = match kind {
            NewKind::Folder => "New Folder",
            NewKind::File => "New File",
        };
        let name = rename::unique_name(&self.entries, base);
        let location = self.location.join(&name);
        let dir_location = self.location.clone();
        let name_for_message = name.clone();

        Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&location) else {
                    return Err(VfsError::Other {
                        message: format!("no backend for scheme \"{}\"", location.scheme),
                    });
                };
                match kind {
                    NewKind::Folder => backend.mkdir(&location).await,
                    NewKind::File => {
                        // An empty file: open the write sink and close it
                        // immediately without ever sending a chunk —
                        // `Backend::write` already creates/truncates on
                        // open (see `modules::local::write`'s
                        // `fs::File::create`), so a bare open-then-close is
                        // a genuine empty-file creation, not a partial one.
                        let mut sink = backend.write(&location).await?;
                        sink.close().await.map_err(|_| VfsError::Other {
                            message: format!("creating {location} failed"),
                        })
                    }
                }
            },
            move |result| {
                Message::CreateResult(dir_location.clone(), name_for_message.clone(), result)
            },
        )
    }

    /// If `pending_select` names an entry in the just-loaded listing,
    /// select it and put the cursor there; otherwise leave the selection
    /// as-is (a `--select` target that vanished before we got here is not
    /// worth an error). When `pending_rename` is also set (Stage 8's New
    /// Folder/New File flow), additionally starts inline-renaming that
    /// same entry — see `pending_rename`'s doc comment.
    fn apply_pending_select(&mut self) -> Task<Message> {
        let Some(name) = self.pending_select.take() else {
            self.pending_rename = false;
            return Task::none();
        };
        let found = self
            .visible
            .iter()
            .position(|&i| self.entries.get(i).is_some_and(|entry| entry.name == name));
        let task = match found {
            Some(index) => {
                self.selection.click(index, name.clone());
                if self.pending_rename {
                    self.start_rename_named(name)
                } else {
                    Task::none()
                }
            }
            None => Task::none(),
        };
        self.pending_rename = false;
        task
    }

    /// Rebuild `visible` from `entries` (hidden-filtered, sorted) and try
    /// to keep the keyboard cursor on the same *entry* it was on before —
    /// falling back to clamping its raw index. A view that never had a
    /// cursor stays without one (`None`) rather than inventing row 0 —
    /// the very first arrow-key press does that (see
    /// `current_cursor_or_before_start`), the same way a fresh view with
    /// no clicks yet has nothing selected.
    fn recompute_visible(&mut self) {
        // Row indices are about to shift — a recorded half-of-a-double
        // -click must not pair with a fresh click on whatever entry now
        // occupies that index (see `last_click`'s doc comment).
        self.last_click = None;
        let had_cursor = self.selection.cursor().is_some();
        let previous_cursor_name = self
            .selection
            .cursor()
            .and_then(|c| self.entry_at(c))
            .map(|entry| entry.name.clone());

        let mut visible: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.show_hidden || !is_hidden(entry))
            .map(|(i, _)| i)
            .collect();
        let (key, descending) = (self.sort, self.sort_descending);
        visible.sort_by(|&a, &b| match (self.entries.get(a), self.entries.get(b)) {
            (Some(entry_a), Some(entry_b)) => sort::compare(entry_a, entry_b, key, descending),
            // Unreachable in practice — `a`/`b` are always indices this
            // same `entries` produced via `enumerate()` above — but a
            // missing entry degrades to "leave them where they were"
            // rather than a panic.
            _ => std::cmp::Ordering::Equal,
        });
        self.visible = visible;

        let new_cursor = previous_cursor_name
            .and_then(|name| {
                self.visible
                    .iter()
                    .position(|&i| self.entries.get(i).is_some_and(|entry| entry.name == name))
            })
            .or(if had_cursor && !self.visible.is_empty() {
                // Had a cursor, but its entry is gone (deleted/renamed
                // out from under it) — land on row 0 rather than losing
                // the cursor entirely.
                Some(0)
            } else {
                None
            });
        self.selection.set_cursor(new_cursor);
    }

    // ── Stage 11: viewport-driven thumbnail requests ────────────────────

    /// Which `visible` index range is in (or just outside) the viewport,
    /// for thumbnail-request scheduling only — never used for rendering
    /// (that's `list.rs::visible_range`/`grid.rs::visible_row_range`, both
    /// pixel-exact against the real theme token/tile constants). Grid mode
    /// treats one "row" as `row_step()` entries (a full tile row), the same
    /// vertical-step unit `move_cursor` already uses; list mode's
    /// `row_step()` is `1`, so this degenerates to plain item indices.
    fn thumbnail_range(&self) -> (usize, usize) {
        let total = self.visible.len();
        let step = self.row_step().max(1) as usize;
        let (first_row, row_span) = match self.scroll {
            None => (0, THUMB_INITIAL_ROWS),
            Some(viewport) => {
                let offset = viewport.absolute_offset().y.max(0.0);
                let bounds_height = viewport.bounds().height.max(0.0);
                let first_row = (offset / THUMB_ROW_HEIGHT_GUESS).floor() as usize;
                let row_span = (bounds_height / THUMB_ROW_HEIGHT_GUESS).ceil() as usize;
                (first_row, row_span)
            }
        };
        let first_row = first_row.saturating_sub(THUMB_OVERSCAN_ROWS);
        let last_row = first_row
            .saturating_add(row_span)
            .saturating_add(THUMB_OVERSCAN_ROWS * 2);
        let first = first_row.saturating_mul(step).min(total);
        let last = last_row.saturating_mul(step).min(total).max(first);
        (first, last)
    }

    /// Regular, non-symlink files in (or near) the viewport with a known
    /// mtime — Stage 11's "visible-range requests only" rule (no
    /// whole-directory eager generation). Empty whenever the current
    /// backend doesn't claim `Caps::THUMBNAILS` (today, any non-local
    /// backend) — checked here, once, rather than at every call site that
    /// might bubble `Event::ThumbnailsNeeded`. Directories/symlinks/
    /// entries with no `modified` (nothing to validate a cache entry
    /// against) are skipped, not just deprioritized.
    fn thumbnail_candidates(&self) -> Vec<ThumbCandidate> {
        if !self.caps().contains(Caps::THUMBNAILS) {
            return Vec::new();
        }
        let (first, last) = self.thumbnail_range();
        self.visible
            .get(first..last)
            .unwrap_or(&[])
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .filter(|entry| entry.kind == EntryKind::File && !entry.is_symlink)
            .filter_map(|entry| {
                let modified = entry.modified?;
                Some(ThumbCandidate {
                    location: self.location.join(&entry.name),
                    size_bytes: entry.size,
                    modified,
                })
            })
            .collect()
    }

    /// `Some(Event::ThumbnailsNeeded(..))` when there's at least one
    /// candidate, `None` otherwise — the shared tail every `update()` arm
    /// that could change what's on screen (a fresh listing, a watch
    /// refresh, a scroll, a sort/hidden-filter change) calls instead of
    /// returning a bare `None` for its event.
    fn thumbnail_event(&self) -> Option<Event> {
        let candidates = self.thumbnail_candidates();
        (!candidates.is_empty()).then_some(Event::ThumbnailsNeeded(candidates))
    }
}

/// The glyph for one row/tile — shared by `list.rs`/`grid.rs` so both
/// presentations pick the same icon for the same entry. Directories skip
/// the `MimeDb` call entirely (`inode/directory` is a fixed, free
/// classification — no glob lookup needed); everything else resolves a
/// mimetype from the name alone (no content sniff — see `core::mime`'s
/// module docs for why that's the right tradeoff for a per-frame,
/// per-row call).
pub(super) fn row_icon(entry: &FileEntry, mime_db: &MimeDb) -> saola_theme::icon::Icon {
    let category = if entry.kind == EntryKind::Directory {
        crate::core::mime::Category::Directory
    } else {
        crate::core::mime::category(&mime_db.guess(&entry.name, None))
    };
    crate::icons::for_entry(entry.kind, entry.is_symlink, category)
}

/// The cached thumbnail for one row/tile, if any — shared by `list.rs`/
/// `grid.rs` the same way [`row_icon`] is, so both presentations agree on
/// when a thumbnail replaces the glyph. `None` for anything
/// `DirectoryView::thumbnail_candidates` would never have requested in the
/// first place (directories, symlinks, no known `modified`) as well as for
/// an ordinary cache miss/in-flight request — this never triggers
/// generation itself (that only ever happens via `Event::ThumbnailsNeeded`,
/// see this module's docs), it only ever reads what's already decoded.
pub(super) fn thumbnail_for(
    state: &DirectoryView,
    thumb_cache: &ThumbCache,
    entry: &FileEntry,
) -> Option<crate::core::thumbs::ThumbHandle> {
    if entry.kind != EntryKind::File || entry.is_symlink {
        return None;
    }
    let modified = entry.modified?;
    let location = state.location.join(&entry.name);
    thumb_cache.get_for(&location, modified)
}

/// Two plain clicks this close together (and on the same row) are a
/// double-click. 300 ms is iced's own consecutive-click window —
/// `iced_core::mouse::Click::is_consecutive` uses `<= 300` ms (plus a 6 px
/// radius; the app-level analogue of "same place" is "same row index") —
/// so double-clicking here feels the same as double-clicking in an iced
/// `text_input`.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// Whether a plain click on `visible[index]` at `now` completes a
/// double-click with the previously recorded one. Pure — the caller
/// (`Message::RowClicked`'s handler) supplies `Instant::now()`, so tests
/// can drive this with synthetic instants instead of sleeping.
///
/// `saturating_duration_since` (never panics, clamps to zero) instead of
/// bare subtraction: `Instant` arithmetic that could underflow is exactly
/// the kind of "can't happen" the no-panic rule says to make impossible
/// anyway.
fn is_double_click(
    previous: Option<(usize, std::time::Instant)>,
    index: usize,
    now: std::time::Instant,
) -> bool {
    match previous {
        Some((last_index, last_time)) => {
            last_index == index && now.saturating_duration_since(last_time) <= DOUBLE_CLICK_WINDOW
        }
        None => false,
    }
}

/// `entries[i].name` starts with a dot — the one place `DirectoryView`
/// checks a name's bytes directly rather than going through
/// `display_name`, since a leading-dot check doesn't need a valid string
/// (a non-UTF-8 name can still start with an ASCII `.`).
fn is_hidden(entry: &FileEntry) -> bool {
    use std::os::unix::ffi::OsStrExt;
    entry.name.as_bytes().first() == Some(&b'.')
}

/// `open_target`'s "no backend for this scheme" fallback path: list
/// `fallback` directly rather than leaving the view permanently empty.
async fn list_with_fallback(fallback: &Location) -> Result<Vec<FileEntry>, VfsError> {
    match crate::modules::resolve(fallback) {
        Some(backend) => backend.list(fallback).await,
        None => Err(VfsError::Other {
            message: format!("no backend for scheme \"{}\"", fallback.scheme),
        }),
    }
}

/// Parses the breadcrumb path/URI editor's submitted text into a
/// [`Location`]. A thin wrapper over [`Location::parse`] (Stage 7 lifted
/// the actual grammar up to `core::vfs` so `core::places`' saved-server
/// entries can share it too) — kept as a named function here purely so the
/// call site below reads as "what the user typed", not "the shared URI
/// grammar".
fn parse_typed_location(input: &str) -> Location {
    Location::parse(input)
}

/// What `DirectoryView::create_new` is creating — kept as a tiny local enum
/// rather than a `bool` so the call sites (`Action::NewFolder`,
/// `Message::MenuNewFolderRequested`, `Message::MenuNewFileRequested`) read
/// as what they mean, not "true or false".
#[derive(Debug, Clone, Copy)]
enum NewKind {
    Folder,
    File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vfs::{Backend, FakeBackend};
    use std::ffi::OsStr;

    fn config() -> Config {
        Config::default()
    }

    fn file(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size: 10,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    fn dir(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    /// A regular file with a known `modified` time — the shape
    /// `DirectoryView::thumbnail_candidates` actually requires (`file()`
    /// above deliberately has `modified: None`, so it never qualifies —
    /// see that helper's use across every pre-Stage-11 test in this
    /// module, none of which should start bubbling `Event::
    /// ThumbnailsNeeded` just because this stage exists).
    fn thumbnailable_file(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size: 10,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)),
            is_symlink: false,
            mode: None,
        }
    }

    /// Directly exercises `recompute_visible`/selection plumbing by
    /// driving `update` with a synthetic `Listed` message — this is the
    /// "selection model (via FakeBackend)" test the stage plan calls for;
    /// `FakeBackend` itself is exercised in `core::vfs`'s own tests, and
    /// this view intentionally resolves backends by scheme (never takes
    /// one as a constructor argument), so we feed its `entries` the same
    /// shape a `FakeBackend::list` call would have produced.
    fn listed_view(entries: Vec<FileEntry>) -> DirectoryView {
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        let (_, event) = view.update(Message::Listed(Location::local("/home"), Ok(entries)));
        assert!(event.is_none());
        view
    }

    #[test]
    fn new_view_reports_its_location_loading_state_and_config_defaults() {
        let mut cfg = config();
        cfg.view = View::Grid;
        let view = DirectoryView::new(Location::local("/home"), &cfg);
        assert_eq!(view.location(), &Location::local("/home"));
        assert!(!view.is_loading());
        assert_eq!(view.view_mode, View::Grid);
    }

    #[test]
    fn open_marks_the_view_loading_and_returns_a_task() {
        let (view, _task) = DirectoryView::open(Location::local("/home"), &config());
        assert!(view.is_loading());
    }

    #[test]
    fn listing_sorts_dirs_first_and_hides_dotfiles_by_default() {
        let view = listed_view(vec![file("b.txt"), dir("zzz"), file(".hidden")]);
        let names: Vec<_> = view
            .visible
            .iter()
            .map(|&i| view.entries[i].display_name().into_owned())
            .collect();
        assert_eq!(names, vec!["zzz", "b.txt"]);
    }

    #[test]
    fn toggling_hidden_reveals_dotfiles() {
        let mut view = listed_view(vec![file(".hidden"), file("visible.txt")]);
        let (_, event) = view.apply_action_for_test(Action::ToggleHidden);
        assert!(event.is_none());
        assert_eq!(view.visible.len(), 2);
    }

    #[test]
    fn arrow_down_moves_cursor_and_selects_that_row() {
        let mut view = listed_view(vec![file("a"), file("b"), file("c")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(0));
        assert!(view.selection.is_selected(OsStr::new("a")));

        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(1));
        assert!(view.selection.is_selected(OsStr::new("b")));
        assert!(!view.selection.is_selected(OsStr::new("a")));
    }

    #[test]
    fn shift_down_extends_the_selection_from_the_anchor() {
        let mut view = listed_view(vec![file("a"), file("b"), file("c")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown); // cursor+anchor at "a" (0)
        let _ = view.apply_action_for_test(Action::ExtendSelectionDown); // extend to "b" (1)
        let _ = view.apply_action_for_test(Action::ExtendSelectionDown); // extend to "c" (2)
        assert_eq!(view.selection.len(), 3);
        assert_eq!(view.selection.anchor(), Some(0));
        assert_eq!(view.selection.cursor(), Some(2));
    }

    #[test]
    fn ctrl_a_selects_every_visible_row() {
        let mut view = listed_view(vec![file("a"), file("b"), dir("c")]);
        let _ = view.apply_action_for_test(Action::SelectAll);
        assert_eq!(view.selection.len(), 3);
    }

    #[test]
    fn enter_on_a_lone_selected_directory_requests_open_directory() {
        let mut view = listed_view(vec![dir("docs")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let (_, event) = view.apply_action_for_test(Action::Descend);
        match event {
            Some(Event::OpenDirectory(location)) => {
                assert_eq!(location, Location::local("/home/docs"));
            }
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_a_file_requests_activation() {
        let mut view = listed_view(vec![file("readme.txt")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let (_, event) = view.apply_action_for_test(Action::Descend);
        match event {
            Some(Event::Activated(locations)) => {
                assert_eq!(locations, vec![Location::local("/home/readme.txt")]);
            }
            other => panic!("expected Activated, got {other:?}"),
        }
    }

    // ── Double-click pairing (app-side, see `Message::RowClicked`) ──────
    //
    // The pure decision (`is_double_click`) is tested with synthetic
    // instants — never a real sleep — exactly the "test the argument-
    // taking half" split CLAUDE.md prescribes for env lookups. The handler
    // tests below then only rely on "two consecutive `update` calls in
    // one test run happen within 300 ms of each other", which is safe by
    // orders of magnitude.

    #[test]
    fn a_second_click_on_the_same_row_inside_the_window_is_a_double() {
        let base = std::time::Instant::now();
        let previous = Some((3, base));
        assert!(is_double_click(
            previous,
            3,
            base + std::time::Duration::from_millis(120)
        ));
        // Boundary is inclusive, matching iced's own `<= 300` ms check.
        assert!(is_double_click(previous, 3, base + DOUBLE_CLICK_WINDOW));
    }

    #[test]
    fn a_slow_or_misaimed_second_click_is_not_a_double() {
        let base = std::time::Instant::now();
        let previous = Some((3, base));
        // Too slow — one past the window.
        assert!(!is_double_click(
            previous,
            3,
            base + DOUBLE_CLICK_WINDOW + std::time::Duration::from_millis(1)
        ));
        // Fast enough, but a different row.
        assert!(!is_double_click(
            previous,
            4,
            base + std::time::Duration::from_millis(50)
        ));
        // Nothing recorded at all (first-ever click).
        assert!(!is_double_click(None, 3, base));
    }

    #[test]
    fn double_clicking_a_directory_row_requests_open_directory() {
        let mut view = listed_view(vec![dir("docs"), file("readme.txt")]);
        let (_, first) = view.update(Message::RowClicked(0));
        assert!(first.is_none(), "a single click must only select");
        assert!(view.selection.is_selected(OsStr::new("docs")));

        let (_, second) = view.update(Message::RowClicked(0));
        match second {
            Some(Event::OpenDirectory(location)) => {
                assert_eq!(location, Location::local("/home/docs"));
            }
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
        // The pair is consumed — a third quick click starts over rather
        // than activating again.
        assert!(view.last_click.is_none());
    }

    #[test]
    fn ctrl_clicks_never_pair_into_an_activation() {
        let mut view = listed_view(vec![dir("docs")]);
        view.modifiers = keyboard::Modifiers::CTRL;
        let (_, first) = view.update(Message::RowClicked(0));
        let (_, second) = view.update(Message::RowClicked(0));
        assert!(first.is_none());
        assert!(second.is_none());
        assert!(view.last_click.is_none());
    }

    #[test]
    fn a_relist_between_clicks_clears_the_pending_pair() {
        let mut view = listed_view(vec![dir("docs")]);
        let _ = view.update(Message::RowClicked(0));
        assert!(view.last_click.is_some());
        // A refresh/navigation rebuilds `visible` — row indices may now
        // mean different entries, so the recorded half must be dropped.
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![dir("docs"), dir("other")]),
        ));
        let (_, event) = view.update(Message::RowClicked(0));
        assert!(event.is_none(), "a stale pre-relist click must not pair");
    }

    #[test]
    fn ascend_requests_the_parent_directory() {
        let view = listed_view(vec![]);
        let mut view = view;
        let (_, event) = view.apply_action_for_test(Action::Ascend);
        match event {
            Some(Event::OpenDirectory(location)) => assert_eq!(location, Location::local("/")),
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn header_click_sorts_then_toggles_direction_on_second_click() {
        // Start sorted by Size (not the default) so the *first* Name click
        // below is a genuine "switch column" (always ascending), and the
        // second is the same-column click that toggles direction.
        let mut cfg = config();
        cfg.sort = SortKey::Size;
        let mut view = DirectoryView::new(Location::local("/home"), &cfg);
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("b"), file("a")]),
        ));

        let _ = view.update(Message::HeaderClicked(SortKey::Name));
        assert_eq!(view.sort, SortKey::Name);
        assert!(!view.sort_descending);
        let first_pass: Vec<_> = view
            .visible
            .iter()
            .filter_map(|&i| view.entries.get(i))
            .map(|e| e.display_name().into_owned())
            .collect();
        assert_eq!(first_pass, vec!["a", "b"]);

        let _ = view.update(Message::HeaderClicked(SortKey::Name));
        assert!(view.sort_descending);
        let second_pass: Vec<_> = view
            .visible
            .iter()
            .filter_map(|&i| view.entries.get(i))
            .map(|e| e.display_name().into_owned())
            .collect();
        assert_eq!(second_pass, vec!["b", "a"]);
    }

    #[test]
    fn stale_listed_response_is_ignored_after_a_navigation() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.navigate(Location::local("/elsewhere"));
        // A slow response for the *old* location arrives after the new
        // navigation already started.
        let (_, event) = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("stale")]),
        ));
        assert!(event.is_none());
        assert_eq!(view.entries, vec![file("a")]); // untouched by the stale response
    }

    #[test]
    fn pending_select_highlights_the_revealed_entry_once_listed() {
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        view.pending_select = Some(OsString::from("b"));
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("a"), file("b")]),
        ));
        assert!(view.selection.is_selected(OsStr::new("b")));
        assert!(view.pending_select.is_none());
    }

    #[test]
    fn a_listing_error_clears_entries_and_records_the_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Err(VfsError::PermissionDenied {
                location: "/home".to_owned(),
            }),
        ));
        assert!(view.entries.is_empty());
        assert!(view.error.is_some());
    }

    #[test]
    fn fake_backend_round_trips_through_the_registry_shaped_call() {
        // Sanity check that the `Backend` trait object this view calls
        // through (via `modules::resolve`) behaves the same way
        // `FakeBackend` does directly — protects the `Listed` plumbing
        // above from silently diverging from the real trait contract.
        let backend = FakeBackend::new().with_dir("/home", vec![file("a")]);
        let result = futures::executor::block_on(backend.list(&Location::local("/home"))).unwrap();
        assert_eq!(result, vec![file("a")]);
    }

    // ── Stage 4: history ────────────────────────────────────────────────

    #[test]
    fn navigate_pushes_back_and_clears_forward() {
        let mut view = listed_view(vec![]);
        assert!(!view.can_go_back());
        let _ = view.navigate(Location::local("/elsewhere"));
        assert!(view.can_go_back());
        assert!(!view.can_go_forward());
        assert_eq!(view.location(), &Location::local("/elsewhere"));
    }

    #[test]
    fn history_back_then_forward_round_trips() {
        let mut view = listed_view(vec![]); // starts at /home
        let _ = view.navigate(Location::local("/a"));
        let _ = view.navigate(Location::local("/b"));
        assert_eq!(view.location(), &Location::local("/b"));

        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &Location::local("/a"));
        assert!(view.can_go_forward());

        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &Location::local("/home"));
        assert!(!view.can_go_back());

        let _ = view.apply_action_for_test(Action::HistoryForward);
        assert_eq!(view.location(), &Location::local("/a"));
        let _ = view.apply_action_for_test(Action::HistoryForward);
        assert_eq!(view.location(), &Location::local("/b"));
        assert!(!view.can_go_forward());
    }

    #[test]
    fn history_back_at_the_start_is_a_no_op() {
        let mut view = listed_view(vec![]);
        let start = view.location().clone();
        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &start);
    }

    #[test]
    fn a_fresh_navigate_after_going_back_drops_the_old_forward_branch() {
        let mut view = listed_view(vec![]); // /home
        let _ = view.navigate(Location::local("/a"));
        let _ = view.apply_action_for_test(Action::HistoryBack); // back to /home, /a on forward
        assert!(view.can_go_forward());

        // Going somewhere new (not via forward) invalidates that redo path.
        let _ = view.navigate(Location::local("/c"));
        assert!(!view.can_go_forward());
    }

    // ── Stage 4: header-driven actions ──────────────────────────────────

    #[test]
    fn refresh_action_reloads_the_current_location() {
        let mut view = listed_view(vec![file("a")]);
        assert!(!view.is_loading());
        let _ = view.apply_action_for_test(Action::Refresh);
        assert!(view.is_loading());
    }

    #[test]
    fn set_view_actions_switch_presentation() {
        let mut view = listed_view(vec![]);
        assert_eq!(view.view_mode(), View::List);
        let _ = view.apply_action_for_test(Action::SetViewGrid);
        assert_eq!(view.view_mode(), View::Grid);
        let _ = view.apply_action_for_test(Action::SetViewList);
        assert_eq!(view.view_mode(), View::List);
    }

    #[test]
    fn caps_reflects_the_local_backend_including_watch_as_of_stage_5() {
        let view = listed_view(vec![]);
        let caps = view.caps();
        assert!(caps.contains(crate::core::vfs::Caps::LOCAL_PATH));
        assert!(caps.contains(crate::core::vfs::Caps::WATCH));
    }

    // ── Stage 4: grid cursor stepping ───────────────────────────────────

    #[test]
    fn grid_view_steps_the_cursor_by_a_full_row() {
        let mut cfg = config();
        cfg.view = View::Grid;
        let mut view = DirectoryView::new(Location::local("/home"), &cfg);
        // No scroll viewport has been reported in a headless test, so the
        // step is whatever `columns_for_scroll` falls back to — asked here
        // rather than assumed, so this test keeps testing *stepping by a
        // row* even after a token change moves how wide a row is.
        let columns = grid::columns_for_scroll(None);
        let names: Vec<_> = (0..(columns * 2))
            .map(|i| file(&format!("f{i:02}")))
            .collect();
        let _ = view.update(Message::Listed(Location::local("/home"), Ok(names)));

        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(0));
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(columns));

        let _ = view.apply_action_for_test(Action::MoveCursorRight);
        assert_eq!(view.selection.cursor(), Some(columns + 1));
    }

    #[test]
    fn list_view_left_right_are_still_no_ops() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(0));
        let _ = view.apply_action_for_test(Action::MoveCursorRight);
        assert_eq!(view.selection.cursor(), Some(0));
    }

    // ── Stage 4: breadcrumb/path editing ────────────────────────────────

    #[test]
    fn edit_path_action_seeds_the_buffer_from_the_current_location() {
        let mut view = listed_view(vec![]);
        assert_eq!(view.path_edit(), None);
        let _ = view.apply_action_for_test(Action::EditPath);
        assert_eq!(view.path_edit(), Some("/home"));
    }

    #[test]
    fn path_submitted_requests_navigation_and_closes_the_editor() {
        let mut view = listed_view(vec![]);
        let _ = view.apply_action_for_test(Action::EditPath);
        let _ = view.update(Message::PathInputChanged("/etc".to_owned()));
        let (_, event) = view.update(Message::PathSubmitted);
        assert_eq!(view.path_edit(), None);
        match event {
            Some(Event::OpenDirectory(location)) => {
                assert_eq!(location, Location::local("/etc"));
            }
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn submitting_a_blank_path_is_a_no_op() {
        let mut view = listed_view(vec![]);
        let _ = view.apply_action_for_test(Action::EditPath);
        let _ = view.update(Message::PathInputChanged("   ".to_owned()));
        let (_, event) = view.update(Message::PathSubmitted);
        assert!(event.is_none());
    }

    #[test]
    fn breadcrumb_clicked_requests_navigation() {
        let mut view = listed_view(vec![]);
        let (_, event) = view.update(Message::BreadcrumbClicked(Location::local("/")));
        match event {
            Some(Event::OpenDirectory(location)) => assert_eq!(location, Location::local("/")),
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn parse_typed_location_handles_local_and_remote_forms() {
        assert_eq!(
            parse_typed_location("/home/jordan"),
            Location::local("/home/jordan")
        );
        assert_eq!(
            parse_typed_location("sftp://jordan@host/srv"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/srv"),
            }
        );
        assert_eq!(
            parse_typed_location("sftp://jordan@host"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/"),
            }
        );
    }

    // ── Stage 5: watch events ────────────────────────────────────────────

    #[test]
    fn watch_created_is_applied_only_once_metadata_comes_back() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_watch_events_for_test(vec![DirEvent::Created(OsString::from("b"))]);
        // The name alone isn't enough to add a row — `entries` is
        // untouched until `WatchRefreshed` supplies the metadata.
        assert_eq!(view.entries, vec![file("a")]);

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("b"), Some(file("b")))],
        ));
        assert!(event.is_none());
        assert!(view.entries.contains(&file("b")));
    }

    #[test]
    fn watch_removed_drops_the_entry_and_its_selection_immediately() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        view.selection.click(0, OsString::from("a"));
        assert!(view.selection.is_selected(OsStr::new("a")));

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Removed(OsString::from("a"))]);

        assert!(!view.entries.iter().any(|e| e.name == "a"));
        assert!(!view.selection.is_selected(OsStr::new("a")));
        assert!(view.entries.contains(&file("b")));
    }

    #[test]
    fn watch_rename_keeps_the_renamed_entry_selected() {
        let mut view = listed_view(vec![file("old.txt")]);
        view.selection.click(0, OsString::from("old.txt"));

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Renamed {
            from: OsString::from("old.txt"),
            to: OsString::from("new.txt"),
        }]);
        // The "from" half is gone immediately; the "to" half isn't a row
        // yet (still awaiting its metadata fetch) but is already selected
        // — `Selection` is name-keyed, so this holds true even before the
        // row exists.
        assert!(!view.entries.iter().any(|e| e.name == "old.txt"));
        assert!(!view.selection.is_selected(OsStr::new("old.txt")));
        assert!(view.selection.is_selected(OsStr::new("new.txt")));

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("new.txt"), Some(file("new.txt")))],
        ));
        assert!(event.is_none());
        assert!(view.entries.contains(&file("new.txt")));
        assert!(view.selection.is_selected(OsStr::new("new.txt")));
    }

    #[test]
    fn watch_changed_refreshes_an_existing_entrys_metadata() {
        let mut view = listed_view(vec![file("a")]);
        let mut updated = file("a");
        updated.size = 999;

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Changed(OsString::from("a"))]);
        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("a"), Some(updated.clone()))],
        ));
        assert!(event.is_none());
        assert_eq!(view.entries, vec![updated]);
    }

    #[test]
    fn watch_overflow_falls_back_to_a_full_reload_and_drops_the_rest_of_the_batch() {
        let mut view = listed_view(vec![file("a")]);
        assert!(!view.is_loading());

        let _ = view.apply_watch_events_for_test(vec![
            DirEvent::Created(OsString::from("b")),
            DirEvent::Overflow,
        ]);
        // `load()` sets `loading` synchronously, before its `Task` resolves
        // — the same signal `Action::Refresh`'s own test checks.
        assert!(view.is_loading());
    }

    #[test]
    fn watch_refreshed_for_a_stale_location_is_dropped() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.navigate(Location::local("/elsewhere"));

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("stale"), Some(file("stale")))],
        ));
        assert!(event.is_none());
        assert!(!view.entries.iter().any(|e| e.name == "stale"));
    }

    #[test]
    fn watch_refreshed_removes_an_entry_that_vanished_before_the_fetch_landed() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("a"), None)],
        ));
        assert!(!view.entries.iter().any(|e| e.name == "a"));
    }

    // ── Stage 8: clipboard, rename, new folder/file ─────────────────────

    #[test]
    fn copy_bubbles_copy_requested_with_the_selection() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let (_, event) = view.apply_action_for_test(Action::Copy);
        match event {
            Some(Event::CopyRequested(locations)) => {
                assert_eq!(locations, vec![Location::local("/home/a")]);
            }
            other => panic!("expected CopyRequested, got {other:?}"),
        }
    }

    #[test]
    fn copy_with_nothing_selected_is_a_no_op() {
        let mut view = listed_view(vec![file("a")]);
        let (_, event) = view.apply_action_for_test(Action::Copy);
        assert!(event.is_none());
    }

    #[test]
    fn cut_bubbles_cut_requested_with_the_selection() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let (_, event) = view.apply_action_for_test(Action::Cut);
        match event {
            Some(Event::CutRequested(locations)) => {
                assert_eq!(locations, vec![Location::local("/home/a")]);
            }
            other => panic!("expected CutRequested, got {other:?}"),
        }
    }

    // ── Stage 13: properties ─────────────────────────────────────────────

    #[test]
    fn properties_action_bubbles_properties_requested_with_the_selection() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let (_, event) = view.apply_action_for_test(Action::Properties);
        match event {
            Some(Event::PropertiesRequested(items)) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].0, Location::local("/home/a"));
                assert_eq!(items[0].1.name, OsString::from("a"));
            }
            other => panic!("expected PropertiesRequested, got {other:?}"),
        }
    }

    #[test]
    fn properties_with_nothing_selected_is_a_no_op() {
        let mut view = listed_view(vec![file("a")]);
        let (_, event) = view.apply_action_for_test(Action::Properties);
        assert!(event.is_none());
    }

    #[test]
    fn properties_menu_message_bubbles_the_same_event_and_closes_the_menu() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        view.menu_open = true;
        let (_, event) = view.update(Message::MenuPropertiesRequested);
        assert!(!view.menu_open());
        match event {
            Some(Event::PropertiesRequested(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected PropertiesRequested, got {other:?}"),
        }
    }

    #[test]
    fn paste_bubbles_paste_requested_with_the_current_directory() {
        let mut view = listed_view(vec![]);
        let (_, event) = view.apply_action_for_test(Action::Paste);
        match event {
            Some(Event::PasteRequested(location)) => {
                assert_eq!(location, Location::local("/home"));
            }
            other => panic!("expected PasteRequested, got {other:?}"),
        }
    }

    #[test]
    fn rename_starts_inline_editing_when_exactly_one_entry_is_selected() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert!(view.rename_state().is_none());
        let (_, event) = view.apply_action_for_test(Action::Rename);
        assert!(event.is_none());
        let state = view.rename_state().expect("rename should have started");
        assert_eq!(state.original, OsString::from("a"));
        assert_eq!(state.buffer, "a");
    }

    #[test]
    fn rename_with_multiple_selected_is_a_no_op() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::SelectAll);
        let _ = view.apply_action_for_test(Action::Rename);
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_changed_updates_the_buffer_and_clears_a_previous_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        view.rename.as_mut().unwrap().error = Some("boom".to_owned());

        let _ = view.update(Message::RenameChanged("a-new".to_owned()));
        let state = view.rename_state().unwrap();
        assert_eq!(state.buffer, "a-new");
        assert!(state.error.is_none());
    }

    #[test]
    fn rename_cancelled_clears_the_edit_state() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        assert!(view.rename_state().is_some());
        let _ = view.update(Message::RenameCancelled);
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn escape_while_renaming_cancels_via_the_keyboard_path() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);

        let escape = keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let _ = view.update(Message::Keyboard(escape));
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn submitting_an_empty_or_slash_containing_name_re_opens_with_an_inline_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);

        let _ = view.update(Message::RenameChanged("   ".to_owned()));
        let _ = view.update(Message::RenameSubmitted);
        assert!(view.rename_state().unwrap().error.is_some());

        let _ = view.update(Message::RenameChanged("a/b".to_owned()));
        let _ = view.update(Message::RenameSubmitted);
        assert!(view.rename_state().unwrap().error.is_some());
    }

    #[test]
    fn submitting_the_same_name_back_is_a_silent_no_op() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameSubmitted);
        // No backend call was needed — the field just closes.
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_result_for_a_stale_directory_is_dropped() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.navigate(Location::local("/elsewhere"));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Ok(()),
        ));
        assert!(event.is_none());
        // `navigate` already cleared `rename`; the stale result must not
        // resurrect or otherwise touch it.
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_result_success_renames_the_entry_and_selection_in_place() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameChanged("a-renamed".to_owned()));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Ok(()),
        ));
        // Stage 10: a successful rename bubbles `Event::Renamed` so `App`
        // can push it onto the undo stack.
        match event {
            Some(Event::Renamed(from, to)) => {
                assert_eq!(from, Location::local("/home/a"));
                assert_eq!(to, Location::local("/home/a-renamed"));
            }
            other => panic!("expected Renamed, got {other:?}"),
        }
        assert!(view.rename_state().is_none());
        assert!(view.entries.iter().any(|e| e.name == "a-renamed"));
        assert!(!view.entries.iter().any(|e| e.name == "a"));
        assert!(view.selection.is_selected(OsStr::new("a-renamed")));
    }

    #[test]
    fn rename_result_failure_keeps_editing_with_an_inline_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameChanged("b".to_owned()));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Err(VfsError::AlreadyExists {
                location: "/home/b".to_owned(),
            }),
        ));
        assert!(event.is_none());
        let state = view
            .rename_state()
            .expect("edit should stay open on failure");
        assert!(state.error.is_some());
        // Nothing renamed in `entries` — the backend call never succeeded.
        assert!(view.entries.iter().any(|e| e.name == "a"));
    }

    #[test]
    fn create_result_success_reloads_selects_and_starts_renaming_the_new_entry() {
        let mut view = listed_view(vec![file("existing")]);
        let (_, event) = view.update(Message::CreateResult(
            Location::local("/home"),
            OsString::from("New Folder"),
            Ok(()),
        ));
        // Stage 10: a successful create bubbles `Event::Created` so `App`
        // can push it onto the undo stack.
        match event {
            Some(Event::Created(created)) => {
                assert_eq!(created, Location::local("/home/New Folder"));
            }
            other => panic!("expected Created, got {other:?}"),
        }
        assert!(view.is_loading());

        // The reload lands, carrying the freshly created entry.
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("existing"), dir("New Folder")]),
        ));
        assert!(view.selection.is_selected(OsStr::new("New Folder")));
        let state = view
            .rename_state()
            .expect("New Folder should start already in rename mode");
        assert_eq!(state.original, OsString::from("New Folder"));
    }

    #[test]
    fn create_result_for_a_stale_directory_is_dropped() {
        let mut view = listed_view(vec![]);
        let _ = view.navigate(Location::local("/elsewhere"));
        let (_, event) = view.update(Message::CreateResult(
            Location::local("/home"),
            OsString::from("New Folder"),
            Ok(()),
        ));
        assert!(event.is_none());
        assert_eq!(view.location(), &Location::local("/elsewhere"));
        assert!(view.rename_state().is_none());
    }

    // ── Stage 11: viewport-driven thumbnail requests ────────────────────

    #[test]
    fn listed_with_no_thumbnailable_entries_bubbles_no_thumbnail_event() {
        // `file()`/`dir()` both have `modified: None` — every pre-Stage-11
        // test in this module builds its fixtures this way, so this
        // pins down that Stage 11 didn't change their behavior: still no
        // event at all (checked inside `listed_view` itself already, but
        // named explicitly here as the Stage 11 contract, not an
        // incidental side effect of `file()`'s shape).
        let _ = listed_view(vec![file("a.txt"), dir("sub")]);
    }

    #[test]
    fn listed_with_thumbnailable_files_bubbles_thumbnails_needed() {
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        let (_, event) = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![
                thumbnailable_file("photo.jpg"),
                dir("sub"),
                file("no-mtime.txt"),
            ]),
        ));
        match event {
            Some(Event::ThumbnailsNeeded(candidates)) => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].location, Location::local("/home/photo.jpg"));
                assert_eq!(candidates[0].size_bytes, 10);
            }
            other => panic!("expected ThumbnailsNeeded, got {other:?}"),
        }
    }

    #[test]
    fn thumbnail_candidates_exclude_directories_and_symlinks() {
        let mut symlinked = thumbnailable_file("link.jpg");
        symlinked.is_symlink = true;
        // Not `listed_view` here — that helper asserts no event bubbles,
        // which doesn't hold once thumbnailable entries are in the mix
        // (see `listed_with_thumbnailable_files_bubbles_thumbnails_needed`
        // above); this test only cares about `thumbnail_candidates`'
        // filtering, not what `update` bubbles.
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![thumbnailable_file("photo.jpg"), dir("sub"), symlinked]),
        ));
        let candidates = view.thumbnail_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].location, Location::local("/home/photo.jpg"));
    }

    #[test]
    fn thumbnail_candidates_are_empty_without_caps_thumbnails() {
        // `caps()` resolves via `modules::resolve`, which only ever
        // returns backends compiled into this binary — there's no
        // `Caps`-lacking backend to point a real `Location` at here, so
        // this instead pins the *positive* case (the local backend does
        // claim `Caps::THUMBNAILS` — verified directly in
        // `modules::local`'s own tests) and trusts the `!self.caps().
        // contains(..)` guard reads correctly by inspection; a location
        // whose scheme resolves to nothing at all is the closest
        // reachable "no capability" stand-in this view can exercise.
        let view = DirectoryView::new(
            Location {
                scheme: "nonexistent".to_owned(),
                authority: None,
                path: PathBuf::from("/x"),
            },
            &config(),
        );
        assert!(view.thumbnail_candidates().is_empty());
    }

    impl DirectoryView {
        /// Test-only shim: `apply_action` is private (only reached via
        /// `Message::Keyboard` normally), but driving it directly keeps
        /// these tests from having to fabricate `iced::keyboard::Event`s.
        fn apply_action_for_test(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
            self.apply_action(action)
        }

        /// Test-only shim: `apply_watch_events` is private (only reached
        /// via `Message::Watch` normally), but driving it directly keeps
        /// these tests from having to build a real `watch::subscription`
        /// stream (which needs a live tokio runtime and a real inotify
        /// watch — exercised instead by `modules::local`'s own temp-dir
        /// tests and the stage's manual done-criterion).
        fn apply_watch_events_for_test(&mut self, events: Vec<DirEvent>) -> Task<Message> {
            self.apply_watch_events(events)
        }
    }
}
