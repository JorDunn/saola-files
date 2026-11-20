//! The context menu and Open-with popover: their own message enum
//! ([`Message`], nested in the view's as `dirview::Message::Menu`), its
//! handlers, and the selection-to-event helpers they share with the
//! keyboard path (`activate_selection`, `selection_targets`,
//! `properties_event`, `terminal_target`).

use iced::Task;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::Location;

use super::rename::NewKind;
use super::{DeleteMode, DirectoryView, Event};

/// The view's own message type, named once here: inside this file plain
/// `Message` means the menu's enum below, so the handlers' return type
/// (`Task<ViewMessage>`) needs a distinct name for the outer one.
type ViewMessage = super::Message;

/// Everything the context menu and the Open-with popover can send. Nested
/// in the view's own message as `dirview::Message::Menu(menu::Message)`, so
/// a feature that adds a menu entry adds one variant here (under the
/// marker) instead of widening `dirview::Message`.
///
/// `ui::menus` builds these (it renders both popovers); the header's
/// overflow button sends [`Message::Open`].
#[derive(Debug, Clone)]
pub enum Message {
    /// The header's overflow button: opens the context menu.
    Open,
    /// The scrim, or a menu item that finished its job: closes whichever
    /// of the context menu/Open-with popover is open.
    Close,
    /// "Open" — the same activation double-click/Enter already drive,
    /// reached from the menu instead.
    OpenSelected,
    /// "Open with…": swaps the context menu for the Open-with popover.
    OpenWithRequested,
    /// "Open in terminal".
    OpenTerminalRequested,
    /// A config-defined `[[action]]`, by its index into
    /// `DirectoryView::actions`.
    CustomActionRequested(usize),
    /// An app chosen in the Open-with popover, by its desktop-id.
    OpenWithChosen(String),
    /// The menu equivalents of `Action::Copy`/`Cut`/`Paste` — mouse parity
    /// for the Ctrl+C/X/V keyboard path. Each closes the menu first.
    CopyRequested,
    CutRequested,
    PasteRequested,
    /// "Rename…" — the mouse path to the same `start_rename_named` F2 uses.
    RenameRequested,
    NewFolderRequested,
    NewFileRequested,
    /// "Delete"/"Move to Trash" — always `DeleteMode::ToTrash`; the menu
    /// never offers a permanent-delete row of its own, that's
    /// Shift+Delete's job (see `ui::menus`'s doc comment on why one
    /// capability-honestly-worded row is enough).
    DeleteRequested,
    /// "Properties" — the mouse path to the same dialog Alt+Enter opens
    /// (`Action::Properties`).
    PropertiesRequested,
    // --- features (alphabetical) ---
}

impl DirectoryView {
    /// `update`'s `dirview::Message::Menu` arm: one line per menu message,
    /// each delegating to its handler below.
    pub(super) fn on_menu(&mut self, message: Message) -> (Task<ViewMessage>, Option<Event>) {
        match message {
            Message::Open => self.on_open_menu(),
            Message::Close => self.on_close_menu(),
            Message::OpenSelected => self.on_menu_open_selected(),
            Message::OpenWithRequested => self.on_menu_open_with_requested(),
            Message::OpenTerminalRequested => self.on_menu_open_terminal_requested(),
            Message::CustomActionRequested(index) => self.on_menu_custom_action_requested(index),
            Message::OpenWithChosen(desktop_id) => self.on_open_with_chosen(desktop_id),
            Message::CopyRequested => self.on_menu_copy_requested(),
            Message::CutRequested => self.on_menu_cut_requested(),
            Message::PasteRequested => self.on_menu_paste_requested(),
            Message::RenameRequested => self.on_menu_rename_requested(),
            Message::NewFolderRequested => self.on_menu_new_folder_requested(),
            Message::NewFileRequested => self.on_menu_new_file_requested(),
            Message::DeleteRequested => self.on_menu_delete_requested(),
            Message::PropertiesRequested => self.on_menu_properties_requested(),
            // --- features (alphabetical) ---
        }
    }

    /// [`Message::Open`]'s handler.
    pub(super) fn on_open_menu(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = true;
        self.open_with_open = false;
        (Task::none(), None)
    }

    /// [`Message::Close`]'s handler.
    pub(super) fn on_close_menu(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        self.open_with_open = false;
        (Task::none(), None)
    }

    /// [`Message::OpenSelected`]'s handler.
    pub(super) fn on_menu_open_selected(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (Task::none(), self.activate_selection())
    }

    /// [`Message::OpenWithRequested`]'s handler.
    pub(super) fn on_menu_open_with_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        self.open_with_open = true;
        (Task::none(), None)
    }

    /// [`Message::OpenTerminalRequested`]'s handler.
    pub(super) fn on_menu_open_terminal_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (Task::none(), Some(self.terminal_target()))
    }

    /// [`Message::CustomActionRequested`]'s handler.
    pub(super) fn on_menu_custom_action_requested(
        &mut self,
        index: usize,
    ) -> (Task<ViewMessage>, Option<Event>) {
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

    /// [`Message::OpenWithChosen`]'s handler.
    pub(super) fn on_open_with_chosen(
        &mut self,
        desktop_id: String,
    ) -> (Task<ViewMessage>, Option<Event>) {
        self.open_with_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::OpenWith(targets, desktop_id)))
    }

    /// [`Message::CopyRequested`]'s handler.
    pub(super) fn on_menu_copy_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::CopyRequested(targets)))
    }

    /// [`Message::CutRequested`]'s handler.
    pub(super) fn on_menu_cut_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::CutRequested(targets)))
    }

    /// [`Message::PasteRequested`]'s handler.
    pub(super) fn on_menu_paste_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (
            Task::none(),
            Some(Event::PasteRequested(self.location.clone())),
        )
    }

    /// [`Message::RenameRequested`]'s handler.
    pub(super) fn on_menu_rename_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        let selected = self.selected_entries();
        if let [entry] = selected.as_slice() {
            let name = entry.name.clone();
            (self.start_rename_named(name), None)
        } else {
            (Task::none(), None)
        }
    }

    /// [`Message::NewFolderRequested`]'s handler.
    pub(super) fn on_menu_new_folder_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (self.create_new(NewKind::Folder), None)
    }

    /// [`Message::NewFileRequested`]'s handler.
    pub(super) fn on_menu_new_file_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (self.create_new(NewKind::File), None)
    }

    /// [`Message::DeleteRequested`]'s handler.
    pub(super) fn on_menu_delete_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
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

    /// [`Message::PropertiesRequested`]'s handler.
    pub(super) fn on_menu_properties_requested(&mut self) -> (Task<ViewMessage>, Option<Event>) {
        self.menu_open = false;
        (Task::none(), self.properties_event())
    }

    /// The rows Enter/double-click should act on: the current selection,
    /// falling back to the cursor row alone if nothing's selected. A lone
    /// selected directory descends into it; anything else (files, or a
    /// mixed/multi selection) activates the whole set.
    pub(super) fn activate_selection(&self) -> Option<Event> {
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
    pub(super) fn selection_targets(&self) -> Vec<Location> {
        self.selection
            .selected_names()
            .map(|name| self.location.join(name))
            .collect()
    }

    /// `Event::PropertiesRequested`'s payload for the current selection —
    /// shared by `Action::Properties` (Alt+Enter) and
    /// `menu::Message::PropertiesRequested` (the context menu row), the same
    /// "one behavior, two input paths" split every other action/menu-row
    /// pair in this module already takes. `None` when nothing is selected
    /// — there is nothing to show properties for.
    pub(super) fn properties_event(&self) -> Option<Event> {
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
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::keymap::Action;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn enter_on_an_unselected_cursor_row_still_finds_its_sniffed_type() {
        use crate::core::mime::MimeDb;
        use std::time::{Duration, SystemTime};

        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
        // List without an mtime (a listing with one bubbles thumbnail
        // requests, which `listed_view` forbids), then stamp it.
        let mut view = listed_view(vec![file("picture")]);
        if let Some(entry) = view.entries.first_mut() {
            entry.modified = Some(mtime);
        }
        // Cursor sits on the row without selecting it (as after a click
        // that was then cleared, or Ctrl+arrow navigation).
        view.selection.set_cursor(Some(0));
        assert_eq!(view.selected_count(), 0);

        let db = MimeDb::new();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        db.record_sniff(view.location(), OsStr::new("picture"), mtime, png);

        let Some(Event::Activated(locations)) = view.activate_selection() else {
            panic!("expected Activated");
        };
        let name = locations[0].path.file_name().unwrap();
        // The mtime comes from the view, not the (empty) selection.
        let found = db.guess_entry(view.location(), name, view.modified_of(name));
        assert_eq!(found, "image/png");
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

    // ── Menu message nesting ─────────────────────────────────────────────

    #[test]
    fn nested_open_and_close_messages_toggle_the_menu() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(ViewMessage::Menu(Message::Open));
        assert!(view.menu_open());
        let _ = view.update(ViewMessage::Menu(Message::Close));
        assert!(!view.menu_open());
    }

    #[test]
    fn nested_open_with_request_swaps_the_menu_for_the_popover() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(ViewMessage::Menu(Message::Open));
        let _ = view.update(ViewMessage::Menu(Message::OpenWithRequested));
        assert!(!view.menu_open());
        assert!(view.open_with_open());
    }

    #[test]
    fn nested_copy_with_a_selection_bubbles_copy_requested() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.update(ViewMessage::Menu(Message::Open));
        let (_, event) = view.update(ViewMessage::Menu(Message::CopyRequested));
        assert!(!view.menu_open());
        match event {
            Some(Event::CopyRequested(locations)) => {
                assert_eq!(locations, vec![Location::local("/home/a")]);
            }
            other => panic!("expected CopyRequested, got {other:?}"),
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
        let (_, event) = view.update(ViewMessage::Menu(Message::PropertiesRequested));
        assert!(!view.menu_open());
        match event {
            Some(Event::PropertiesRequested(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected PropertiesRequested, got {other:?}"),
        }
    }
}
