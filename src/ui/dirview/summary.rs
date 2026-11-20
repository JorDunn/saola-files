//! What the status bar needs to know about one view, kept ready so that
//! drawing the bar never does work proportional to the folder size.
//!
//! **Where the selection size is computed.** Not in `view` (which runs every
//! frame) but at the end of [`DirectoryView::update`](super::DirectoryView),
//! and only when something that can change the answer happened: the
//! selection's [`revision`](super::selection::Selection::revision) moved, or
//! a message that can change `entries` (a listing, a watch batch, a rename or
//! create result) was handled. The result is cached in [`Cache`]; `view`
//! reads it in O(1). Rebuilding is one pass over `entries` with a hash
//! lookup per row, so selecting 100,000 rows costs one pass per selection
//! *change* (a click, a Shift+arrow), never per frame, and scrolling or
//! hovering costs nothing.
//!
//! **Folders.** A directory's size is not in a listing; finding it means a
//! recursive walk, which the status bar must never start. So the byte total
//! is the sum of the selected *non-folders*, and [`SelectionTotals::folders`]
//! lets the wording say so ("12.4 MB in files").

use std::collections::HashSet;
use std::ffi::OsString;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::Location;

use super::Message;

/// Counts and bytes for the rows that are both selected and present in the
/// current listing. Counted from `entries`, not from the selection set: a
/// name can stay selected after the file vanished (a watched delete in a
/// listing that has not caught up), and the bar should count only what
/// exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectionTotals {
    /// Selected rows that are not folders (files, symlinks, special files).
    pub files: usize,
    /// Selected folders.
    pub folders: usize,
    /// Sum of `size` over the selected non-folders. Folders add nothing:
    /// their size is unknown without a recursive walk.
    pub bytes: u64,
}

impl SelectionTotals {
    pub fn count(&self) -> usize {
        self.files + self.folders
    }

    fn add(&mut self, entry: &FileEntry) {
        if entry.kind == EntryKind::Directory {
            self.folders += 1;
        } else {
            self.files += 1;
            // Saturating: a total past u64::MAX is not reachable, but the
            // no-panic rule says an overflow must not be able to panic.
            self.bytes = self.bytes.saturating_add(entry.size);
        }
    }

    /// One pass over `rows`, counting those whose name is in `selected`.
    /// The caller passes only the rows on screen: a name can stay selected
    /// while the hidden filter holds its row back, and the bar must agree
    /// with the "of N" it prints (`N` is the visible count).
    pub fn from_entries<'a>(
        rows: impl Iterator<Item = &'a FileEntry>,
        selected: &HashSet<&OsString>,
    ) -> Self {
        let mut totals = Self::default();
        if selected.is_empty() {
            return totals;
        }
        for entry in rows.filter(|e| selected.contains(&e.name)) {
            totals.add(entry);
        }
        totals
    }
}

/// How far the view's listing has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A listing is in flight.
    Loading,
    /// The last listing failed; there are no rows to count.
    Failed,
    /// Rows are on screen.
    Listed,
}

/// Everything the bar's left side is made of, as plain numbers. Built by
/// [`DirectoryView::summary`](super::DirectoryView::summary) and turned into
/// words by `ui::statusbar`, which keeps the wording testable without a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewSummary {
    pub phase: Phase,
    /// Rows on screen (after the hidden filter).
    pub visible: usize,
    /// Entries the hidden-files filter is currently holding back.
    pub hidden: usize,
    pub selection: SelectionTotals,
}

/// The cached totals plus the stamps that say when they are stale.
#[derive(Debug, Default)]
pub struct Cache {
    totals: SelectionTotals,
    /// The selection revision `totals` was built for.
    seen_revision: u64,
    /// Bumped whenever a listing or watch batch lands. The app compares it
    /// before and after `update` to know a free-space refresh is due.
    refresh_epoch: u64,
    /// Set by `recompute_visible`: the set of rows on screen changed (a
    /// listing, a sort, the hidden toggle), so `totals` must be rebuilt.
    visible_changed: bool,
    /// The location a listing last succeeded for. While a reload of that
    /// same folder is in flight the bar keeps its counts instead of
    /// blanking to "Loading…".
    listed_at: Option<Location>,
}

impl Cache {
    pub(super) fn mark_visible_changed(&mut self) {
        self.visible_changed = true;
    }
}

/// Messages after which the free space may have changed: a listing landed
/// (navigation, F5, reload after an operation) or a watch batch arrived.
pub(super) fn is_refresh_signal(message: &Message) -> bool {
    matches!(
        message,
        Message::Listed(..)
            | Message::TargetResolved(..)
            | Message::Watch(..)
            | Message::WatchRefreshed(..)
    )
}

/// Messages that can change `entries` (so sizes/counts) without touching
/// the selection.
pub(super) fn may_change_entries(message: &Message) -> bool {
    is_refresh_signal(message)
        || matches!(
            message,
            Message::RenameResult(..) | Message::CreateResult(..)
        )
}

impl super::DirectoryView {
    /// Called once at the end of `update`. Cheap when nothing relevant
    /// happened: two integer comparisons.
    pub(super) fn sync_summary(&mut self, entries_may_have_changed: bool, refresh_signal: bool) {
        if refresh_signal {
            self.summary.refresh_epoch = self.summary.refresh_epoch.wrapping_add(1);
        }
        // Remember which folder has a listing on screen (see `listed_at`).
        if self.error.is_some() {
            self.summary.listed_at = None;
        } else if self.listed && self.summary.listed_at.as_ref() != Some(&self.location) {
            self.summary.listed_at = Some(self.location.clone());
        }
        let revision = self.selection.revision();
        if entries_may_have_changed
            || self.summary.visible_changed
            || revision != self.summary.seen_revision
        {
            let selected: HashSet<&OsString> = self.selection.selected_names().collect();
            let rows = self.visible.iter().filter_map(|&i| self.entries.get(i));
            self.summary.totals = SelectionTotals::from_entries(rows, &selected);
            self.summary.seen_revision = revision;
            self.summary.visible_changed = false;
        }
    }

    /// The bar's left-hand facts for this view. O(1): everything is cached.
    pub fn summary(&self) -> ViewSummary {
        // "Loading…" only when there is nothing for this folder to show yet
        // (first load, navigation); a reload of the folder already listed
        // keeps its counts until the new listing lands.
        let has_listing = self.summary.listed_at.as_ref() == Some(&self.location);
        let phase = if self.loading && !has_listing {
            Phase::Loading
        } else if self.error.is_some() {
            Phase::Failed
        } else {
            Phase::Listed
        };
        ViewSummary {
            phase,
            visible: self.visible.len(),
            hidden: self.entries.len().saturating_sub(self.visible.len()),
            selection: self.summary.totals,
        }
    }

    /// Changes whenever a listing or watch batch lands; see [`Cache`].
    pub fn refresh_epoch(&self) -> u64 {
        self.summary.refresh_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::vfs::Location;

    fn sized(name: &str, size: u64) -> FileEntry {
        FileEntry { size, ..file(name) }
    }

    #[test]
    fn totals_sum_files_and_count_folders_without_sizing_them() {
        let entries = [sized("a", 100), sized("b", 50), dir("d")];
        let (a, b, d) = (
            OsString::from("a"),
            OsString::from("b"),
            OsString::from("d"),
        );
        let selected: HashSet<&OsString> = [&a, &b, &d].into_iter().collect();
        let totals = SelectionTotals::from_entries(entries.iter(), &selected);
        assert_eq!(
            totals,
            SelectionTotals {
                files: 2,
                folders: 1,
                bytes: 150
            }
        );
        assert_eq!(totals.count(), 3);
    }

    #[test]
    fn a_selected_name_that_no_longer_exists_is_not_counted() {
        let entries = [sized("a", 100)];
        let (a, gone) = (OsString::from("a"), OsString::from("gone"));
        let selected: HashSet<&OsString> = [&a, &gone].into_iter().collect();
        assert_eq!(
            SelectionTotals::from_entries(entries.iter(), &selected).count(),
            1
        );
    }

    #[test]
    fn totals_follow_every_selection_change_immediately() {
        let mut view = listed_view(vec![sized("a", 100), sized("b", 50), sized("c", 7)]);
        assert_eq!(view.summary().selection, SelectionTotals::default());

        let _ = view.update(Message::RowClicked(0));
        assert_eq!(view.summary().selection.bytes, 100);

        view.modifiers = iced::keyboard::Modifiers::CTRL;
        let _ = view.update(Message::RowClicked(2));
        assert_eq!(view.summary().selection.count(), 2);
        assert_eq!(view.summary().selection.bytes, 107);

        // Ctrl+click again takes it back out: the total goes back down.
        let _ = view.update(Message::RowClicked(2));
        assert_eq!(view.summary().selection.bytes, 100);
    }

    #[test]
    fn totals_are_rebuilt_only_when_the_selection_or_listing_changed() {
        let mut view = listed_view(vec![sized("a", 100)]);
        let _ = view.update(Message::RowClicked(0));
        // Corrupt the cache, then run the no-change path: it must not
        // rebuild (a scroll is neither a selection nor an entries change).
        view.summary.totals.bytes = 1;
        view.sync_summary(false, false);
        assert_eq!(view.summary().selection.bytes, 1);
        // A change that can alter entries rebuilds it.
        view.sync_summary(true, false);
        assert_eq!(view.summary().selection.bytes, 100);
    }

    #[test]
    fn a_watched_size_change_updates_the_total() {
        let mut view = listed_view(vec![sized("a", 100)]);
        let _ = view.update(Message::RowClicked(0));
        let _ = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("a"), Some(sized("a", 300)))],
        ));
        assert_eq!(view.summary().selection.bytes, 300);
    }

    #[test]
    fn counts_hidden_entries_and_phase() {
        let mut view = listed_view(vec![file("a"), file(".h1"), file(".h2")]);
        let summary = view.summary();
        assert_eq!((summary.visible, summary.hidden), (1, 2));
        assert_eq!(summary.phase, Phase::Listed);

        // A reload of an already-listed folder keeps its counts.
        view.loading = true;
        assert_eq!(view.summary().phase, Phase::Listed);
        view.loading = false;
        view.error = Some(crate::core::vfs::VfsError::Other {
            message: "x".into(),
        });
        assert_eq!(view.summary().phase, Phase::Failed);
    }

    #[test]
    fn only_listing_and_watch_messages_signal_a_free_space_refresh() {
        let mut view = listed_view(vec![file("a")]);
        let before = view.refresh_epoch();
        let _ = view.update(Message::RowClicked(0));
        assert_eq!(view.refresh_epoch(), before, "a click is not a signal");
        let _ = view.update(Message::Listed(Location::local("/home"), Ok(vec![])));
        assert_ne!(view.refresh_epoch(), before, "a listing is");
    }

    #[test]
    fn rows_hidden_by_the_filter_are_not_counted_or_summed() {
        use crate::keymap::Action;
        let mut view = listed_view(vec![
            sized("a", 1),
            sized("b", 2),
            sized("c", 4),
            sized(".h1", 100),
            sized(".h2", 200),
        ]);
        let _ = view.update(Message::Action(Action::ToggleHidden)); // show
        let _ = view.update(Message::Action(Action::SelectAll));
        assert_eq!(view.summary().selection.count(), 5);
        assert_eq!(view.summary().selection.bytes, 307);

        // Hide again: the dotfiles stay selected, but the bar must read
        // "3 of 3", not "5 of 3", and sum only what is on screen.
        let _ = view.update(Message::Action(Action::ToggleHidden));
        let summary = view.summary();
        assert_eq!(summary.visible, 3);
        assert_eq!(summary.selection.count(), 3);
        assert_eq!(summary.selection.bytes, 7);
    }

    #[test]
    fn a_reload_of_the_shown_folder_keeps_counts_but_a_first_load_does_not() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        // F5 on the folder already shown.
        view.loading = true;
        assert_eq!(view.summary().phase, Phase::Listed);
        assert_eq!(view.summary().visible, 2);
        view.loading = false;

        // Navigation: a different folder with nothing listed yet.
        let _ = view.navigate(Location::local("/elsewhere"));
        assert_eq!(view.summary().phase, Phase::Loading);
    }
}
