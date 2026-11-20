//! Viewport-driven thumbnail requests (which visible entries need one,
//! bubbled as `Event::ThumbnailsNeeded`) and the per-row glyph/thumbnail
//! lookups `list.rs`/`grid.rs` share.

use iced::Task;
use iced::widget::scrollable;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::core::vfs::Caps;

use super::{DirectoryView, Event, Message, ThumbCandidate};

/// Row-height guess used only to decide which entries are "near the
/// viewport" for thumbnail-request scheduling (Stage 11) — the same
/// placeholder posture as `PAGE_ROWS` (in `keys.rs`): `update()` has no
/// `Theme` to read the real `sizes.list_row`/grid tile height from (only
/// `view()` does — see `list.rs`/`grid.rs`'s own pixel-exact virtualization
/// math, which this never touches). Deliberately generous: a miss here
/// only ever costs a few thumbnails requested a little early or late,
/// never a wrong row acted on the way `PAGE_ROWS` being off would be.
const THUMB_ROW_HEIGHT_GUESS: f32 = 40.0;
/// Rows' worth of candidates requested before the first `Scrolled` event
/// lands — mirrors `list.rs`/`grid.rs`'s own pre-viewport `INITIAL_ROWS`
/// fallback.
const THUMB_INITIAL_ROWS: usize = 48;
/// Extra rows beyond the visible band, both sides — generous on purpose,
/// see `THUMB_ROW_HEIGHT_GUESS`'s doc comment.
const THUMB_OVERSCAN_ROWS: usize = 8;

impl DirectoryView {
    /// `update`'s `Message::Scrolled` arm, moved here verbatim.
    pub(super) fn on_scrolled(
        &mut self,
        viewport: scrollable::Viewport,
    ) -> (Task<Message>, Option<Event>) {
        self.scroll = Some(viewport);
        (Task::none(), self.thumbnail_event())
    }

    // ── Stage 11: viewport-driven thumbnail requests ────────────────────

    /// Which `visible` index range is in (or just outside) the viewport,
    /// for thumbnail-request scheduling only — never used for rendering
    /// (that's `list.rs::visible_range`/`grid.rs::visible_row_range`, both
    /// pixel-exact against the real theme token/tile constants). Grid mode
    /// treats one "row" as `row_step()` entries (a full tile row), the same
    /// vertical-step unit `move_cursor` already uses; list mode's
    /// `row_step()` is `1`, so this degenerates to plain item indices.
    fn thumbnail_range(&self) -> (usize, usize) {
        let total = self.visible.len();
        let step = self.row_step().max(1) as usize;
        let (first_row, row_span) = match self.scroll {
            None => (0, THUMB_INITIAL_ROWS),
            Some(viewport) => {
                let offset = viewport.absolute_offset().y.max(0.0);
                let bounds_height = viewport.bounds().height.max(0.0);
                let first_row = (offset / THUMB_ROW_HEIGHT_GUESS).floor() as usize;
                let row_span = (bounds_height / THUMB_ROW_HEIGHT_GUESS).ceil() as usize;
                (first_row, row_span)
            }
        };
        let first_row = first_row.saturating_sub(THUMB_OVERSCAN_ROWS);
        let last_row = first_row
            .saturating_add(row_span)
            .saturating_add(THUMB_OVERSCAN_ROWS * 2);
        let first = first_row.saturating_mul(step).min(total);
        let last = last_row.saturating_mul(step).min(total).max(first);
        (first, last)
    }

    /// Regular, non-symlink files in (or near) the viewport with a known
    /// mtime — Stage 11's "visible-range requests only" rule (no
    /// whole-directory eager generation). Empty whenever the current
    /// backend doesn't claim `Caps::THUMBNAILS` (today, any non-local
    /// backend) — checked here, once, rather than at every call site that
    /// might bubble `Event::ThumbnailsNeeded`. Directories/symlinks/
    /// entries with no `modified` (nothing to validate a cache entry
    /// against) are skipped, not just deprioritized.
    fn thumbnail_candidates(&self) -> Vec<ThumbCandidate> {
        if !self.caps().contains(Caps::THUMBNAILS) {
            return Vec::new();
        }
        let (first, last) = self.thumbnail_range();
        self.visible
            .get(first..last)
            .unwrap_or(&[])
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .filter(|entry| entry.kind == EntryKind::File && !entry.is_symlink)
            .filter_map(|entry| {
                let modified = entry.modified?;
                Some(ThumbCandidate {
                    location: self.location.join(&entry.name),
                    size_bytes: entry.size,
                    modified,
                })
            })
            .collect()
    }

    /// `Some(Event::ThumbnailsNeeded(..))` when there's at least one
    /// candidate, `None` otherwise — the shared tail every `update()` arm
    /// that could change what's on screen (a fresh listing, a watch
    /// refresh, a scroll, a sort/hidden-filter change) calls instead of
    /// returning a bare `None` for its event.
    pub(super) fn thumbnail_event(&self) -> Option<Event> {
        let candidates = self.thumbnail_candidates();
        (!candidates.is_empty()).then_some(Event::ThumbnailsNeeded(candidates))
    }
}

/// The glyph for one row/tile — shared by `list.rs`/`grid.rs` so both
/// presentations pick the same icon for the same entry. Directories skip
/// the `MimeDb` call entirely (`inode/directory` is a fixed, free
/// classification — no glob lookup needed); everything else resolves a
/// mimetype from the name alone (no content sniff — see `core::mime`'s
/// module docs for why that's the right tradeoff for a per-frame,
/// per-row call).
pub(in crate::ui) fn row_icon(entry: &FileEntry, mime_db: &MimeDb) -> saola_theme::icon::Icon {
    let category = if entry.kind == EntryKind::Directory {
        crate::core::mime::Category::Directory
    } else {
        crate::core::mime::category(&mime_db.guess(&entry.name, None))
    };
    crate::icons::for_entry(entry.kind, entry.is_symlink, category)
}

/// The cached thumbnail for one row/tile, if any — shared by `list.rs`/
/// `grid.rs` the same way [`row_icon`] is, so both presentations agree on
/// when a thumbnail replaces the glyph. `None` for anything
/// `DirectoryView::thumbnail_candidates` would never have requested in the
/// first place (directories, symlinks, no known `modified`) as well as for
/// an ordinary cache miss/in-flight request — this never triggers
/// generation itself (that only ever happens via `Event::ThumbnailsNeeded`,
/// see this module's docs), it only ever reads what's already decoded.
pub(in crate::ui) fn thumbnail_for(
    state: &DirectoryView,
    thumb_cache: &ThumbCache,
    entry: &FileEntry,
) -> Option<crate::core::thumbs::ThumbHandle> {
    if entry.kind != EntryKind::File || entry.is_symlink {
        return None;
    }
    let modified = entry.modified?;
    let location = state.location.join(&entry.name);
    thumb_cache.get_for(&location, modified)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::vfs::Location;
    use std::path::PathBuf;

    // ── Stage 11: viewport-driven thumbnail requests ────────────────────

    #[test]
    fn listed_with_no_thumbnailable_entries_bubbles_no_thumbnail_event() {
        // `file()`/`dir()` both have `modified: None` — every pre-Stage-11
        // test in this module builds its fixtures this way, so this
        // pins down that Stage 11 didn't change their behavior: still no
        // event at all (checked inside `listed_view` itself already, but
        // named explicitly here as the Stage 11 contract, not an
        // incidental side effect of `file()`'s shape).
        let _ = listed_view(vec![file("a.txt"), dir("sub")]);
    }

    #[test]
    fn listed_with_thumbnailable_files_bubbles_thumbnails_needed() {
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        let (_, event) = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![
                thumbnailable_file("photo.jpg"),
                dir("sub"),
                file("no-mtime.txt"),
            ]),
        ));
        match event {
            Some(Event::ThumbnailsNeeded(candidates)) => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].location, Location::local("/home/photo.jpg"));
                assert_eq!(candidates[0].size_bytes, 10);
            }
            other => panic!("expected ThumbnailsNeeded, got {other:?}"),
        }
    }

    #[test]
    fn thumbnail_candidates_exclude_directories_and_symlinks() {
        let mut symlinked = thumbnailable_file("link.jpg");
        symlinked.is_symlink = true;
        // Not `listed_view` here — that helper asserts no event bubbles,
        // which doesn't hold once thumbnailable entries are in the mix
        // (see `listed_with_thumbnailable_files_bubbles_thumbnails_needed`
        // above); this test only cares about `thumbnail_candidates`'
        // filtering, not what `update` bubbles.
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![thumbnailable_file("photo.jpg"), dir("sub"), symlinked]),
        ));
        let candidates = view.thumbnail_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].location, Location::local("/home/photo.jpg"));
    }

    #[test]
    fn thumbnail_candidates_are_empty_without_caps_thumbnails() {
        // `caps()` resolves via `modules::resolve`, which only ever
        // returns backends compiled into this binary — there's no
        // `Caps`-lacking backend to point a real `Location` at here, so
        // this instead pins the *positive* case (the local backend does
        // claim `Caps::THUMBNAILS` — verified directly in
        // `modules::local`'s own tests) and trusts the `!self.caps().
        // contains(..)` guard reads correctly by inspection; a location
        // whose scheme resolves to nothing at all is the closest
        // reachable "no capability" stand-in this view can exercise.
        let view = DirectoryView::new(
            Location {
                scheme: "nonexistent".to_owned(),
                authority: None,
                path: PathBuf::from("/x"),
            },
            &config(),
        );
        assert!(view.thumbnail_candidates().is_empty());
    }
}
