//! Row order and filtering: `recompute_visible` (hidden filter + sort into
//! `visible`), the dotfile check, and the sort-column header click.

use iced::Task;

use crate::config::SortKey;
use crate::core::fs::entry::FileEntry;
use crate::core::fs::sort;

use super::{DirectoryView, Event, Message};

impl DirectoryView {
    /// `update`'s `Message::HeaderClicked` arm, moved here verbatim.
    pub(super) fn on_header_clicked(&mut self, key: SortKey) -> (Task<Message>, Option<Event>) {
        if self.sort == key {
            self.sort_descending = !self.sort_descending;
        } else {
            self.sort = key;
            self.sort_descending = false;
        }
        self.recompute_visible();
        (Task::none(), self.thumbnail_event())
    }

    /// Rebuild `visible` from `entries` (hidden-filtered, sorted) and try
    /// to keep the keyboard cursor on the same *entry* it was on before —
    /// falling back to clamping its raw index. A view that never had a
    /// cursor stays without one (`None`) rather than inventing row 0 —
    /// the very first arrow-key press does that (see
    /// `current_cursor_or_before_start`), the same way a fresh view with
    /// no clicks yet has nothing selected.
    pub(super) fn recompute_visible(&mut self) {
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
}

/// `entries[i].name` starts with a dot — the one place `DirectoryView`
/// checks a name's bytes directly rather than going through
/// `display_name`, since a leading-dot check doesn't need a valid string
/// (a non-UTF-8 name can still start with an ASCII `.`).
fn is_hidden(entry: &FileEntry) -> bool {
    use std::os::unix::ffi::OsStrExt;
    entry.name.as_bytes().first() == Some(&b'.')
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::vfs::Location;
    use crate::keymap::Action;

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
}
