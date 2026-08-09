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

mod grid;
mod list;
mod selection;
mod typeahead;

use std::ffi::OsString;
use std::path::PathBuf;

use iced::widget::scrollable;
use iced::{Element, Task, keyboard};
use saola_theme::Theme;

use crate::config::{Config, SortKey, View};
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::fs::sort;
use crate::core::vfs::{Caps, Location, VfsError};
use crate::keymap::{self, Action};
use crate::ui::breadcrumbs;

/// Cursor rows a single PageUp/PageDown moves — a fixed placeholder rather
/// than the real viewport-height/row-height math, which needs a theme
/// lookup `update()` doesn't otherwise take. Stage 4's navigation-chrome
/// pass can wire this to `DirectoryView::scroll` once it threads sizing
/// through; the keymap contract (`Action::MoveCursorPageUp/Down`) already
/// doesn't change either way.
const PAGE_ROWS: isize = 10;

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
    RowClicked(usize),
    RowDoubleClicked(usize),
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
}

/// What the owner (the app, via `ui::explorer`) decides to act on. The
/// view only ever *requests* these — see the module docs.
#[derive(Debug, Clone)]
pub enum Event {
    /// Descend into a directory or ascend to its parent. The caller
    /// decides whether this reuses the current view or opens a new tab
    /// (a future stage); Stage 3's `explorer.rs` always reuses it.
    OpenDirectory(Location),
    /// Enter/double-click on non-directory entries: open them. Stage 6
    /// wires this to `xdg-open`-equivalent app resolution.
    Activated(Vec<Location>),
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
        crate::modules::resolve(&self.location.scheme)
            .map(|backend| backend.caps())
            .unwrap_or(Caps::empty())
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
                let Some(backend) = crate::modules::resolve(&probed.scheme) else {
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
        self.load()
    }

    /// Fetch `self.location`'s listing.
    fn load(&mut self) -> Task<Message> {
        self.loading = true;
        let location_for_task = self.location.clone();
        let location_for_message = self.location.clone();
        Task::perform(
            async move {
                let backend = crate::modules::resolve(&location_for_task.scheme);
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
                        self.apply_pending_select();
                    }
                    Err(err) => {
                        self.entries.clear();
                        self.visible.clear();
                        self.error = Some(err);
                    }
                }
                (Task::none(), None)
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
                    self.selection.toggle_click(index, name);
                } else if self.modifiers.shift() {
                    self.extend_to(index);
                } else {
                    self.selection.click(index, name);
                }
                (Task::none(), None)
            }
            Message::RowDoubleClicked(index) => {
                let Some(name) = self.entry_at(index).map(|entry| entry.name.clone()) else {
                    return (Task::none(), None);
                };
                self.selection.click(index, name);
                (Task::none(), self.activate_selection())
            }
            Message::HeaderClicked(key) => {
                if self.sort == key {
                    self.sort_descending = !self.sort_descending;
                } else {
                    self.sort = key;
                    self.sort_descending = false;
                }
                self.recompute_visible();
                (Task::none(), None)
            }
            Message::Scrolled(viewport) => {
                self.scroll = Some(viewport);
                (Task::none(), None)
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
        }
    }

    pub fn view<'a>(&'a self, theme: &'a Theme) -> Element<'a, Message> {
        match self.view_mode {
            View::List => list::view(self, theme),
            View::Grid => grid::view(self, theme),
        }
    }

    // ── keyboard/action handling ────────────────────────────────────────

    fn handle_keyboard(&mut self, event: keyboard::Event) -> (Task<Message>, Option<Event>) {
        match event {
            keyboard::Event::KeyPressed { key, modifiers, .. } => {
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
                (Task::none(), None)
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
        }
    }

    /// How many `visible` positions one Up/Down/PageUp/PageDown step
    /// covers: one item in list view, one full tile-row (`grid::
    /// GRID_COLUMNS` items) in grid view — so "Down" always means "the
    /// next thing spatially below", not "the next name alphabetically",
    /// regardless of presentation. Left/Right (grid-only) always step by
    /// exactly one, independent of this.
    fn row_step(&self) -> isize {
        match self.view_mode {
            View::List => 1,
            View::Grid => grid::GRID_COLUMNS as isize,
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

    /// If `pending_select` names an entry in the just-loaded listing,
    /// select it and put the cursor there; otherwise leave the selection
    /// as-is (a `--select` target that vanished before we got here is not
    /// worth an error).
    fn apply_pending_select(&mut self) {
        let Some(name) = self.pending_select.take() else {
            return;
        };
        let found = self
            .visible
            .iter()
            .position(|&i| self.entries.get(i).is_some_and(|entry| entry.name == name));
        if let Some(index) = found {
            self.selection.click(index, name);
        }
    }

    /// Rebuild `visible` from `entries` (hidden-filtered, sorted) and try
    /// to keep the keyboard cursor on the same *entry* it was on before —
    /// falling back to clamping its raw index. A view that never had a
    /// cursor stays without one (`None`) rather than inventing row 0 —
    /// the very first arrow-key press does that (see
    /// `current_cursor_or_before_start`), the same way a fresh view with
    /// no clicks yet has nothing selected.
    fn recompute_visible(&mut self) {
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
    match crate::modules::resolve(&fallback.scheme) {
        Some(backend) => backend.list(fallback).await,
        None => Err(VfsError::Other {
            message: format!("no backend for scheme \"{}\"", fallback.scheme),
        }),
    }
}

/// Parses the breadcrumb path/URI editor's submitted text into a
/// [`Location`]. Anything containing `"://"` is treated as
/// `scheme://[authority]/path` (a remote location typed by hand — a
/// scheme nothing recognizes surfaces as an ordinary "no backend"
/// `VfsError` at load time via `modules::resolve`, not a parse error
/// here); everything else is a bare local path. Mirrors `Location`'s own
/// `Display` impl (`core::vfs`), so round-tripping "edit, don't change
/// anything, submit" reproduces the same location.
fn parse_typed_location(input: &str) -> Location {
    let Some((scheme, rest)) = input.split_once("://") else {
        return Location::local(PathBuf::from(input));
    };
    if scheme.is_empty() {
        return Location::local(PathBuf::from(input));
    }
    match rest.find('/') {
        Some(path_start) => {
            let authority = &rest[..path_start];
            let path = &rest[path_start..];
            Location {
                scheme: scheme.to_owned(),
                authority: (!authority.is_empty()).then(|| authority.to_owned()),
                path: PathBuf::from(path),
            }
        }
        // No `/` at all after the scheme — the whole remainder is the
        // authority, with an implied root path.
        None => Location {
            scheme: scheme.to_owned(),
            authority: (!rest.is_empty()).then(|| rest.to_owned()),
            path: PathBuf::from("/"),
        },
    }
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
        }
    }

    fn dir(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            is_symlink: false,
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
    fn caps_reflects_the_local_backend_and_lacks_watch() {
        let view = listed_view(vec![]);
        let caps = view.caps();
        assert!(caps.contains(crate::core::vfs::Caps::LOCAL_PATH));
        assert!(!caps.contains(crate::core::vfs::Caps::WATCH));
    }

    // ── Stage 4: grid cursor stepping ───────────────────────────────────

    #[test]
    fn grid_view_steps_the_cursor_by_a_full_row() {
        let mut cfg = config();
        cfg.view = View::Grid;
        let mut view = DirectoryView::new(Location::local("/home"), &cfg);
        let names: Vec<_> = (0..(grid::GRID_COLUMNS * 2))
            .map(|i| file(&format!("f{i:02}")))
            .collect();
        let _ = view.update(Message::Listed(Location::local("/home"), Ok(names)));

        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(0));
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert_eq!(view.selection.cursor(), Some(grid::GRID_COLUMNS));

        let _ = view.apply_action_for_test(Action::MoveCursorRight);
        assert_eq!(view.selection.cursor(), Some(grid::GRID_COLUMNS + 1));
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

    impl DirectoryView {
        /// Test-only shim: `apply_action` is private (only reached via
        /// `Message::Keyboard` normally), but driving it directly keeps
        /// these tests from having to fabricate `iced::keyboard::Event`s.
        fn apply_action_for_test(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
            self.apply_action(action)
        }
    }
}
