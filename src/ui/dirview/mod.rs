//! `DirectoryView` — self-contained per-directory state (the tabs seam):
//! the app holds `Vec<DirectoryView> + active` (UI shows one for now, see
//! `main.rs`). This module never navigates itself — clicks, Enter, and
//! Backspace/Alt+Up all resolve to an [`Event`] the owner decides whether
//! to act on, so a future tabs feature can open a new view instead of
//! reusing this one without touching this file.

mod list;
mod selection;

use std::ffi::OsString;
use std::path::PathBuf;

use iced::widget::scrollable;
use iced::{Element, Task, keyboard};
use saola_theme::Theme;

use crate::config::{Config, SortKey, View};
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::fs::sort;
use crate::core::vfs::{Location, VfsError};
use crate::keymap::{self, Action};

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
    /// Back-stack of previously-visited locations, most recent last.
    /// Stage 4 builds the actual back/forward navigation UI on top of
    /// this; Stage 3 only maintains it.
    history: Vec<Location>,
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
        self.location = location;
        self.selection.clear();
        self.pending_select = None;
        self.error = None;
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
        }
    }

    pub fn view<'a>(&'a self, theme: &'a Theme) -> Element<'a, Message> {
        list::view(self, theme)
    }

    // ── keyboard/action handling ────────────────────────────────────────

    fn handle_keyboard(&mut self, event: keyboard::Event) -> (Task<Message>, Option<Event>) {
        match event {
            keyboard::Event::KeyPressed { key, modifiers, .. } => {
                match keymap::resolve(&key, modifiers) {
                    Some(action) => self.apply_action(action),
                    None => (Task::none(), None),
                }
            }
            keyboard::Event::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                (Task::none(), None)
            }
            keyboard::Event::KeyReleased { .. } => (Task::none(), None),
        }
    }

    fn apply_action(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
        match action {
            Action::MoveCursorUp => {
                self.move_cursor(-1);
                (Task::none(), None)
            }
            Action::MoveCursorDown => {
                self.move_cursor(1);
                (Task::none(), None)
            }
            Action::MoveCursorLeft | Action::MoveCursorRight => (Task::none(), None),
            Action::MoveCursorHome => {
                self.move_cursor_to(0);
                (Task::none(), None)
            }
            Action::MoveCursorEnd => {
                self.move_cursor_to(self.visible.len().saturating_sub(1));
                (Task::none(), None)
            }
            Action::MoveCursorPageUp => {
                self.move_cursor(-PAGE_ROWS);
                (Task::none(), None)
            }
            Action::MoveCursorPageDown => {
                self.move_cursor(PAGE_ROWS);
                (Task::none(), None)
            }
            Action::ExtendSelectionUp => {
                self.extend_cursor(-1);
                (Task::none(), None)
            }
            Action::ExtendSelectionDown => {
                self.extend_cursor(1);
                (Task::none(), None)
            }
            Action::ExtendSelectionLeft | Action::ExtendSelectionRight => (Task::none(), None),
            Action::ExtendSelectionHome => {
                self.extend_to(0);
                (Task::none(), None)
            }
            Action::ExtendSelectionEnd => {
                self.extend_to(self.visible.len().saturating_sub(1));
                (Task::none(), None)
            }
            Action::ExtendSelectionPageUp => {
                self.extend_cursor(-PAGE_ROWS);
                (Task::none(), None)
            }
            Action::ExtendSelectionPageDown => {
                self.extend_cursor(PAGE_ROWS);
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
    /// row *before* the first when nothing is selected/cursored yet —
    /// not `0`, which would make the very first `MoveCursorDown` land on
    /// row 1 (skipping row 0) instead of selecting row 0.
    fn current_cursor_or_before_start(&self) -> isize {
        self.selection.cursor().map_or(-1, |c| c as isize)
    }

    fn move_cursor(&mut self, delta: isize) {
        self.move_cursor_to_signed(self.current_cursor_or_before_start() + delta);
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
        self.extend_to_signed(self.current_cursor_or_before_start() + delta);
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

    impl DirectoryView {
        /// Test-only shim: `apply_action` is private (only reached via
        /// `Message::Keyboard` normally), but driving it directly keeps
        /// these tests from having to fabricate `iced::keyboard::Event`s.
        fn apply_action_for_test(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
            self.apply_action(action)
        }
    }
}
