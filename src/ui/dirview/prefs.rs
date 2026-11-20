//! The per-folder settings the session state remembers (view mode, sort
//! key, sort direction): a read accessor and a setter, so `src/app/
//! session.rs` can save and restore them without reaching into private
//! fields.

use crate::core::state::FolderPrefs;

use super::DirectoryView;

impl DirectoryView {
    /// The settings that would be remembered for the folder shown now.
    pub fn prefs(&self) -> FolderPrefs {
        FolderPrefs {
            view: self.view_mode,
            sort: self.sort,
            descending: self.sort_descending,
        }
    }

    /// Whether the current location has produced a real directory listing.
    pub fn has_listed(&self) -> bool {
        self.listed
    }

    /// Whether listing the current location failed (and nothing has
    /// listed since).
    pub fn list_failed(&self) -> bool {
        !self.loading && !self.listed && self.error.is_some()
    }

    /// Applies remembered settings. Re-sorts the rows already on screen;
    /// a listing still in flight sorts by these once it lands.
    pub fn set_prefs(&mut self, prefs: FolderPrefs) {
        if self.prefs() == prefs {
            return;
        }
        let resort = self.sort != prefs.sort || self.sort_descending != prefs.descending;
        self.view_mode = prefs.view;
        self.sort = prefs.sort;
        self.sort_descending = prefs.descending;
        if resort {
            self.recompute_visible();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::config::{SortKey, View};

    #[test]
    fn prefs_round_trip_through_the_view() {
        let mut view = listed_view(vec![file("b"), file("a")]);
        let wanted = FolderPrefs {
            view: View::Grid,
            sort: SortKey::Size,
            descending: true,
        };
        view.set_prefs(wanted);
        assert_eq!(view.prefs(), wanted);
        assert_eq!(view.view_mode(), View::Grid);
    }
}
