//! Session state: the app-side half of `core::state`. Restores the window
//! size, last location and per-folder view settings at launch, and saves
//! them when they change.
//!
//! **When state is written** (every write is a reaction to one of these;
//! nothing here runs on a timer, so an idle app writes nothing):
//!
//! * the window is resized (`Message::Resized`, from iced's resize events);
//! * a view lands on a different location (`App::navigate_view`, back/
//!   forward, the first view at startup) — records `last-location`;
//! * a view's view mode or sort changes while it stays put.
//!
//! A write is skipped when it would not change the file. During a live
//! resize drag the compositor sends many events, each a few hundred bytes
//! to a temp file plus a rename; when the drag stops, so do the writes.
//!
//! The write happens inline in `update` rather than in a `Task`: two
//! overlapping background writes could land out of order and leave stale
//! state, and a file this small is cheaper to write than to schedule.
//!
//! Per-folder settings are *per folder*: a folder with nothing saved opens
//! with the `files.toml` defaults, not with whatever the previous folder
//! used. A folder whose settings are changed back to the defaults has its
//! entry removed, which keeps the file small and lets a later config change
//! reach it.

use std::collections::HashMap;

use iced::{Size, Subscription, window};

use saola_files::config::Config;
use saola_files::core::state::{FolderPrefs, Store, WindowSize, location_from_key, location_key};
use saola_files::core::vfs::Location;
use saola_files::ui::dirview::DirectoryView;
use saola_files::ui::explorer::{self, ViewId};

use super::{App, Message as AppMessage};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// The window's new logical size.
    Resized(Size),
}

/// The loaded state plus the bookkeeping needed to notice changes.
pub(crate) struct Session {
    store: Store,
    /// What a folder with nothing saved gets: the `files.toml` values.
    defaults: FolderPrefs,
    /// Per view: the location and prefs as last recorded. Comparing a
    /// view against this is how a change (or a navigation) is noticed
    /// without the view having to announce it.
    seen: HashMap<ViewId, (Location, FolderPrefs)>,
    /// A failing disk should produce one warning, not one per resize event.
    write_warned: bool,
}

impl Session {
    pub(crate) fn new(store: Store, config: &Config) -> Self {
        Session {
            store,
            defaults: FolderPrefs {
                view: config.view,
                sort: config.sort,
                descending: config.sort_descending,
            },
            seen: HashMap::new(),
            write_warned: false,
        }
    }

    /// The folder to open when launched without a path. Local only: a
    /// remote location would open the connect dialog before the user has
    /// asked for anything.
    pub(crate) fn last_location(&self) -> Option<Location> {
        self.store
            .state
            .last_location
            .as_deref()
            .and_then(location_from_key)
            .filter(Location::is_local)
    }

    /// Brings `view` in line with the saved state after it has (maybe)
    /// moved or changed, and records whatever is new. Called after every
    /// update of a view and every navigation; cheap when nothing changed
    /// (one location comparison and one three-field comparison).
    pub(crate) fn view_updated(&mut self, id: ViewId, view: &mut DirectoryView) {
        let mut dirty = false;
        let here = view.location();
        match self.seen.get(&id) {
            Some((location, recorded)) if location == here => {
                // Same folder: only a change of its own settings matters.
                let current = view.prefs();
                if *recorded != current {
                    let location = location.clone();
                    dirty |= self.remember_prefs(&location, current);
                    self.seen.insert(id, (location, current));
                }
            }
            _ => {
                // A new location (or the first sight of this view):
                // restore its saved settings, or the defaults.
                let location = here.clone();
                let key = location_key(&location);
                let prefs = self.store.state.folder_prefs(&key).unwrap_or(self.defaults);
                view.set_prefs(prefs);
                if location.is_local() && self.store.state.last_location.as_deref() != Some(&key) {
                    self.store.state.last_location = Some(key);
                    dirty = true;
                }
                self.seen.insert(id, (location, prefs));
            }
        }
        if dirty {
            self.save();
        }
    }

    /// Writes `prefs` for `location` into the in-memory state; returns
    /// whether it changed. Defaults are stored as the absence of an entry.
    fn remember_prefs(&mut self, location: &Location, prefs: FolderPrefs) -> bool {
        let key = location_key(location);
        if prefs == self.defaults {
            self.store.state.clear_folder_prefs(&key)
        } else {
            self.store.state.set_folder_prefs(&key, prefs)
        }
    }

    fn resized(&mut self, size: Size) {
        // `as` on a float saturates rather than wrapping, and rounding
        // first turns 1100.4 into 1100. Out-of-range sizes (a transient
        // tiny configure) are rejected by `WindowSize::from_logical`.
        let Some(size) = WindowSize::from_logical(size.width, size.height) else {
            return;
        };
        if self.store.state.window != Some(size) {
            self.store.state.window = Some(size);
            self.save();
        }
    }

    fn save(&mut self) {
        if let Err(err) = self.store.save()
            && !self.write_warned
        {
            self.write_warned = true;
            eprintln!("saola-files: could not save window and folder state ({err})");
        }
    }
}

impl App {
    /// `Message::Session`'s handler.
    pub(super) fn handle_session_message(&mut self, message: Message) -> iced::Task<AppMessage> {
        match message {
            Message::Resized(size) => self.session.resized(size),
        }
        iced::Task::none()
    }

    /// Lets the session notice that view `id` moved or changed settings.
    /// An unknown id is a no-op, like every other view lookup.
    pub(super) fn session_view_updated(&mut self, id: ViewId) {
        if let Some(view) = explorer::find_mut(&mut self.views, id) {
            self.session.view_updated(id, view);
        }
    }

    /// Window resize events — the signal for saving the window size.
    ///
    /// iced identifies a subscription partly by its mapper function, so the
    /// mapper may not capture anything: this closure uses only its own
    /// argument, which is allowed.
    pub(super) fn session_subscription(&self) -> Subscription<AppMessage> {
        window::resize_events().map(|(_, size)| AppMessage::Session(Message::Resized(size)))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use saola_files::config::{SortKey, View};
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

    fn launch(path: &Path, at: &str) -> (Session, ViewId, DirectoryView) {
        let config = Config::default();
        let mut session = Session::new(Store::load(Some(path.to_path_buf())), &config);
        let id = ViewIdSource::default().alloc();
        let mut view = DirectoryView::new(Location::local(at), &config);
        session.view_updated(id, &mut view);
        (session, id, view)
    }

    #[test]
    fn folder_settings_survive_leaving_returning_and_relaunching() {
        let scratch = Scratch::new("roundtrip");
        let path = scratch.0.join("state.toml");

        let (mut session, id, mut view) = launch(&path, "/a");
        assert_eq!(view.prefs(), session.defaults);

        // The user switches /a to grid sorted by size, descending.
        view.set_prefs(grid_by_size());
        session.view_updated(id, &mut view);

        // Leave for a folder with nothing saved: it gets the defaults.
        let _ = view.navigate(Location::local("/b"));
        session.view_updated(id, &mut view);
        assert_eq!(view.prefs(), session.defaults);

        // Return: the settings come back.
        let _ = view.navigate(Location::local("/a"));
        session.view_updated(id, &mut view);
        assert_eq!(view.prefs(), grid_by_size());

        // Relaunch from the same file, opening /a again.
        let (_, _, view) = launch(&path, "/a");
        assert_eq!(view.prefs(), grid_by_size());
    }

    #[test]
    fn last_location_is_recorded_and_offered_to_the_next_launch() {
        let scratch = Scratch::new("last");
        let path = scratch.0.join("state.toml");
        let (_, _, _) = launch(&path, "/data/photos");
        let config = Config::default();
        let next = Session::new(Store::load(Some(path)), &config);
        assert_eq!(next.last_location(), Some(Location::local("/data/photos")));
    }

    #[test]
    fn changing_settings_back_to_the_defaults_removes_the_entry() {
        let scratch = Scratch::new("revert");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        view.set_prefs(grid_by_size());
        session.view_updated(id, &mut view);
        assert_eq!(session.store.state.folder_count(), 1);
        view.set_prefs(session.defaults);
        session.view_updated(id, &mut view);
        assert_eq!(session.store.state.folder_count(), 0);
    }

    #[test]
    fn an_idle_view_causes_no_writes() {
        let scratch = Scratch::new("idle");
        let path = scratch.0.join("state.toml");
        let (mut session, id, mut view) = launch(&path, "/a");
        std::fs::remove_file(&path).expect("remove the file written at launch");
        // Nothing changed: updates must not recreate it.
        for _ in 0..3 {
            session.view_updated(id, &mut view);
        }
        session.resized(Size::new(0.0, 0.0));
        assert!(!path.exists());
    }

    #[test]
    fn a_resize_is_saved_once_and_only_when_it_differs() {
        let scratch = Scratch::new("resize");
        let path = scratch.0.join("state.toml");
        let (mut session, _, _) = launch(&path, "/a");
        session.resized(Size::new(900.0, 600.0));
        assert_eq!(
            Store::load(Some(path.clone())).state.window,
            Some(WindowSize {
                width: 900,
                height: 600
            })
        );
        std::fs::remove_file(&path).expect("remove");
        session.resized(Size::new(900.0, 600.0));
        assert!(!path.exists(), "same size is not rewritten");
    }

    #[test]
    fn an_unwritable_location_warns_and_carries_on() {
        // The state file's parent is a regular file, so create_dir_all fails.
        let scratch = Scratch::new("unwritable");
        let blocker = scratch.0.join("blocker");
        std::fs::write(&blocker, "x").expect("write");
        let path = blocker.join("state.toml");
        let (mut session, _, _) = launch(&path, "/a");
        assert!(session.write_warned);
        session.resized(Size::new(900.0, 600.0));
        assert_eq!(
            session.store.state.window,
            Some(WindowSize {
                width: 900,
                height: 600
            })
        );
    }
}
