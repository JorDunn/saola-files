//! The per-view watch [`Subscription`]: resolves the active `Location`'s
//! backend, and — if it can signal (`Backend::watch` returns `Some`) —
//! bridges its raw [`DirEvent`] stream into debounced `Vec<DirEvent>`
//! batches as [`Message::Watch`]. `DirectoryView::apply_watch_events`
//! (`mod.rs`) is what actually mutates `entries`/`selection` off of those;
//! this module is only the plumbing that gets a batch there.
//!
//! Backends without `Caps::WATCH` (`watch()` returns `None`, or nothing
//! resolves for the location's scheme at all) produce an empty stream here
//! — those backends' whole live-update story stays refresh-on-navigate +
//! F5 (CLAUDE.md), which `ui::header`'s conditional refresh button and the
//! existing `Action::Refresh` already cover; nothing else to wire up.
//!
//! The `impl DirectoryView` block at the bottom holds the receiving end:
//! `apply_watch_events` and the `Message::WatchRefreshed` arm.

use std::time::Duration;

use iced::Subscription;
use iced::futures::stream::{self, BoxStream, Stream, StreamExt};

use std::ffi::{OsStr, OsString};

use iced::Task;

use crate::core::fs::entry::FileEntry;
use crate::core::vfs::{DirEvent, Location};

use super::{DirectoryView, Event, Message};

/// How long a burst of watch events must stay quiet before it's flushed as
/// one [`Message::Watch`] batch. Bounds how many times `DirectoryView`
/// re-sorts per burst (once per quiet gap, not once per file) while
/// staying comfortably inside the stage's "appears within 100ms" bar —
/// this is a fixed, generous fraction of that budget, not a tuned value.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(40);

/// Hard cap on how many events one batch accumulates before flushing
/// early, so a watch that never goes quiet (something writing
/// continuously) still surfaces changes instead of buffering forever.
const MAX_BATCH: usize = 256;

/// `Subscription::run_with`'s builder must be a bare `fn` pointer, not a
/// capturing closure — `data` (here, `location.clone()`) plus this pointer
/// together are the subscription's identity, which is how iced knows to
/// keep the underlying stream alive across re-renders of the *same*
/// location, and to tear it down and rebuild fresh the moment the view
/// navigates somewhere else (see `Subscription::run_with`'s docs). This
/// wrapper exists only so `DirectoryView::subscription` doesn't have to
/// name `build`'s function-pointer type itself.
pub fn subscription(location: &Location) -> Subscription<Message> {
    Subscription::run_with(location.clone(), build)
}

fn build(location: &Location) -> BoxStream<'static, Message> {
    let Some(backend) = crate::modules::resolve(location) else {
        return stream::empty().boxed();
    };
    let Some(events) = backend.watch(location) else {
        return stream::empty().boxed();
    };
    debounced(events).map(Message::Watch).boxed()
}

/// State the debounce loop below folds over one step at a time via
/// `stream::unfold` — the raw event stream, plus whatever's been buffered
/// for the batch currently being assembled.
struct DebounceState {
    events: BoxStream<'static, DirEvent>,
    pending: Vec<DirEvent>,
}

/// Groups a raw `DirEvent` stream into `Vec<DirEvent>` batches, one per
/// quiet gap of [`DEBOUNCE_WINDOW`] (or every [`MAX_BATCH`] events,
/// whichever comes first).
///
/// Event-driven, not polling (CLAUDE.md's signal-never-poll rule): an idle
/// watch blocks on `events.next()` with nothing ticking — the debounce
/// timer only exists once at least one real event has landed and a batch
/// is actually pending, the same shape as `typeahead`'s "checked only at
/// the next keystroke" posture, just with an explicit flush deadline
/// instead of an inert buffer, since here nothing else is going to prompt
/// the flush once the burst goes quiet.
fn debounced(events: BoxStream<'static, DirEvent>) -> impl Stream<Item = Vec<DirEvent>> + 'static {
    stream::unfold(
        DebounceState {
            events,
            pending: Vec::new(),
        },
        step,
    )
}

async fn step(mut state: DebounceState) -> Option<(Vec<DirEvent>, DebounceState)> {
    loop {
        if state.pending.is_empty() {
            match state.events.next().await {
                Some(event) => state.pending.push(event),
                None => return None,
            }
            continue;
        }

        if state.pending.len() >= MAX_BATCH {
            let batch = std::mem::take(&mut state.pending);
            return Some((batch, state));
        }

        match tokio::time::timeout(DEBOUNCE_WINDOW, state.events.next()).await {
            Ok(Some(event)) => state.pending.push(event),
            // The backend stream ended mid-burst (the view navigated away
            // and dropped the watch, or the watched directory itself
            // vanished): flush what's buffered now; the next call to
            // `step` will see `events.next()` return `None` immediately
            // and end the subscription cleanly.
            Ok(None) => {
                let batch = std::mem::take(&mut state.pending);
                return Some((batch, state));
            }
            Err(_elapsed) => {
                let batch = std::mem::take(&mut state.pending);
                return Some((batch, state));
            }
        }
    }
}

impl DirectoryView {
    /// `update`'s `Message::WatchRefreshed` arm, moved here verbatim.
    pub(super) fn on_watch_refreshed(
        &mut self,
        location: Location,
        results: Vec<(OsString, Option<FileEntry>)>,
    ) -> (Task<Message>, Option<Event>) {
        if location != self.location {
            // A later navigation raced ahead of this fetch.
            return (Task::none(), None);
        }
        for (name, entry) in results {
            match entry {
                Some(entry) => self.upsert_entry(entry),
                // Vanished again before the fetch completed (a
                // create-then-immediate-delete race) — nothing to
                // insert, and any watched `Removed` for it either
                // already ran or is still in flight and will.
                None => self.remove_entry_by_name(&name),
            }
        }
        self.recompute_visible();
        (Task::none(), self.thumbnail_event())
    }

    /// Applies one debounced batch of watch events (Stage 5) — see the
    /// module docs' "Live updates" section for the split between what
    /// happens synchronously here vs. in `Message::WatchRefreshed`.
    pub(super) fn apply_watch_events(&mut self, events: Vec<DirEvent>) -> Task<Message> {
        // Any `Overflow` in the batch means the backend can no longer
        // promise it reported every change (see `DirEvent::Overflow`'s
        // docs) — the rest of the batch is superseded by a full re-list,
        // not worth applying first.
        if events
            .iter()
            .any(|event| matches!(event, DirEvent::Overflow))
        {
            return self.load();
        }

        // Deduplicated: a rapid create-then-modify (e.g. `touch`, or a
        // download landing) can produce two events for the same name
        // within one batch, and there's no reason to fetch its metadata
        // twice.
        let mut to_fetch: std::collections::HashSet<OsString> = std::collections::HashSet::new();
        for event in events {
            match event {
                DirEvent::Created(name) | DirEvent::Changed(name) => {
                    to_fetch.insert(name);
                }
                DirEvent::Removed(name) => {
                    self.remove_entry_by_name(&name);
                    self.selection.forget(&name);
                }
                DirEvent::Renamed { from, to } => {
                    self.selection.rename(&from, to.clone());
                    self.remove_entry_by_name(&from);
                    to_fetch.insert(to);
                }
                // Already handled by the early return above, which scans
                // the same `events` this loop was built from — a no-op
                // here rather than `unreachable!()`, per CLAUDE.md's
                // no-panic rule: if that invariant is ever wrong, silently
                // skipping one event is a far better failure mode than
                // taking the app down.
                DirEvent::Overflow => {}
            }
        }
        // One resort for every synchronous removal/rename in the batch,
        // not one per event — the "debounced re-sort" the stage calls
        // for. `Message::WatchRefreshed` below does the second (and last)
        // one, for whatever this call kicks off fetching.
        self.recompute_visible();

        if to_fetch.is_empty() {
            return Task::none();
        }

        let location = self.location.clone();
        let message_location = self.location.clone();
        Task::perform(
            async move {
                let backend = crate::modules::resolve(&location);
                let mut results = Vec::with_capacity(to_fetch.len());
                for name in to_fetch {
                    let entry = match &backend {
                        Some(backend) => backend.metadata(&location.join(&name)).await.ok(),
                        None => None,
                    };
                    results.push((name, entry));
                }
                results
            },
            move |results| Message::WatchRefreshed(message_location.clone(), results),
        )
    }

    /// Removes `name` from `entries` if present — the synchronous half of
    /// applying a `DirEvent::Removed`/rename's "from" half. A name that
    /// isn't there (already removed, or never listed) is a no-op, not an
    /// error: watch events and the in-flight listing they're layered on
    /// top of can race harmlessly.
    fn remove_entry_by_name(&mut self, name: &OsStr) {
        self.entries.retain(|entry| entry.name.as_os_str() != name);
    }

    /// Inserts `entry`, or overwrites the existing row with the same name
    /// — the shared tail of applying a `DirEvent::Created`/`Changed`/
    /// rename's "to" half once its metadata has come back.
    fn upsert_entry(&mut self, entry: FileEntry) {
        match self
            .entries
            .iter_mut()
            .find(|existing| existing.name == entry.name)
        {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::ffi::OsString;

    fn created(name: &str) -> DirEvent {
        DirEvent::Created(OsString::from(name))
    }

    /// Drives `debounced` over a stream that fires several events with no
    /// gap, then ends — proves a burst collapses into one batch (not one
    /// message per event) without needing to fake real time (the "quiet
    /// gap" flush itself needs a live timer/tokio runtime and is exercised
    /// by `modules::local`'s own temp-dir tests plus the manual
    /// done-criterion instead).
    #[tokio::test]
    async fn a_burst_with_no_gaps_collapses_into_one_batch_when_the_source_ends() {
        let raw = stream::iter(vec![created("a"), created("b"), created("c")]).boxed();
        let mut batches = debounced(raw).boxed();
        let batch = batches.next().await.unwrap();
        assert_eq!(batch, vec![created("a"), created("b"), created("c")]);
        assert!(batches.next().await.is_none());
    }

    #[tokio::test]
    async fn an_empty_source_stream_produces_no_batches() {
        let raw = stream::empty::<DirEvent>().boxed();
        let mut batches = debounced(raw).boxed();
        assert!(batches.next().await.is_none());
    }

    #[tokio::test]
    async fn a_batch_flushes_early_once_it_hits_the_max_size() {
        let events: Vec<DirEvent> = (0..(MAX_BATCH + 5))
            .map(|i| created(&format!("f{i}")))
            .collect();
        let raw = stream::iter(events.clone()).boxed();
        let mut batches = debounced(raw).boxed();

        let first = batches.next().await.unwrap();
        assert_eq!(first.len(), MAX_BATCH);

        let second = batches.next().await.unwrap();
        assert_eq!(second.len(), 5);
    }

    #[test]
    fn a_location_with_no_backend_subscribes_to_an_empty_stream() {
        let location = Location {
            scheme: "gopher".to_owned(),
            authority: None,
            path: std::path::PathBuf::from("/"),
        };
        // No panic, no runtime needed — `build` degrades synchronously
        // before ever touching a stream.
        let _ = build(&location);
    }

    // ── Stage 5: watch events ────────────────────────────────────────────

    #[test]
    fn watch_created_is_applied_only_once_metadata_comes_back() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_watch_events_for_test(vec![DirEvent::Created(OsString::from("b"))]);
        // The name alone isn't enough to add a row — `entries` is
        // untouched until `WatchRefreshed` supplies the metadata.
        assert_eq!(view.entries, vec![file("a")]);

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("b"), Some(file("b")))],
        ));
        assert!(event.is_none());
        assert!(view.entries.contains(&file("b")));
    }

    #[test]
    fn watch_removed_drops_the_entry_and_its_selection_immediately() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        view.selection.click(0, OsString::from("a"));
        assert!(view.selection.is_selected(OsStr::new("a")));

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Removed(OsString::from("a"))]);

        assert!(!view.entries.iter().any(|e| e.name == "a"));
        assert!(!view.selection.is_selected(OsStr::new("a")));
        assert!(view.entries.contains(&file("b")));
    }

    #[test]
    fn watch_rename_keeps_the_renamed_entry_selected() {
        let mut view = listed_view(vec![file("old.txt")]);
        view.selection.click(0, OsString::from("old.txt"));

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Renamed {
            from: OsString::from("old.txt"),
            to: OsString::from("new.txt"),
        }]);
        // The "from" half is gone immediately; the "to" half isn't a row
        // yet (still awaiting its metadata fetch) but is already selected
        // — `Selection` is name-keyed, so this holds true even before the
        // row exists.
        assert!(!view.entries.iter().any(|e| e.name == "old.txt"));
        assert!(!view.selection.is_selected(OsStr::new("old.txt")));
        assert!(view.selection.is_selected(OsStr::new("new.txt")));

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("new.txt"), Some(file("new.txt")))],
        ));
        assert!(event.is_none());
        assert!(view.entries.contains(&file("new.txt")));
        assert!(view.selection.is_selected(OsStr::new("new.txt")));
    }

    #[test]
    fn watch_changed_refreshes_an_existing_entrys_metadata() {
        let mut view = listed_view(vec![file("a")]);
        let mut updated = file("a");
        updated.size = 999;

        let _ = view.apply_watch_events_for_test(vec![DirEvent::Changed(OsString::from("a"))]);
        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("a"), Some(updated.clone()))],
        ));
        assert!(event.is_none());
        assert_eq!(view.entries, vec![updated]);
    }

    #[test]
    fn watch_overflow_falls_back_to_a_full_reload_and_drops_the_rest_of_the_batch() {
        let mut view = listed_view(vec![file("a")]);
        assert!(!view.is_loading());

        let _ = view.apply_watch_events_for_test(vec![
            DirEvent::Created(OsString::from("b")),
            DirEvent::Overflow,
        ]);
        // `load()` sets `loading` synchronously, before its `Task` resolves
        // — the same signal `Action::Refresh`'s own test checks.
        assert!(view.is_loading());
    }

    #[test]
    fn watch_refreshed_for_a_stale_location_is_dropped() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.navigate(Location::local("/elsewhere"));

        let (_, event) = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("stale"), Some(file("stale")))],
        ));
        assert!(event.is_none());
        assert!(!view.entries.iter().any(|e| e.name == "stale"));
    }

    #[test]
    fn watch_refreshed_removes_an_entry_that_vanished_before_the_fetch_landed() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(Message::WatchRefreshed(
            Location::local("/home"),
            vec![(OsString::from("a"), None)],
        ));
        assert!(!view.entries.iter().any(|e| e.name == "a"));
    }

    impl DirectoryView {
        /// Test-only shim: `apply_watch_events` is private (only reached
        /// via `Message::Watch` normally), but driving it directly keeps
        /// these tests from having to build a real `watch::subscription`
        /// stream (which needs a live tokio runtime and a real inotify
        /// watch — exercised instead by `modules::local`'s own temp-dir
        /// tests and the stage's manual done-criterion).
        fn apply_watch_events_for_test(&mut self, events: Vec<DirEvent>) -> Task<Message> {
            self.apply_watch_events(events)
        }
    }
}
