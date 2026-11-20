//! Several items selected + F2 (or the menu's "Rename selected…"): hand the
//! owner what the bulk-rename dialog needs. The view never renames anything
//! itself here; it only reports the selection, in the order the rows are
//! showing, and the names that are *not* selected (so the dialog can flag a
//! collision with them). The dialog and the renaming live in
//! `ui::dialogs::bulk_rename` and `app::bulk_rename`.

use std::collections::HashSet;
use std::ffi::OsString;

use crate::core::vfs::Location;

use super::{DirectoryView, Event};

/// What the bulk-rename dialog is opened with.
#[derive(Debug, Clone)]
pub struct BulkRenameRequest {
    /// The folder the view is showing; every name below is inside it.
    pub dir: Location,
    /// The selected names, in the order the view lists them (the current
    /// sort), so numbering follows what the person sees.
    pub selected: Vec<OsString>,
    /// Every other name in the folder, hidden files included.
    pub others: Vec<OsString>,
}

impl DirectoryView {
    /// The event for "rename the selection in bulk": `None` unless two or
    /// more rows are selected (one item keeps the inline rename).
    pub(super) fn bulk_rename_event(&self) -> Option<Event> {
        // Walk `visible` (the sorted, filtered row order) rather than the
        // selection set, which has no order of its own.
        let selected: Vec<OsString> = self
            .visible
            .iter()
            .filter_map(|&index| self.entries.get(index))
            .filter(|entry| self.selection.is_selected(&entry.name))
            .map(|entry| entry.name.clone())
            .collect();
        if selected.len() < 2 {
            return None;
        }
        // A set, not `selected.contains`: that is a linear scan per entry,
        // which freezes the UI thread with a 100 000-item selection.
        let chosen: HashSet<&OsString> = selected.iter().collect();
        let others = self
            .entries
            .iter()
            .filter(|entry| !chosen.contains(&entry.name))
            .map(|entry| entry.name.clone())
            .collect();
        Some(Event::BulkRenameRequested(BulkRenameRequest {
            dir: self.location.clone(),
            selected,
            others,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{file, listed_view};
    use super::*;
    use crate::keymap::Action;

    #[test]
    fn several_selected_rows_bubble_a_request_in_view_order() {
        let mut view = listed_view(vec![file("b.txt"), file("a.txt"), file("c.txt")]);
        let _ = view.apply_action_for_test(Action::SelectAll);
        let (_, event) = view.apply_action_for_test(Action::Rename);
        let Some(Event::BulkRenameRequested(request)) = event else {
            panic!("expected a bulk rename request, got {event:?}");
        };
        // Name sort puts a before b before c, whatever the listing order.
        assert_eq!(
            request.selected,
            vec![
                OsString::from("a.txt"),
                OsString::from("b.txt"),
                OsString::from("c.txt")
            ]
        );
        assert!(request.others.is_empty());
    }

    #[test]
    fn unselected_names_become_collision_candidates() {
        let mut view = listed_view(vec![file("a.txt"), file("b.txt"), file("keep.txt")]);
        let _ = view.apply_action_for_test(Action::SelectAll);
        // Drop `keep.txt` from the selection with a Ctrl+click.
        view.selection.toggle_click(2, OsString::from("keep.txt"));
        let (_, event) = view.apply_action_for_test(Action::Rename);
        let Some(Event::BulkRenameRequested(request)) = event else {
            panic!("expected a bulk rename request, got {event:?}");
        };
        assert_eq!(request.selected.len(), 2);
        assert_eq!(request.others, vec![OsString::from("keep.txt")]);
    }

    #[test]
    fn one_selected_item_keeps_the_inline_rename() {
        let mut view = listed_view(vec![file("a.txt"), file("b.txt")]);
        view.selection.click(0, OsString::from("a.txt"));
        let (_, event) = view.apply_action_for_test(Action::Rename);
        assert!(event.is_none());
        assert!(view.rename.is_some());
    }
}
