//! The context menu and Open-with popover's messages (`Message::Menu*`,
//! `OpenMenu`/`CloseMenu`, `OpenWithChosen`), plus the selection-to-event
//! helpers they share with the keyboard path (`activate_selection`,
//! `selection_targets`, `properties_event`, `terminal_target`).

use iced::Task;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::Location;

use super::rename::NewKind;
use super::{DeleteMode, DirectoryView, Event, Message};

impl DirectoryView {
    /// `update`'s `Message::OpenMenu` arm, moved here verbatim.
    pub(super) fn on_open_menu(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = true;
        self.open_with_open = false;
        (Task::none(), None)
    }

    /// `update`'s `Message::CloseMenu` arm, moved here verbatim.
    pub(super) fn on_close_menu(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        self.open_with_open = false;
        (Task::none(), None)
    }

    /// `update`'s `Message::MenuOpenSelected` arm, moved here verbatim.
    pub(super) fn on_menu_open_selected(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        (Task::none(), self.activate_selection())
    }

    /// `update`'s `Message::MenuOpenWithRequested` arm, moved here verbatim.
    pub(super) fn on_menu_open_with_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        self.open_with_open = true;
        (Task::none(), None)
    }

    /// `update`'s `Message::MenuOpenTerminalRequested` arm, moved here verbatim.
    pub(super) fn on_menu_open_terminal_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        (Task::none(), Some(self.terminal_target()))
    }

    /// `update`'s `Message::MenuCustomActionRequested` arm, moved here verbatim.
    pub(super) fn on_menu_custom_action_requested(
        &mut self,
        index: usize,
    ) -> (Task<Message>, Option<Event>) {
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

    /// `update`'s `Message::OpenWithChosen` arm, moved here verbatim.
    pub(super) fn on_open_with_chosen(
        &mut self,
        desktop_id: String,
    ) -> (Task<Message>, Option<Event>) {
        self.open_with_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::OpenWith(targets, desktop_id)))
    }

    /// `update`'s `Message::MenuCopyRequested` arm, moved here verbatim.
    pub(super) fn on_menu_copy_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::CopyRequested(targets)))
    }

    /// `update`'s `Message::MenuCutRequested` arm, moved here verbatim.
    pub(super) fn on_menu_cut_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        let targets = self.selection_targets();
        if targets.is_empty() {
            return (Task::none(), None);
        }
        (Task::none(), Some(Event::CutRequested(targets)))
    }

    /// `update`'s `Message::MenuPasteRequested` arm, moved here verbatim.
    pub(super) fn on_menu_paste_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        (
            Task::none(),
            Some(Event::PasteRequested(self.location.clone())),
        )
    }

    /// `update`'s `Message::MenuRenameRequested` arm, moved here verbatim.
    pub(super) fn on_menu_rename_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        let selected = self.selected_entries();
        if let [entry] = selected.as_slice() {
            let name = entry.name.clone();
            (self.start_rename_named(name), None)
        } else {
            (Task::none(), None)
        }
    }

    /// `update`'s `Message::MenuNewFolderRequested` arm, moved here verbatim.
    pub(super) fn on_menu_new_folder_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        (self.create_new(NewKind::Folder), None)
    }

    /// `update`'s `Message::MenuNewFileRequested` arm, moved here verbatim.
    pub(super) fn on_menu_new_file_requested(&mut self) -> (Task<Message>, Option<Event>) {
        self.menu_open = false;
        (self.create_new(NewKind::File), None)
    }

    /// `update`'s `Message::MenuDeleteRequested` arm, moved here verbatim.
    pub(super) fn on_menu_delete_requested(&mut self) -> (Task<Message>, Option<Event>) {
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

    /// `update`'s `Message::MenuPropertiesRequested` arm, moved here verbatim.
    pub(super) fn on_menu_properties_requested(&mut self) -> (Task<Message>, Option<Event>) {
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
    /// `Message::MenuPropertiesRequested` (the context menu row), the same
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
    use std::ffi::OsString;

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
}
