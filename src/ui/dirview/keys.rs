//! Keyboard input: raw `keyboard::Event`s (`handle_keyboard`), type-ahead
//! for unbound printable keys, and `apply_action` — the one place every
//! keymap [`Action`] (from a key or a header button) is carried out.

use iced::{Task, keyboard};

use crate::config::View;
use crate::keymap::Action;
use crate::ui::breadcrumbs;

use super::rename::NewKind;
use super::{DeleteMode, DirectoryView, Event, Message};

/// Cursor rows a single PageUp/PageDown moves — a fixed placeholder rather
/// than the real viewport-height/row-height math, which needs a theme
/// lookup `update()` doesn't otherwise take. Stage 4's navigation-chrome
/// pass can wire this to `DirectoryView::scroll` once it threads sizing
/// through; the keymap contract (`Action::MoveCursorPageUp/Down`) already
/// doesn't change either way.
const PAGE_ROWS: isize = 10;

impl DirectoryView {
    /// True while this view has something its own Escape handling would
    /// close or cancel (the menu, the Open-with popover, an inline rename,
    /// the path editor). `App` asks before using Escape to dismiss the
    /// error notice, so one press closes the thing in front of the person,
    /// not the notice behind it.
    pub fn owns_escape(&self) -> bool {
        self.menu_open || self.open_with_open || self.rename.is_some() || self.path_edit.is_some()
    }

    // ── keyboard/action handling ────────────────────────────────────────

    pub(super) fn handle_keyboard(
        &mut self,
        event: keyboard::Event,
    ) -> (Task<Message>, Option<Event>) {
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
                match self.keymap.resolve(&key, modifiers) {
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

    /// A key the keymap doesn't own: if it's an unmodified printable
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

    pub(super) fn apply_action(&mut self, action: Action) -> (Task<Message>, Option<Event>) {
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
                    // Several selected: the bulk-rename dialog (`None` when
                    // nothing is selected, so F2 then does nothing).
                    (Task::none(), self.bulk_rename_event())
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

            // App-level commands are not this view's to carry out: pass
            // them up unchanged for `App` to handle.
            Action::App(app_action) => (Task::none(), Some(Event::App(app_action))),
            // --- features (alphabetical) ---
            Action::OpenContextMenu => {
                self.open_context_menu_at_cursor();
                (Task::none(), None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::grid;
    use super::super::test_support::*;
    use super::*;
    use crate::core::vfs::Location;
    use std::ffi::OsStr;

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
    fn ascend_requests_the_parent_directory() {
        let view = listed_view(vec![]);
        let mut view = view;
        let (_, event) = view.apply_action_for_test(Action::Ascend);
        match event {
            Some(Event::OpenDirectory(location)) => assert_eq!(location, Location::local("/")),
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
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
}
