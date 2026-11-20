//! `DirectoryView` — self-contained per-directory state (the tabs seam):
//! the app holds `Vec<(ViewId, DirectoryView)>` plus the focused
//! `ui::explorer::ViewId` (UI shows one for now, see `src/app/mod.rs`).
//! This module never navigates itself for a *new* location —
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

// Most of `DirectoryView`'s behavior lives in these child modules, each
// with its own `impl DirectoryView { ... }` block. A child module can read
// its parent's private fields, so the struct's fields stay private here;
// methods one file calls from another are `pub(super)` (visible inside
// `dirview` only, never to `ui::explorer` or the app).
pub mod context;
mod cursor;
mod grid;
mod keys;
mod list;
mod listing;
pub mod menu;
mod navigation;
mod pointer;
mod prefs;
mod rename;
mod selection;
#[cfg(test)]
mod test_support;
mod thumbs;
mod typeahead;
mod watch;

use std::ffi::OsString;
use std::time::SystemTime;

use iced::widget::scrollable;
use iced::{Element, Subscription, Task, keyboard};
use saola_theme::{Surface, Theme};

use crate::config::{Config, CustomAction, SortKey, View};
use crate::core::apps::AppsDb;
use crate::core::failure::{Failure, Incident, Operation};
use crate::core::fs::entry::FileEntry;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::core::vfs::{Caps, DirEvent, Location, VfsError};
use crate::keymap::{Action, AppAction};
use crate::ui::menus;
use crate::ui::right_click::RightClick;
// `row_icon`/`thumbnail_for` moved to `thumbs.rs`; re-exported here so
// `list.rs`/`grid.rs` keep importing them as `super::{row_icon, thumbnail_for}`.
pub(super) use thumbs::{row_icon, thumbnail_for};

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
    /// second plain click on the same row within `DOUBLE_CLICK_WINDOW`
    /// of the last one (`DirectoryView::last_click`, paired by
    /// `is_double_click` — both in `pointer.rs`) activates the selection.
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

    /// Anything from the context menu or the Open-with popover
    /// (`ui::menus`, plus the header's overflow button that opens it) —
    /// see [`menu::Message`]. A new menu entry adds its variant there, not
    /// here.
    Menu(menu::Message),

    // ── Stage 8: rename / new folder / new file ─────────────────────────
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
    CreateResult(Incident, Location, OsString, Result<(), VfsError>),
    // --- features (alphabetical) ---
    /// Right-click placement and selection for the context menu — see
    /// `context.rs`.
    Context(context::Message),
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
    /// A keymap [`Action::App`] the view received: passed up unchanged,
    /// because app-level commands are the window's to carry out, never one
    /// view's (see [`AppAction`]).
    App(AppAction),
    // --- features (alphabetical) ---
    /// An operation this view started failed (today: New Folder / New File
    /// could not be created). The owner decides how to show it; see
    /// `ui::dialogs::error_notice`.
    Failed(Incident, Operation, Vec<Failure>),
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
    /// (`menu::Message::CustomActionRequested`). Small and static for the
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
    // --- features (alphabetical) ---
    /// Whether the current location has listed successfully (session
    /// state records a folder as "last location" only once this is true).
    /// Cleared whenever a new listing starts (`load`).
    listed: bool,
    /// What the open popover hangs from; `None` = the header's overflow
    /// button. See `context.rs`.
    menu_anchor: Option<context::MenuAnchor>,
    /// Where the latest right button press landed, held between the
    /// position message and the row/empty message that follows it.
    last_press: Option<iced::Point>,
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
            // --- features (alphabetical) ---
            listed: false,
            menu_anchor: None,
            last_press: None,
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
    /// one per open view, tagged with that view's id
    /// (`src/app/subscription.rs`). Identified
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

    /// The modification time of the entry named `name` in this directory,
    /// whether or not it is selected. Activation falls back to the cursor
    /// row when nothing is selected, so a lookup through
    /// [`Self::selected_entries`] would miss it; the content-sniff cache is
    /// keyed by mtime and needs this to find the verdict for that row.
    pub fn modified_of(&self, name: &std::ffi::OsStr) -> Option<SystemTime> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.modified)
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

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Event>) {
        match message {
            Message::Listed(location, result) => self.on_listed(location, result),
            Message::TargetResolved(location, select, result) => {
                self.on_target_resolved(location, select, result)
            }
            Message::RowClicked(index) => self.on_row_clicked(index),
            Message::HeaderClicked(key) => self.on_header_clicked(key),
            Message::Scrolled(viewport) => self.on_scrolled(viewport),
            Message::Keyboard(event) => self.handle_keyboard(event),
            Message::Action(action) => self.apply_action(action),
            Message::BreadcrumbClicked(location) => self.on_breadcrumb_clicked(location),
            Message::PathInputChanged(value) => self.on_path_input_changed(value),
            Message::PathSubmitted => self.on_path_submitted(),
            Message::Watch(events) => (self.apply_watch_events(events), None),
            Message::WatchRefreshed(location, results) => {
                self.on_watch_refreshed(location, results)
            }
            Message::Menu(inner) => self.on_menu(inner),

            // ── Stage 8: rename / new folder / new file ─────────────────
            Message::RenameChanged(value) => self.on_rename_changed(value),
            Message::RenameSubmitted => (self.submit_rename(), None),
            Message::RenameCancelled => self.on_rename_cancelled(),
            Message::RenameResult(dir_location, old_name, result) => {
                self.on_rename_result(dir_location, old_name, result)
            }
            Message::CreateResult(incident, dir, name, result) => {
                self.on_create_result(incident, dir, name, result)
            }
            // --- features (alphabetical) ---
            Message::Context(inner) => self.on_context(inner),
        }
    }

    /// `s` is the window's ground (`files.toml`'s `surface` knob), threaded
    /// in from `ui::explorer`. It reaches the listing only: `menus::overlay`
    /// deliberately never sees it, because a popover is always ink by the
    /// style guide no matter what the window under it is drawn on.
    pub fn view<'a>(
        &'a self,
        theme: &'a Theme,
        s: Surface,
        mime_db: &'a MimeDb,
        thumb_cache: &'a ThumbCache,
        apps_db: &'a AppsDb,
        clipboard_has_contents: bool,
    ) -> Element<'a, Message> {
        let content = match self.view_mode {
            View::List => list::view(self, theme, s, mime_db, thumb_cache),
            View::Grid => grid::view(self, theme, s, mime_db, thumb_cache),
        };
        // Report right-clicks with their position (see `context.rs`); the
        // popover overlay is stacked over this same area, so both share one
        // coordinate frame.
        let content = RightClick::new(
            content,
            |point| Message::Context(context::Message::PointerPressed(point)),
            Message::Context(context::Message::EmptyPressed),
        )
        .into();
        menus::overlay(
            theme,
            self,
            mime_db,
            apps_db,
            clipboard_has_contents,
            content,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::core::vfs::{Backend, FakeBackend};

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
    fn fake_backend_round_trips_through_the_registry_shaped_call() {
        // Sanity check that the `Backend` trait object this view calls
        // through (via `modules::resolve`) behaves the same way
        // `FakeBackend` does directly — protects the `Listed` plumbing
        // above from silently diverging from the real trait contract.
        let backend = FakeBackend::new().with_dir("/home", vec![file("a")]);
        let result = futures::executor::block_on(backend.list(&Location::local("/home"))).unwrap();
        assert_eq!(result, vec![file("a")]);
    }

    #[test]
    fn caps_reflects_the_local_backend_including_watch_as_of_stage_5() {
        let view = listed_view(vec![]);
        let caps = view.caps();
        assert!(caps.contains(crate::core::vfs::Caps::LOCAL_PATH));
        assert!(caps.contains(crate::core::vfs::Caps::WATCH));
    }
}
