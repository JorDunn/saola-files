//! Mouse row clicks: plain/Ctrl/Shift selection and app-side
//! double-click pairing (`is_double_click`).

use iced::Task;

use super::{DirectoryView, Event, Message};

impl DirectoryView {
    /// `update`'s `Message::RowClicked` arm, moved here verbatim.
    pub(super) fn on_row_clicked(&mut self, index: usize) -> (Task<Message>, Option<Event>) {
        let Some(name) = self.entry_at(index).map(|entry| entry.name.clone()) else {
            return (Task::none(), None);
        };
        if self.modifiers.control() {
            self.last_click = None;
            self.selection.toggle_click(index, name);
            (Task::none(), None)
        } else if self.modifiers.shift() {
            self.last_click = None;
            self.extend_to(index);
            (Task::none(), None)
        } else {
            // The pairing *decision* is the pure `is_double_click`
            // (unit-tested with synthetic instants); this arm is
            // the thin wrapper that supplies the real clock — the
            // same split CLAUDE.md prescribes for env lookups.
            let now = std::time::Instant::now();
            let is_double = is_double_click(self.last_click, index, now);
            self.selection.click(index, name);
            if is_double {
                // Consumed: a third quick click starts a fresh
                // pair rather than chaining activations.
                self.last_click = None;
                (Task::none(), self.activate_selection())
            } else {
                self.last_click = Some((index, now));
                (Task::none(), None)
            }
        }
    }
}

/// Two plain clicks this close together (and on the same row) are a
/// double-click. 300 ms is iced's own consecutive-click window —
/// `iced_core::mouse::Click::is_consecutive` uses `<= 300` ms (plus a 6 px
/// radius; the app-level analogue of "same place" is "same row index") —
/// so double-clicking here feels the same as double-clicking in an iced
/// `text_input`.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// Whether a plain click on `visible[index]` at `now` completes a
/// double-click with the previously recorded one. Pure — the caller
/// (`Message::RowClicked`'s handler) supplies `Instant::now()`, so tests
/// can drive this with synthetic instants instead of sleeping.
///
/// `saturating_duration_since` (never panics, clamps to zero) instead of
/// bare subtraction: `Instant` arithmetic that could underflow is exactly
/// the kind of "can't happen" the no-panic rule says to make impossible
/// anyway.
fn is_double_click(
    previous: Option<(usize, std::time::Instant)>,
    index: usize,
    now: std::time::Instant,
) -> bool {
    match previous {
        Some((last_index, last_time)) => {
            last_index == index && now.saturating_duration_since(last_time) <= DOUBLE_CLICK_WINDOW
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::vfs::Location;
    use iced::keyboard;
    use std::ffi::OsStr;

    // ── Double-click pairing (app-side, see `Message::RowClicked`) ──────
    //
    // The pure decision (`is_double_click`) is tested with synthetic
    // instants — never a real sleep — exactly the "test the argument-
    // taking half" split CLAUDE.md prescribes for env lookups. The handler
    // tests below then only rely on "two consecutive `update` calls in
    // one test run happen within 300 ms of each other", which is safe by
    // orders of magnitude.

    #[test]
    fn a_second_click_on_the_same_row_inside_the_window_is_a_double() {
        let base = std::time::Instant::now();
        let previous = Some((3, base));
        assert!(is_double_click(
            previous,
            3,
            base + std::time::Duration::from_millis(120)
        ));
        // Boundary is inclusive, matching iced's own `<= 300` ms check.
        assert!(is_double_click(previous, 3, base + DOUBLE_CLICK_WINDOW));
    }

    #[test]
    fn a_slow_or_misaimed_second_click_is_not_a_double() {
        let base = std::time::Instant::now();
        let previous = Some((3, base));
        // Too slow — one past the window.
        assert!(!is_double_click(
            previous,
            3,
            base + DOUBLE_CLICK_WINDOW + std::time::Duration::from_millis(1)
        ));
        // Fast enough, but a different row.
        assert!(!is_double_click(
            previous,
            4,
            base + std::time::Duration::from_millis(50)
        ));
        // Nothing recorded at all (first-ever click).
        assert!(!is_double_click(None, 3, base));
    }

    #[test]
    fn double_clicking_a_directory_row_requests_open_directory() {
        let mut view = listed_view(vec![dir("docs"), file("readme.txt")]);
        let (_, first) = view.update(Message::RowClicked(0));
        assert!(first.is_none(), "a single click must only select");
        assert!(view.selection.is_selected(OsStr::new("docs")));

        let (_, second) = view.update(Message::RowClicked(0));
        match second {
            Some(Event::OpenDirectory(location)) => {
                assert_eq!(location, Location::local("/home/docs"));
            }
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
        // The pair is consumed — a third quick click starts over rather
        // than activating again.
        assert!(view.last_click.is_none());
    }

    #[test]
    fn ctrl_clicks_never_pair_into_an_activation() {
        let mut view = listed_view(vec![dir("docs")]);
        view.modifiers = keyboard::Modifiers::CTRL;
        let (_, first) = view.update(Message::RowClicked(0));
        let (_, second) = view.update(Message::RowClicked(0));
        assert!(first.is_none());
        assert!(second.is_none());
        assert!(view.last_click.is_none());
    }

    #[test]
    fn a_relist_between_clicks_clears_the_pending_pair() {
        let mut view = listed_view(vec![dir("docs")]);
        let _ = view.update(Message::RowClicked(0));
        assert!(view.last_click.is_some());
        // A refresh/navigation rebuilds `visible` — row indices may now
        // mean different entries, so the recorded half must be dropped.
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![dir("docs"), dir("other")]),
        ));
        let (_, event) = view.update(Message::RowClicked(0));
        assert!(event.is_none(), "a stale pre-relist click must not pair");
    }
}
