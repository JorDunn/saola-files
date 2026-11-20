//! Session state: the app-side half of `core::state`. Restores the window
//! size, last location and per-folder view settings at launch, and saves
//! them when they change.
//!
//! # Rules for per-folder view settings
//!
//! * A folder **with** a saved row restores that row when a view arrives,
//!   even if the view arrives carrying different settings.
//! * A folder **without** a row keeps whatever view mode and sort the view
//!   already has (carry-over, as the app always behaved).
//! * A row is written for a folder only when the user changes view mode or
//!   sort *while in that folder*. Visiting, or carrying settings in, never
//!   writes one. Setting a folder back to the defaults still writes its
//!   row: "this folder is a list sorted by name" is a choice, and the
//!   folder you came from may have been something else.
//!
//! # When state is written
//!
//! Nothing here runs on a timer, and an idle app writes nothing.
//! Changes are first recorded in memory and the store is marked dirty;
//! it is written ("flushed") on these signals only:
//!
//! * a view lands on a different location, or its view mode or sort
//!   changes (these also record `last-location` once the folder has really
//!   listed, never before: a CLI target may be a file or missing);
//! * the window is closing (`window::close_requests`, and the header's own
//!   close button).
//!
//! A window **resize** only updates the size in memory. A drag or a
//! compositor animation produces one event per frame, and a write
//! (create-directory, write, rename) on the UI thread for each would stall
//! the interface on a slow or network home directory. The size therefore
//! reaches disk with the next flush signal, at the latest on close. A
//! crash or SIGTERM can lose the last resize; that is an accepted cost.
//!
//! A flush with nothing dirty does nothing. A failed write keeps the store
//! dirty (the next signal tries again) and warns only once.
//!
//! # Closing
//!
//! The window is opened with `exit_on_close_request: false` (see
//! `main.rs`), so a compositor close request reaches us as a message
//! instead of iced closing the window by itself. We flush, *then* return
//! `window::close(id)`; the flush has therefore finished before iced is
//! even asked to close anything, with no race against process exit. (With
//! iced's default the window is closed inside the event loop before the
//! app sees anything, and `Closed` is only delivered afterwards.)
//!
//! # Startup
//!
//! A launch without a path reopens the last local location. If that
//! folder no longer lists, the view falls back to the home directory and
//! the dead path is forgotten, instead of showing an error every launch.

use std::collections::HashMap;

use iced::{Size, Subscription, Task, window};

use saola_files::core::state::{FolderPrefs, Store, WindowSize, location_from_key, location_key};
use saola_files::core::vfs::Location;
use saola_files::ui::dirview::DirectoryView;
use saola_files::ui::explorer::{self, ViewId};

use super::{App, Message as AppMessage};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// The window's new logical size (memory only; see the module docs).
    Resized(Size),
    /// The window is closing (a close request, or its `Closed` event):
    /// flush. A request also closes the window afterwards.
    CloseRequested(window::Id),
    Closed,
}

/// What the startup restore of the last location should do next.
#[derive(Debug, PartialEq, Eq)]
enum Restore {
    /// Still loading; decide later.
    Pending,
    /// Listed fine, or the user already went elsewhere. Stop watching.
    Done,
    /// The remembered folder cannot be listed: go home, forget it.
    FallBack,
}

/// The pure decision behind the startup fallback.
fn restore_outcome(target: &Location, here: &Location, listed: bool, failed: bool) -> Restore {
    if here != target || listed {
        Restore::Done
    } else if failed {
        Restore::FallBack
    } else {
        Restore::Pending
    }
}

/// What the session last recorded about one view.
struct Seen {
    location: Location,
    prefs: FolderPrefs,
    /// Whether `last-location` has been considered for this location (it
    /// is, once the folder has listed).
    recorded: bool,
}

/// The loaded state plus the bookkeeping needed to notice changes.
pub(crate) struct Session {
    store: Store,
    /// The home directory: where a dead last location falls back to.
    home: Location,
    /// Per view, as last seen; comparing a view against this is how a
    /// change or a navigation is noticed without the view announcing it.
    seen: HashMap<ViewId, Seen>,
    /// The last location opened at startup, until it is known to list.
    restoring: Option<Location>,
    /// The store differs from the file.
    dirty: bool,
    /// A failing disk should produce one warning, not one per signal.
    write_warned: bool,
    /// Writes attempted so far (tests assert on this).
    attempts: u32,
}

impl Session {
    pub(crate) fn new(store: Store, home: Location) -> Self {
        Session {
            store,
            home,
            seen: HashMap::new(),
            restoring: None,
            dirty: false,
            write_warned: false,
            attempts: 0,
        }
    }

    /// The folder to open when launched without a path. Local only: a
    /// remote location would open the connect dialog before the user has
    /// asked for anything. Call [`Self::begin_restore`] if it is used.
    pub(crate) fn last_location(&self) -> Option<Location> {
        self.store
            .state
            .last_location
            .as_deref()
            .and_then(location_from_key)
            .filter(Location::is_local)
    }

    /// Marks `location` as the startup restore, so a failure to list it
    /// falls back to home.
    pub(crate) fn begin_restore(&mut self, location: Location) {
        self.restoring = Some(location);
    }

    /// Brings `view` in line with the saved state after it has (maybe)
    /// moved or changed, and records whatever is new. Called after every
    /// update of a view and every navigation; cheap when nothing changed
    /// (one location comparison and one three-field comparison).
    ///
    /// Returns a location the caller should navigate the view to (the
    /// home fallback for a dead startup location).
    pub(crate) fn view_updated(
        &mut self,
        id: ViewId,
        view: &mut DirectoryView,
    ) -> Option<Location> {
        let mut changed = false;
        let mut fallback = None;

        let outcome = self.restoring.as_ref().map(|target| {
            restore_outcome(
                target,
                view.location(),
                view.has_listed(),
                view.list_failed(),
            )
        });
        match outcome {
            Some(Restore::Done) => self.restoring = None,
            Some(Restore::FallBack) => {
                self.restoring = None;
                if self.store.state.last_location.take().is_some() {
                    changed = true;
                }
                fallback = Some(self.home.clone());
            }
            Some(Restore::Pending) | None => {}
        }

        match self.seen.get_mut(&id) {
            Some(seen) if seen.location == *view.location() => {
                // Same folder: only the user changing its settings counts.
                let current = view.prefs();
                if seen.prefs != current {
                    seen.prefs = current;
                    changed |= self
                        .store
                        .state
                        .set_folder_prefs(&location_key(&seen.location), current);
                }
            }
            _ => {
                // A new location (or the first sight of this view). A
                // saved row wins; otherwise the view keeps what it has.
                let location = view.location().clone();
                if let Some(row) = self.store.state.folder_prefs(&location_key(&location)) {
                    view.set_prefs(row);
                }
                self.seen.insert(
                    id,
                    Seen {
                        location,
                        prefs: view.prefs(),
                        recorded: false,
                    },
                );
            }
        }

        // `last-location` only once the folder has really listed.
        if let Some(seen) = self.seen.get_mut(&id)
            && !seen.recorded
            && view.has_listed()
        {
            seen.recorded = true;
            if seen.location.is_local() {
                let key = location_key(&seen.location);
                if self.store.state.last_location.as_deref() != Some(&key) {
                    self.store.state.last_location = Some(key);
                    changed = true;
                }
            }
        }

        if changed {
            self.dirty = true;
            self.flush();
        }
        fallback
    }

    /// Records the window size in memory only (see the module docs).
    fn resized(&mut self, size: Size) {
        // Out-of-range sizes (a transient tiny configure) are rejected by
        // `from_logical`; rounding turns 1100.4 into 1100.
        let Some(size) = WindowSize::from_logical(size.width, size.height) else {
            return;
        };
        if self.store.state.window != Some(size) {
            self.store.state.window = Some(size);
            self.dirty = true;
        }
    }

    /// Writes the store if it differs from the file. Safe to call at any
    /// time; does nothing when clean.
    fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.attempts += 1;
        match self.store.save() {
            Ok(()) => self.dirty = false,
            // Stay dirty so the next signal retries, but say so once.
            Err(err) => {
                if !self.write_warned {
                    self.write_warned = true;
                    eprintln!("saola-files: could not save window and folder state ({err})");
                }
            }
        }
    }
}

impl App {
    /// `Message::Session`'s handler.
    pub(super) fn handle_session_message(&mut self, message: Message) -> Task<AppMessage> {
        match message {
            Message::Resized(size) => {
                self.session.resized(size);
                Task::none()
            }
            Message::CloseRequested(id) => {
                // Flush first; only then ask iced to close the window.
                self.session.flush();
                window::close(id)
            }
            Message::Closed => {
                self.session.flush();
                Task::none()
            }
        }
    }

    /// Lets the session notice that view `id` moved or changed settings.
    /// An unknown id is a no-op, like every other view lookup. May return
    /// a navigation (the dead-startup-location fallback).
    pub(super) fn session_view_updated(&mut self, id: ViewId) -> Task<AppMessage> {
        let Some(view) = explorer::find_mut(&mut self.views, id) else {
            return Task::none();
        };
        match self.session.view_updated(id, view) {
            Some(location) => self.navigate_view(id, location),
            None => Task::none(),
        }
    }

    /// Resize events (memory only) and close signals (flush).
    ///
    /// iced identifies a subscription partly by its mapper function, so a
    /// mapper may not capture anything; these closures use only their own
    /// argument, which is allowed.
    pub(super) fn session_subscription(&self) -> Subscription<AppMessage> {
        Subscription::batch([
            window::resize_events().map(|(_, size)| AppMessage::Session(Message::Resized(size))),
            window::close_requests().map(|id| AppMessage::Session(Message::CloseRequested(id))),
            window::close_events().map(|_| AppMessage::Session(Message::Closed)),
        ])
    }

    /// The window header's close button: flush before the window closes.
    pub(super) fn session_flush(&mut self) {
        self.session.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use saola_files::config::{Config, SortKey, View};
    use saola_files::ui::dirview::Message as ViewMessage;
    use saola_files::ui::explorer::ViewIdSource;

    use super::*;

    /// A scratch directory removed on drop (see `core::state`'s tests).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("saola-files-session-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn grid_by_size() -> FolderPrefs {
        FolderPrefs {
            view: View::Grid,
            sort: SortKey::Size,
            descending: true,
        }
    }

    fn defaults() -> FolderPrefs {
        let config = Config::default();
        FolderPrefs {
            view: config.view,
            sort: config.sort,
            descending: config.sort_descending,
        }
    }

    fn session_at(path: &Path) -> Session {
        Session::new(
            Store::load(Some(path.to_path_buf())),
            Location::local("/home/test"),
        )
    }

    /// Opens a view at `at` in a fresh session, as listed (the common
    /// case; tests that care about listing build their own).
    fn launch(path: &Path, at: &str) -> (Session, ViewId, DirectoryView) {
        let mut session = session_at(path);
        let id = ViewIdSource::default().alloc();
        let mut view = DirectoryView::new(Location::local(at), &Config::default());
        list_ok(&mut view);
        session.view_updated(id, &mut view);
        (session, id, view)
    }

    fn list_ok(view: &mut DirectoryView) {
        let here = view.location().clone();
        let _ = view.update(ViewMessage::Listed(here, Ok(Vec::new())));
    }

    /// Navigates to `to`, lists it, and tells the session.
    fn go(session: &mut Session, id: ViewId, view: &mut DirectoryView, to: &str) {
        let _ = view.navigate(Location::local(to));
        list_ok(view);
        session.view_updated(id, view);
    }

    fn rows(path: &Path) -> usize {
        Store::load(Some(path.to_path_buf())).state.folder_count()
    }

    #[test]
    fn a_saved_folder_restores_after_leaving_returning_and_relaunching() {
        let scratch = Scratch::new("roundtrip");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        assert_eq!(view.prefs(), defaults());

        view.set_prefs(grid_by_size());
        session.view_updated(id, &mut view);

        // /b has no row, so it keeps the carried-over settings.
        go(&mut session, id, &mut view, "/b");
        assert_eq!(view.prefs(), grid_by_size());

        // Change /b to something else, then come back to /a: its own
        // row wins over what we arrive carrying.
        view.set_prefs(defaults());
        session.view_updated(id, &mut view);
        go(&mut session, id, &mut view, "/a");
        assert_eq!(view.prefs(), grid_by_size());

        let (_, _, view) = launch(&path, "/a");
        assert_eq!(view.prefs(), grid_by_size());
    }

    #[test]
    fn an_unsaved_folder_inherits_the_carried_settings_and_writes_no_row() {
        let scratch = Scratch::new("carry");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        view.set_prefs(grid_by_size());
        session.view_updated(id, &mut view);
        assert_eq!(rows(&path), 1);

        go(&mut session, id, &mut view, "/a/sub");
        assert_eq!(view.prefs(), grid_by_size(), "carried over");
        assert_eq!(rows(&path), 1, "visiting writes no row");
        go(&mut session, id, &mut view, "/a/sub/deeper");
        assert_eq!(rows(&path), 1);
    }

    #[test]
    fn changing_a_setting_in_an_unsaved_folder_writes_its_row() {
        let scratch = Scratch::new("change");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        view.set_prefs(grid_by_size());
        session.view_updated(id, &mut view);
        go(&mut session, id, &mut view, "/b");
        assert_eq!(rows(&path), 1);

        // Back to the defaults inside /b: a deliberate choice, so a row.
        view.set_prefs(defaults());
        session.view_updated(id, &mut view);
        assert_eq!(rows(&path), 2);
        go(&mut session, id, &mut view, "/a");
        go(&mut session, id, &mut view, "/b");
        assert_eq!(view.prefs(), defaults());
    }

    #[test]
    fn last_location_is_recorded_only_once_the_folder_has_listed() {
        let scratch = Scratch::new("last");
        let path = scratch.0.join("state.toml");
        let mut session = session_at(&path);
        let id = ViewIdSource::default().alloc();
        // A view the way `open_target` builds it: not loading, not listed.
        let mut view = DirectoryView::new(Location::local("/maybe/a/file"), &Config::default());
        session.view_updated(id, &mut view);
        assert_eq!(session.store.state.last_location, None);
        assert!(!path.exists());

        // The probe resolves to the parent directory, which lists.
        let _ = view.update(ViewMessage::TargetResolved(
            Location::local("/maybe/a"),
            None,
            Ok(Vec::new()),
        ));
        session.view_updated(id, &mut view);
        assert_eq!(session.last_location(), Some(Location::local("/maybe/a")));
        let next = session_at(&path);
        assert_eq!(next.last_location(), Some(Location::local("/maybe/a")));
    }

    #[test]
    fn a_failed_listing_never_becomes_the_last_location() {
        let scratch = Scratch::new("failed");
        let path = scratch.0.join("state.toml");
        let mut session = session_at(&path);
        let id = ViewIdSource::default().alloc();
        let mut view = DirectoryView::new(Location::local("/gone"), &Config::default());
        let _ = view.navigate(Location::local("/gone2"));
        session.view_updated(id, &mut view);
        assert_eq!(session.store.state.last_location, None);
    }

    #[test]
    fn restore_outcome_decides_the_startup_fallback() {
        let target = Location::local("/gone");
        let other = Location::local("/elsewhere");
        assert_eq!(
            restore_outcome(&target, &target, false, false),
            Restore::Pending
        );
        assert_eq!(
            restore_outcome(&target, &target, true, false),
            Restore::Done
        );
        assert_eq!(
            restore_outcome(&target, &target, false, true),
            Restore::FallBack
        );
        // The user has already moved on: leave them alone.
        assert_eq!(restore_outcome(&target, &other, false, true), Restore::Done);
    }

    #[test]
    fn a_dead_last_location_falls_back_home_and_is_forgotten() {
        let scratch = Scratch::new("dead");
        let path = scratch.0.join("state.toml");
        let mut store = Store::load(Some(path.clone()));
        store.state.last_location = Some("file:///gone/forever".to_owned());
        store.save().expect("saves");

        let mut session = session_at(&path);
        let dead = session.last_location().expect("restorable");
        session.begin_restore(dead.clone());
        let id = ViewIdSource::default().alloc();
        let (mut view, _task) = DirectoryView::open(dead.clone(), &Config::default());
        assert_eq!(session.view_updated(id, &mut view), None, "still loading");

        let err = saola_files::core::vfs::VfsError::Other {
            message: "gone".to_owned(),
        };
        let _ = view.update(ViewMessage::Listed(dead, Err(err)));
        assert_eq!(
            session.view_updated(id, &mut view),
            Some(Location::local("/home/test"))
        );
        assert_eq!(Store::load(Some(path)).state.last_location, None);
    }

    #[test]
    fn resizes_cause_no_writes_until_a_flush_signal() {
        let scratch = Scratch::new("resize");
        let path = scratch.0.join("state.toml");
        let (mut session, _, _) = launch(&path, "/a");
        let before = session.attempts;
        for n in 0..50_u8 {
            session.resized(Size::new(900.0 + f32::from(n), 600.0));
        }
        assert_eq!(session.attempts, before, "no write attempted");
        assert_eq!(Store::load(Some(path.clone())).state.window, None);

        session.flush();
        assert_eq!(session.attempts, before + 1);
        assert_eq!(
            Store::load(Some(path)).state.window,
            Some(WindowSize {
                width: 949,
                height: 600
            })
        );
    }

    #[test]
    fn a_flush_with_nothing_dirty_writes_nothing() {
        let scratch = Scratch::new("clean");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        std::fs::remove_file(&path).expect("remove the file written at launch");
        let before = session.attempts;
        for _ in 0..3 {
            session.view_updated(id, &mut view);
            session.flush();
        }
        session.resized(Size::new(0.0, 0.0));
        session.flush();
        session.resized(Size::new(900.0, 600.0));
        session.flush();
        session.resized(Size::new(900.0, 600.0));
        session.flush();
        assert_eq!(session.attempts, before + 1, "only the one real change");
    }

    #[test]
    fn a_failing_write_is_attempted_once_per_flush_signal_and_warned_once() {
        // The state file's parent is a regular file, so the write fails.
        let scratch = Scratch::new("unwritable");
        let blocker = scratch.0.join("blocker");
        std::fs::write(&blocker, "x").expect("write");
        let path = blocker.join("state.toml");
        let (mut session, _, _) = launch(&path, "/a");
        let before = session.attempts;
        assert!(session.write_warned, "the launch write failed and warned");

        for _ in 0..40 {
            session.resized(Size::new(900.0, 600.0));
        }
        assert_eq!(session.attempts, before, "resizes never attempt a write");
        session.flush();
        session.flush();
        assert_eq!(session.attempts, before + 2, "one attempt per flush signal");
        assert_eq!(
            session.store.state.window,
            Some(WindowSize {
                width: 900,
                height: 600
            })
        );
    }
}
