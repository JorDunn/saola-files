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

use std::time::Duration;

use iced::Subscription;
use iced::futures::stream::{self, BoxStream, Stream, StreamExt};

use crate::core::vfs::{DirEvent, Location};

use super::Message;

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
    let Some(backend) = crate::modules::resolve(&location.scheme) else {
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

#[cfg(test)]
mod tests {
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
}
