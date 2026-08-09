//! Row selection: a name-keyed set (survives refresh/resort — a rename or
//! re-list keeps the same *entries* highlighted even though their
//! positions shift) plus cursor/anchor indices into `visible` for keyboard
//! navigation and Shift-range selection.
//!
//! [`Selection`] never touches `entries`/`visible` itself — the owner
//! (`DirectoryView`) resolves indices to names before calling in, and
//! resolves names back to rows when rendering. That keeps this module
//! testable with plain names, no `FileEntry`/backend involved.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

#[derive(Debug, Clone, Default)]
pub struct Selection {
    selected: HashSet<OsString>,
    /// Index into `visible` the keyboard cursor sits on. `None` when the
    /// directory is empty or nothing has been clicked/moved to yet.
    cursor: Option<usize>,
    /// Index into `visible` a Shift+click/Shift+arrow range extends from.
    /// Stays put across a range selection; a plain click/move or Ctrl+click
    /// resets it to the new position.
    anchor: Option<usize>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    pub fn is_selected(&self, name: &OsStr) -> bool {
        self.selected.contains(name)
    }

    pub fn selected_names(&self) -> impl Iterator<Item = &OsString> {
        self.selected.iter()
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Clears the selection but leaves the cursor where it is — used when
    /// hidden files toggle or a resort changes `visible` under an
    /// unrelated cursor move, never for a fresh navigation (see
    /// `DirectoryView::navigate`, which resets the whole `Selection`).
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// A plain click (or plain cursor move) landing on `visible[index]`:
    /// select only that row, move the cursor and anchor there.
    pub fn click(&mut self, index: usize, name: OsString) {
        self.selected.clear();
        self.selected.insert(name);
        self.cursor = Some(index);
        self.anchor = Some(index);
    }

    /// Ctrl+click: toggle membership without touching the rest of the
    /// selection; the cursor and anchor both follow to the clicked row
    /// (matching most file managers: the next Shift+click ranges from the
    /// most recent Ctrl+click, not some earlier plain click).
    pub fn toggle_click(&mut self, index: usize, name: OsString) {
        if !self.selected.remove(&name) {
            self.selected.insert(name);
        }
        self.cursor = Some(index);
        self.anchor = Some(index);
    }

    /// Shift+click/Shift+arrow: select exactly `names` (the caller
    /// resolves the anchor..=index range in `visible` to names before
    /// calling), move the cursor to `index`, and leave the anchor as it
    /// was — a second Shift+click re-ranges from the same anchor rather
    /// than the last endpoint.
    pub fn range_select(&mut self, index: usize, names: impl IntoIterator<Item = OsString>) {
        self.selected.clear();
        self.selected.extend(names);
        self.cursor = Some(index);
        if self.anchor.is_none() {
            self.anchor = Some(index);
        }
    }

    pub fn select_all(&mut self, names: impl IntoIterator<Item = OsString>) {
        self.selected.clear();
        self.selected.extend(names);
    }

    /// Move the cursor without changing the selection set or anchor —
    /// used only to clamp the cursor after a resort/refresh drops it out
    /// of range; ordinary cursor movement goes through [`Self::click`].
    pub fn set_cursor(&mut self, cursor: Option<usize>) {
        self.cursor = cursor;
    }

    /// Drops `name` from the selection if it's there — a no-op otherwise.
    /// Used when a watched removal (or the "from" half of a watched
    /// rename) takes an entry out from under the user (Stage 5:
    /// `DirectoryView::apply_watch_events`). Deliberately narrower than
    /// [`Self::clear`]: one entry disappearing shouldn't touch anything
    /// else that's still selected, or the anchor a Shift-range is tracking.
    pub fn forget(&mut self, name: &OsStr) {
        self.selected.remove(name);
    }

    /// A watched rename: if `from` was selected, `to` becomes selected in
    /// its place. This is the whole point of keying `selected` by name
    /// instead of by row index — a rename firing mid-burst doesn't have to
    /// know or care where either name currently sits in `visible` for the
    /// selection to survive it.
    pub fn rename(&mut self, from: &OsStr, to: OsString) {
        if self.selected.remove(from) {
            self.selected.insert(to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn click_replaces_selection_and_moves_cursor_and_anchor() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.click(2, name("c"));
        assert!(!selection.is_selected(&name("a")));
        assert!(selection.is_selected(&name("c")));
        assert_eq!(selection.cursor(), Some(2));
        assert_eq!(selection.anchor(), Some(2));
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn toggle_click_adds_and_removes_without_touching_rest() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.toggle_click(1, name("b"));
        assert!(selection.is_selected(&name("a")));
        assert!(selection.is_selected(&name("b")));
        assert_eq!(selection.len(), 2);

        selection.toggle_click(1, name("b"));
        assert!(!selection.is_selected(&name("b")));
        assert!(selection.is_selected(&name("a")));
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn range_select_keeps_the_original_anchor_across_re_ranges() {
        let mut selection = Selection::new();
        selection.click(1, name("b")); // anchor = 1
        selection.range_select(3, [name("b"), name("c"), name("d")]);
        assert_eq!(selection.anchor(), Some(1));
        assert_eq!(selection.cursor(), Some(3));
        assert_eq!(selection.len(), 3);

        // A second Shift+something re-ranges from the *same* anchor (1),
        // not from the cursor the first range left at 3.
        selection.range_select(0, [name("a"), name("b")]);
        assert_eq!(selection.anchor(), Some(1));
        assert_eq!(selection.cursor(), Some(0));
        assert_eq!(selection.len(), 2);
    }

    #[test]
    fn select_all_replaces_selection() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.select_all([name("a"), name("b"), name("c")]);
        assert_eq!(selection.len(), 3);
    }

    #[test]
    fn selected_names_iterates_the_current_set() {
        let mut selection = Selection::new();
        selection.select_all([name("a"), name("b")]);
        let mut names: Vec<_> = selection.selected_names().cloned().collect();
        names.sort();
        assert_eq!(names, vec![name("a"), name("b")]);
    }

    #[test]
    fn clear_empties_selection_and_anchor_but_not_cursor() {
        let mut selection = Selection::new();
        selection.click(2, name("c"));
        selection.clear();
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), None);
        assert_eq!(selection.cursor(), Some(2));
    }

    #[test]
    fn set_cursor_does_not_touch_selection() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.set_cursor(Some(5));
        assert_eq!(selection.cursor(), Some(5));
        assert!(selection.is_selected(&name("a")));
    }

    // ── Stage 5: watch events ───────────────────────────────────────────

    #[test]
    fn forget_drops_only_the_named_entry() {
        let mut selection = Selection::new();
        selection.select_all([name("a"), name("b")]);
        selection.forget(&name("a"));
        assert!(!selection.is_selected(&name("a")));
        assert!(selection.is_selected(&name("b")));
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn forgetting_an_unselected_name_is_a_no_op() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.forget(&name("z"));
        assert!(selection.is_selected(&name("a")));
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn rename_moves_a_selected_name_to_its_new_name() {
        let mut selection = Selection::new();
        selection.select_all([name("a"), name("b")]);
        selection.rename(&name("a"), name("a-renamed"));
        assert!(!selection.is_selected(&name("a")));
        assert!(selection.is_selected(&name("a-renamed")));
        assert!(selection.is_selected(&name("b")));
        assert_eq!(selection.len(), 2);
    }

    #[test]
    fn renaming_an_unselected_name_does_not_select_the_new_name() {
        let mut selection = Selection::new();
        selection.click(0, name("a"));
        selection.rename(&name("untouched"), name("still-untouched"));
        assert!(!selection.is_selected(&name("still-untouched")));
        assert_eq!(selection.len(), 1);
    }
}
