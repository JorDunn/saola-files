//! Thumbnails (Stage 11): the cache/concurrency constants, dispatching
//! background generation for viewport-visible rows, and folding a
//! finished generation back into the shared cache.

use std::time::SystemTime;

use iced::Task;

use core::vfs::Location;
use saola_files::{core, ui};

use super::{App, Message};

/// The App-owned decoded-thumbnail LRU's capacity (Stage 11 — CLAUDE.md's
/// "~512-handle LRU"). A plain implementation constant, the same posture
/// `modules::local`'s `CHANNEL_CAPACITY`/`WATCH_CHANNEL_CAPACITY` already
/// take for their own bounded-resource sizes: not derived from the theme
/// (it bounds a background cache, not anything rendered), not a config
/// knob (nothing in `files.toml`'s documented shape covers it, and a
/// wrong value here degrades gracefully — a smaller cache just means more
/// re-decodes on scroll-back, never a correctness issue).
pub(super) const THUMB_LRU_CAPACITY: usize = 512;

/// How many thumbnails may be generated concurrently (Stage 11 — CLAUDE.md:
/// "never unbounded task spawns"). Small on purpose: thumbnail generation
/// is blocking CPU work (image decode + resize) sharing the same blocking
/// pool `modules::local`'s file I/O already uses; a handful at a time keeps
/// a 5k-image directory's initial scroll responsive rather than saturating
/// every blocking-pool thread with decode work at once.
pub(super) const THUMB_MAX_CONCURRENT: usize = 4;

impl App {
    /// `Event::ThumbnailsNeeded`'s handling (Stage 11): dispatches
    /// background generation for viewport-visible candidates the view
    /// bubbled up. Gated by `files.toml`'s `thumbnails`/`thumbnail-max-mb`
    /// knobs here — `ui::dirview` has no `Config` after construction (see
    /// `DirectoryView::new`'s own comment on why only a handful of
    /// config-derived fields survive past startup), and `core::thumbs` has
    /// no opinion on a UI-facing size cap of its own (see that module's
    /// doc comment) — so this is the one place both gates actually apply.
    /// Every dispatched location is tracked in `thumb_inflight` until its
    /// `Message::ThumbnailReady` lands, so a second viewport batch
    /// covering the same rows before the first finishes doesn't queue a
    /// duplicate generation for it.
    pub(super) fn request_thumbnails(
        &mut self,
        candidates: Vec<ui::dirview::ThumbCandidate>,
    ) -> Task<Message> {
        if !self.thumbnails_enabled {
            return Task::none();
        }
        let max_mb = self.thumbnail_max_mb;
        // Two passes, deliberately: every `filter`/`filter_map` closure
        // below only ever reads `self` (`thumb_cache`/`thumb_failed`/
        // `thumb_inflight`/`mime_db`), but the dispatch loop after it needs
        // to *mutate* `thumb_inflight` — one chained iterator expression
        // can't hold both an immutable borrow (for the filters) and a
        // mutable one (for the final step) live at the same time, since
        // every closure in a single chain is constructed before any of
        // them run. Collecting the filtered candidates first ends that
        // immutable borrow before the mutating loop starts.
        let filtered: Vec<(ui::dirview::ThumbCandidate, String)> = candidates
            .into_iter()
            .filter(|candidate| !core::thumbs::exceeds_max_size(candidate.size_bytes, max_mb))
            .filter(|candidate| {
                self.thumb_cache
                    .get_for(&candidate.location, candidate.modified)
                    .is_none()
            })
            .filter(|candidate| {
                self.thumb_failed.get(&candidate.location) != Some(&candidate.modified)
            })
            .filter(|candidate| !self.thumb_inflight.contains(&candidate.location))
            .filter_map(|candidate| {
                let name = candidate.location.path.file_name()?;
                let mimetype = self.mime_db.guess(name, None);
                Some((candidate, mimetype))
            })
            .collect();

        let tasks: Vec<Task<Message>> = filtered
            .into_iter()
            .map(|(candidate, mimetype)| {
                self.thumb_inflight.insert(candidate.location.clone());
                let request = core::thumbs::ThumbRequest {
                    location: candidate.location.clone(),
                    mimetype,
                    modified: candidate.modified,
                };
                let registry = self.thumb_registry.clone();
                let semaphore = self.thumb_semaphore.clone();
                let cache_root = self.thumb_cache_root.clone();
                let location = candidate.location.clone();
                let modified = candidate.modified;
                Task::perform(
                    core::thumbs::thumbnail_for(registry, semaphore, cache_root, request),
                    move |handle| Message::ThumbnailReady(location.clone(), modified, handle),
                )
            })
            .collect();
        Task::batch(tasks)
    }

    /// `Message::ThumbnailReady`'s handling (Stage 11) — see that
    /// variant's doc comment.
    pub(super) fn handle_thumbnail_ready(
        &mut self,
        location: Location,
        modified: SystemTime,
        handle: Option<core::thumbs::ThumbHandle>,
    ) -> Task<Message> {
        self.thumb_inflight.remove(&location);
        match handle {
            Some(handle) => {
                self.thumb_cache.insert(location.clone(), modified, handle);
                self.thumb_failed.remove(&location);
            }
            None => {
                self.thumb_failed.insert(location, modified);
            }
        }
        Task::none()
    }
}
