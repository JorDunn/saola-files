//! The status bar at the app level: composing it into the footer, and
//! keeping each view's free-space reading up to date.
//!
//! **The footer slot.** The window's footer is a column: body, error notice,
//! then one band. That band is the progress strip while an operation runs,
//! the undo toast just after one, and otherwise this status bar. They
//! *replace* each other (`saola-theme`'s rule: a transient strip never
//! stacks on top of the bar), so [`App::with_status_bar`] is only called for
//! the "nothing transient" case in `view.rs`.
//!
//! **When free space is read (signal, never poll).** There is no timer. A
//! `statfs` is requested when:
//!
//! 1. a listing lands in a view: navigation, F5, the reload after an
//!    operation or a rename, a `--select` reveal (the view's
//!    `refresh_epoch` moves; see `ui::dirview::summary`);
//! 2. a watch batch lands in a view (a local folder changed on disk, which
//!    is also how an external `rm` or a download shows up in "free");
//! 3. a copy or move finishes or is cancelled (`Message::OpEvent`), for
//!    every view, because the destination may be any of them.
//!
//! Only backends that report `Caps::STATFS` are asked; for the rest the
//! right side of the bar is simply absent. At most one `statfs` per view is
//! in flight: a signal that arrives meanwhile is remembered and answered by
//! one follow-up read, so a burst of watch batches cannot pile up syscalls.
//! The reading is shown only while it belongs to the view's *current*
//! location, so navigating elsewhere never shows the old folder's disk.
//!
//! **Tabs and split panes.** State is keyed by `ViewId`, and
//! [`App::with_status_bar`] reads the *focused* view. A feature that shows
//! more than one view calls `ui::statusbar::view` once per pane with that
//! pane's `ViewId`, `summary()` and [`FreeSpace::stats`]; nothing else here
//! assumes a single view.

use iced::widget::column;
use iced::{Element, Fill, Task};

use saola_files::core::fs::ops;
use saola_files::core::vfs::{Caps, FsStats, Location, VfsError};
use saola_files::{modules, ui};

use ui::explorer::ViewId;

use super::{App, Message};

/// One view's free-space bookkeeping.
#[derive(Debug)]
struct Slot {
    /// The location `stats` was read for (or the one a read is in flight
    /// for). A reading is only ever shown for the view's current location.
    location: Location,
    stats: Option<FsStats>,
    in_flight: bool,
    /// A signal arrived while a read was in flight: read once more when it
    /// returns.
    again: bool,
}

/// Free-space readings for every open view. Pure bookkeeping: it never
/// touches a backend, so the rules above are unit tests.
#[derive(Debug, Default)]
pub struct FreeSpace {
    slots: Vec<(ViewId, Slot)>,
}

impl FreeSpace {
    /// A signal says `id`'s free space may have changed. Returns true when
    /// the caller should start a `statfs` now.
    ///
    /// `supported` is whether the view's backend has `Caps::STATFS`; when it
    /// does not, any old reading is dropped and nothing is requested.
    pub fn request(&mut self, id: ViewId, location: &Location, supported: bool) -> bool {
        if !self.slots.iter().any(|(slot_id, _)| *slot_id == id) {
            self.slots.push((
                id,
                Slot {
                    location: location.clone(),
                    stats: None,
                    in_flight: false,
                    again: false,
                },
            ));
        }
        let Some((_, slot)) = self.slots.iter_mut().find(|(slot_id, _)| *slot_id == id) else {
            return false;
        };
        if slot.location != *location {
            // The view moved: the old reading is about another disk.
            slot.location = location.clone();
            slot.stats = None;
        }
        if !supported {
            slot.stats = None;
            slot.again = false;
            return false;
        }
        if slot.in_flight {
            slot.again = true;
            return false;
        }
        slot.in_flight = true;
        true
    }

    /// A `statfs` for `location` came back. Returns true when another read
    /// is owed (a signal arrived while this one was running).
    pub fn complete(
        &mut self,
        id: ViewId,
        location: &Location,
        result: Result<FsStats, VfsError>,
    ) -> bool {
        let Some((_, slot)) = self.slots.iter_mut().find(|(slot_id, _)| *slot_id == id) else {
            return false;
        };
        slot.in_flight = false;
        // A reading for a place the view has since left is dropped; the
        // follow-up (if owed) reads the right place.
        if slot.location == *location {
            // A failed read shows nothing (no stale number, no error text:
            // a missing right side is the honest "unknown").
            slot.stats = result.ok();
        }
        let owed = std::mem::take(&mut slot.again);
        if owed {
            slot.in_flight = true;
        }
        owed
    }

    /// The reading to show for `id` now viewing `location`, if it is for
    /// that location.
    pub fn stats(&self, id: ViewId, location: &Location) -> Option<&FsStats> {
        self.slots
            .iter()
            .find(|(slot_id, _)| *slot_id == id)
            .map(|(_, slot)| slot)
            .filter(|slot| slot.location == *location)
            .and_then(|slot| slot.stats.as_ref())
    }
}

/// Whether an op event ends the running operation, so its free space is
/// worth re-reading. `Progress`, `Conflict` and the start do not.
pub(super) fn op_event_ends_op(event: &ops::OpEvent) -> bool {
    matches!(
        event,
        ops::OpEvent::Finished { .. } | ops::OpEvent::Cancelled
    )
}

impl App {
    /// After a message reached view `id`: if a listing or watch batch landed
    /// there (`refreshed`), re-read its free space.
    pub(super) fn free_space_after_view_update(
        &mut self,
        id: ViewId,
        refreshed: bool,
    ) -> Task<Message> {
        if refreshed {
            self.refresh_free_space(id)
        } else {
            Task::none()
        }
    }

    /// Re-read free space for every open view (an operation finished).
    pub(super) fn refresh_all_free_space(&mut self) -> Task<Message> {
        let ids: Vec<ViewId> = self.views.iter().map(|(id, _)| *id).collect();
        Task::batch(ids.into_iter().map(|id| self.refresh_free_space(id)))
    }

    /// Ask `id`'s backend for its free space, if it can say and no read is
    /// already in flight.
    fn refresh_free_space(&mut self, id: ViewId) -> Task<Message> {
        let Some(view) = ui::explorer::find(&self.views, id) else {
            return Task::none();
        };
        let location = view.location().clone();
        let supported = view.caps().contains(Caps::STATFS);
        if !self.free_space.request(id, &location, supported) {
            return Task::none();
        }
        Self::statfs_task(id, location)
    }

    fn statfs_task(id: ViewId, location: Location) -> Task<Message> {
        let for_message = location.clone();
        Task::perform(
            // `async move` takes ownership of `location`, so the future is
            // `'static` and can outlive this function while iced runs it.
            async move {
                match modules::resolve(&location) {
                    Some(backend) => backend.statfs(&location).await,
                    // The backend went away (a dropped connection) between
                    // the signal and now; shown as "no free space known".
                    None => Err(VfsError::NotFound {
                        location: location.to_string(),
                    }),
                }
            },
            move |result| Message::FreeSpace(id, for_message.clone(), result),
        )
    }

    /// `Message::FreeSpace`: a `statfs` finished.
    pub(super) fn on_free_space(
        &mut self,
        id: ViewId,
        location: Location,
        result: Result<FsStats, VfsError>,
    ) -> Task<Message> {
        if self.free_space.complete(id, &location, result) {
            // A signal arrived while that read ran. `complete` already
            // marked a read as in flight, so go straight to the task.
            if let Some(view) = ui::explorer::find(&self.views, id) {
                return Self::statfs_task(id, view.location().clone());
            }
        }
        Task::none()
    }

    /// `body` with the status bar beneath it: the footer's resting state.
    pub(super) fn with_status_bar<'a>(
        &'a self,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let bar: Element<'a, Message> = if self.trash_active {
            // The Trash browser is not a `DirectoryView`: no selection, no
            // capacity, just how many items it holds.
            ui::statusbar::view(
                t,
                self.surface,
                ui::statusbar::trash_text(
                    self.trash_view.item_count(),
                    self.trash_view.is_loading(),
                    self.trash_view.has_error(),
                ),
                None,
            )
        } else {
            match self.focused_view() {
                Some(view) => ui::statusbar::view(
                    t,
                    self.surface,
                    ui::statusbar::start_text(&view.summary()),
                    self.free_space.stats(self.focused, view.location()),
                ),
                None => return body,
            }
        };
        column![body, bar].width(Fill).height(Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::explorer::ViewIdSource;

    fn stats(free: u64) -> FsStats {
        FsStats {
            total: 100,
            free,
            available: free,
        }
    }

    fn setup() -> (FreeSpace, ViewId, Location) {
        (
            FreeSpace::default(),
            ViewIdSource::default().alloc(),
            Location::local("/a"),
        )
    }

    #[test]
    fn a_signal_starts_one_read_and_the_result_is_shown() {
        let (mut book, id, here) = setup();
        assert!(book.request(id, &here, true));
        assert_eq!(book.stats(id, &here), None, "nothing until it returns");
        assert!(!book.complete(id, &here, Ok(stats(40))));
        assert_eq!(book.stats(id, &here), Some(&stats(40)));
    }

    #[test]
    fn a_backend_without_statfs_is_never_asked_and_shows_nothing() {
        let (mut book, id, here) = setup();
        assert!(!book.request(id, &here, false));
        assert_eq!(book.stats(id, &here), None);
    }

    #[test]
    fn signals_during_a_read_coalesce_into_one_follow_up() {
        let (mut book, id, here) = setup();
        assert!(book.request(id, &here, true));
        // Three watch batches while the first read is still running.
        assert!(!book.request(id, &here, true));
        assert!(!book.request(id, &here, true));
        assert!(!book.request(id, &here, true));
        // One follow-up is owed, and only one.
        assert!(book.complete(id, &here, Ok(stats(40))));
        assert!(!book.complete(id, &here, Ok(stats(39))));
    }

    #[test]
    fn a_reading_is_hidden_once_the_view_has_moved_elsewhere() {
        let (mut book, id, here) = setup();
        let there = Location::local("/b");
        assert!(book.request(id, &here, true));
        let _ = book.complete(id, &here, Ok(stats(40)));
        assert_eq!(book.stats(id, &there), None);
        // The next signal for the new location drops the old reading.
        assert!(book.request(id, &there, true));
        assert_eq!(book.stats(id, &here), None);
    }

    #[test]
    fn a_late_reading_for_the_old_location_is_dropped_and_a_read_is_owed() {
        let (mut book, id, here) = setup();
        let there = Location::local("/b");
        assert!(book.request(id, &here, true));
        // Navigation happens while the first read is out.
        assert!(!book.request(id, &there, true));
        // The stale answer must not be shown for `there`.
        assert!(book.complete(id, &here, Ok(stats(40))));
        assert_eq!(book.stats(id, &there), None);
        assert!(!book.complete(id, &there, Ok(stats(10))));
        assert_eq!(book.stats(id, &there), Some(&stats(10)));
    }

    #[test]
    fn a_failed_read_clears_the_number_instead_of_keeping_a_stale_one() {
        let (mut book, id, here) = setup();
        assert!(book.request(id, &here, true));
        let _ = book.complete(id, &here, Ok(stats(40)));
        assert!(book.request(id, &here, true));
        let _ = book.complete(
            id,
            &here,
            Err(VfsError::NotFound {
                location: "x".into(),
            }),
        );
        assert_eq!(book.stats(id, &here), None);
    }

    #[test]
    fn views_are_tracked_independently() {
        let mut ids = ViewIdSource::default();
        let (a, b) = (ids.alloc(), ids.alloc());
        let mut book = FreeSpace::default();
        let (here, there) = (Location::local("/a"), Location::local("/b"));
        assert!(book.request(a, &here, true));
        assert!(book.request(b, &there, true));
        let _ = book.complete(a, &here, Ok(stats(1)));
        let _ = book.complete(b, &there, Ok(stats(2)));
        assert_eq!(book.stats(a, &here), Some(&stats(1)));
        assert_eq!(book.stats(b, &there), Some(&stats(2)));
    }

    #[test]
    fn only_the_end_of_an_operation_is_a_free_space_signal() {
        assert!(op_event_ends_op(&ops::OpEvent::Finished { errors: vec![] }));
        assert!(op_event_ends_op(&ops::OpEvent::Cancelled));
        assert!(!op_event_ends_op(&ops::OpEvent::Progress {
            files_done: 1,
            bytes_done: 1
        }));
    }
}
