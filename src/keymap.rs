//! `(Key, Modifiers) -> Action` resolution.
//!
//! CLAUDE.md's messages rule: "keyboard input resolves through `keymap.rs`
//! to an `Action` enum — `update` consumes Actions, never raw key events."
//! [`resolve`] is the one place raw `iced::keyboard::Key`/`Modifiers`
//! values get matched on; everything downstream (`ui::dirview`'s `update`)
//! only ever sees an [`Action`].

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

/// A keyboard command the active directory view can act on. Deliberately
/// UI-shaped ("move the cursor down"), not "what key was pressed" — no
/// caller past [`resolve`] sees a `Key`/`Modifiers` again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MoveCursorUp,
    MoveCursorDown,
    /// Grid view's column stepping (Stage 4: `ui::dirview::grid`); a no-op
    /// in list view, which has no notion of "the next column over".
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorHome,
    MoveCursorEnd,
    MoveCursorPageUp,
    MoveCursorPageDown,
    /// Same keys with Shift held: extend the selection to the new cursor
    /// position (anchored at the last plain click/move) instead of moving
    /// a bare cursor.
    ExtendSelectionUp,
    ExtendSelectionDown,
    ExtendSelectionLeft,
    ExtendSelectionRight,
    ExtendSelectionHome,
    ExtendSelectionEnd,
    ExtendSelectionPageUp,
    ExtendSelectionPageDown,
    /// Ctrl+Space: toggle the cursor row's membership without touching the
    /// rest of the selection. Kept distinct from Ctrl+click, which
    /// `ui::dirview::selection` handles directly from the click message.
    ToggleCursorSelected,
    SelectAll,
    /// Enter: open the selection (descend into a lone selected directory,
    /// or activate the selected file(s)).
    Descend,
    /// Alt+Up or Backspace: navigate to the parent directory.
    Ascend,
    ToggleHidden,
    /// Alt+Left: step back through this view's own history — the
    /// browser-style back button (`DirectoryView`'s back/forward stacks,
    /// Stage 4). Handled entirely inside the view (unlike `Ascend`, which
    /// bubbles an `Event::OpenDirectory` for the owner to act on): back/
    /// forward is inherently per-view, never a "maybe open in a new tab"
    /// situation the way descending into a row is.
    HistoryBack,
    /// Alt+Right: the redo side of [`Action::HistoryBack`].
    HistoryForward,
    /// Ctrl+L: swap the breadcrumb trail for an editable path/URI
    /// `text_input`, focused with its text selected.
    EditPath,
    /// Ctrl+1: switch this view to the list presentation.
    SetViewList,
    /// Ctrl+2: switch this view to the grid presentation.
    SetViewGrid,
    /// F5: re-list the current directory — the manual fallback for
    /// backends that can't signal changes themselves (`Caps::WATCH`
    /// unset).
    Refresh,
}

/// Resolve one key press into an [`Action`], or `None` if this module
/// doesn't own that combination — the caller falls through to its own
/// handling (or does nothing).
pub fn resolve(key: &Key, modifiers: Modifiers) -> Option<Action> {
    let shift = modifiers.shift();
    let ctrl = modifiers.control();
    let alt = modifiers.alt();

    // Ctrl+<letter>/Space combinations, no other modifier riding along.
    if ctrl && !shift && !alt {
        if let Key::Character(c) = key {
            match c.as_str() {
                "a" => return Some(Action::SelectAll),
                "h" => return Some(Action::ToggleHidden),
                "l" => return Some(Action::EditPath),
                "1" => return Some(Action::SetViewList),
                "2" => return Some(Action::SetViewGrid),
                _ => {}
            }
        }
        if matches!(key, Key::Named(Named::Space)) {
            return Some(Action::ToggleCursorSelected);
        }
        return None;
    }

    // Alt+<arrow>, no other modifier riding along: browser-style history
    // navigation, plus the pre-existing Alt+Up ascend synonym for plain
    // Backspace (handled below).
    if alt && !ctrl && !shift {
        return match key {
            Key::Named(Named::ArrowUp) => Some(Action::Ascend),
            Key::Named(Named::ArrowLeft) => Some(Action::HistoryBack),
            Key::Named(Named::ArrowRight) => Some(Action::HistoryForward),
            _ => None,
        };
    }

    // Every remaining action is a named key with at most Shift held.
    if ctrl || alt {
        return None;
    }
    let Key::Named(named) = key else {
        return None;
    };
    match (named, shift) {
        (Named::ArrowUp, false) => Some(Action::MoveCursorUp),
        (Named::ArrowUp, true) => Some(Action::ExtendSelectionUp),
        (Named::ArrowDown, false) => Some(Action::MoveCursorDown),
        (Named::ArrowDown, true) => Some(Action::ExtendSelectionDown),
        (Named::ArrowLeft, false) => Some(Action::MoveCursorLeft),
        (Named::ArrowLeft, true) => Some(Action::ExtendSelectionLeft),
        (Named::ArrowRight, false) => Some(Action::MoveCursorRight),
        (Named::ArrowRight, true) => Some(Action::ExtendSelectionRight),
        (Named::Home, false) => Some(Action::MoveCursorHome),
        (Named::Home, true) => Some(Action::ExtendSelectionHome),
        (Named::End, false) => Some(Action::MoveCursorEnd),
        (Named::End, true) => Some(Action::ExtendSelectionEnd),
        (Named::PageUp, false) => Some(Action::MoveCursorPageUp),
        (Named::PageUp, true) => Some(Action::ExtendSelectionPageUp),
        (Named::PageDown, false) => Some(Action::MoveCursorPageDown),
        (Named::PageDown, true) => Some(Action::ExtendSelectionPageDown),
        (Named::Enter, false) => Some(Action::Descend),
        (Named::Backspace, false) => Some(Action::Ascend),
        (Named::F5, false) => Some(Action::Refresh),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(key: Named) -> Key {
        Key::Named(key)
    }

    #[test]
    fn plain_arrows_move_the_cursor() {
        assert_eq!(
            resolve(&named(Named::ArrowDown), Modifiers::empty()),
            Some(Action::MoveCursorDown)
        );
        assert_eq!(
            resolve(&named(Named::ArrowUp), Modifiers::empty()),
            Some(Action::MoveCursorUp)
        );
    }

    #[test]
    fn shift_arrows_extend_selection() {
        assert_eq!(
            resolve(&named(Named::ArrowDown), Modifiers::SHIFT),
            Some(Action::ExtendSelectionDown)
        );
    }

    #[test]
    fn enter_descends_backspace_ascends() {
        assert_eq!(
            resolve(&named(Named::Enter), Modifiers::empty()),
            Some(Action::Descend)
        );
        assert_eq!(
            resolve(&named(Named::Backspace), Modifiers::empty()),
            Some(Action::Ascend)
        );
    }

    #[test]
    fn alt_up_also_ascends() {
        assert_eq!(
            resolve(&named(Named::ArrowUp), Modifiers::ALT),
            Some(Action::Ascend)
        );
    }

    #[test]
    fn ctrl_a_selects_all_ctrl_h_toggles_hidden() {
        assert_eq!(
            resolve(&Key::Character("a".into()), Modifiers::CTRL),
            Some(Action::SelectAll)
        );
        assert_eq!(
            resolve(&Key::Character("h".into()), Modifiers::CTRL),
            Some(Action::ToggleHidden)
        );
    }

    #[test]
    fn ctrl_space_toggles_cursor_row() {
        assert_eq!(
            resolve(&named(Named::Space), Modifiers::CTRL),
            Some(Action::ToggleCursorSelected)
        );
    }

    #[test]
    fn unmapped_keys_resolve_to_none() {
        assert_eq!(resolve(&named(Named::Tab), Modifiers::empty()), None);
        assert_eq!(resolve(&Key::Character("x".into()), Modifiers::CTRL), None);
    }

    #[test]
    fn alt_left_right_step_through_history() {
        assert_eq!(
            resolve(&named(Named::ArrowLeft), Modifiers::ALT),
            Some(Action::HistoryBack)
        );
        assert_eq!(
            resolve(&named(Named::ArrowRight), Modifiers::ALT),
            Some(Action::HistoryForward)
        );
    }

    #[test]
    fn ctrl_l_edits_the_path_ctrl_1_2_switch_view() {
        assert_eq!(
            resolve(&Key::Character("l".into()), Modifiers::CTRL),
            Some(Action::EditPath)
        );
        assert_eq!(
            resolve(&Key::Character("1".into()), Modifiers::CTRL),
            Some(Action::SetViewList)
        );
        assert_eq!(
            resolve(&Key::Character("2".into()), Modifiers::CTRL),
            Some(Action::SetViewGrid)
        );
    }

    #[test]
    fn f5_refreshes() {
        assert_eq!(
            resolve(&named(Named::F5), Modifiers::empty()),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn alt_shift_arrow_is_unmapped() {
        // The alt-only block above must not swallow Alt+Shift combos —
        // they fall through to the generic "ctrl || alt => None" guard.
        assert_eq!(
            resolve(&named(Named::ArrowLeft), Modifiers::ALT | Modifiers::SHIFT),
            None
        );
    }
}
