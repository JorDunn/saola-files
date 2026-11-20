//! Content sniffing: for visible, local files whose *name* gives no type
//! (`README`, `picture`, a misnamed file), read the first few KiB in the
//! background and let `core::mime` turn them into a mimetype, so the row
//! glyph and the default app are right.
//!
//! When it runs:
//! - only for the files `Event::ThumbnailsNeeded` already names — the
//!   viewport plus a little overscan, never the whole directory, so listing
//!   a 10k-file folder reads a few dozen heads, not 10k;
//! - only for backends that can hand out a local path (`Caps::LOCAL_PATH`):
//!   a remote listing never triggers a read (no surprise network cost);
//! - only when `MimeDb::wants_sniff` says so (untyped name, nothing cached
//!   for this mtime) and the file is non-empty.

use std::time::SystemTime;

use std::collections::HashSet;

use iced::Task;
use iced::futures::StreamExt;

use core::mime::{MimeDb, SNIFF_BYTES};
use core::vfs::{Caps, Location};
use saola_files::{core, modules, ui};

use super::{App, Message};

/// One finished head read: which file (and which mtime it was read for),
/// and its leading bytes, or `None` if the read failed.
pub(crate) type SniffOutcome = (Location, SystemTime, Option<Vec<u8>>);

impl App {
    /// Starts one background task that reads the heads of every candidate
    /// still wanting a sniff. One task per batch, reading one file at a
    /// time, is the concurrency bound: a scroll can never fan out into an
    /// unbounded number of simultaneous reads.
    pub(super) fn request_sniffs(
        &mut self,
        candidates: &[ui::dirview::ThumbCandidate],
    ) -> Task<Message> {
        let batch = select_for_sniff(
            &self.mime_db,
            &self.sniff_inflight,
            candidates,
            has_local_path,
        );
        if batch.is_empty() {
            return Task::none();
        }
        self.sniff_inflight
            .extend(batch.iter().map(|(location, _)| location.clone()));
        Task::perform(read_heads(batch), Message::SniffReady)
    }

    /// `Message::SniffReady`'s handling: fold each head into the shared
    /// `MimeDb` cache. iced re-runs `view` after any update, so the new
    /// glyphs appear without further plumbing.
    pub(super) fn handle_sniff_ready(&mut self, outcomes: Vec<SniffOutcome>) -> Task<Message> {
        apply_outcomes(&self.mime_db, &mut self.sniff_inflight, outcomes);
        Task::none()
    }

    /// The mtime of the entry `location` names in view `id` (selected or
    /// not — Enter acts on the cursor row when nothing is selected), so
    /// opening it can use that entry's cached sniff verdict.
    pub(super) fn entry_mtime(
        &self,
        id: ui::explorer::ViewId,
        location: &Location,
    ) -> Option<SystemTime> {
        let name = location.path.file_name()?;
        ui::explorer::find(&self.views, id)?.modified_of(name)
    }
}

/// Which candidates deserve a head read: non-empty, not already in
/// flight, a name that wants a sniff, and a backend that `is_local` says
/// can serve a local path. Pure (the backend check is injected) so the
/// filtering is unit-tested without an `App`.
fn select_for_sniff(
    mime_db: &MimeDb,
    inflight: &HashSet<Location>,
    candidates: &[ui::dirview::ThumbCandidate],
    is_local: impl Fn(&Location) -> bool,
) -> Vec<(Location, SystemTime)> {
    let mut batch = Vec::new();
    for candidate in candidates {
        let location = &candidate.location;
        let (Some(dir), Some(name)) = (location.parent(), location.path.file_name()) else {
            continue;
        };
        if candidate.size_bytes == 0
            || inflight.contains(location)
            || !mime_db.wants_sniff(&dir, name, Some(candidate.modified))
            || !is_local(location)
        {
            continue;
        }
        batch.push((location.clone(), candidate.modified));
    }
    batch
}

/// Folds finished reads into the cache and clears their in-flight marks,
/// failed reads included (so a failure is not retried until the mtime
/// changes, and the file is not stuck "in flight" forever).
fn apply_outcomes(mime_db: &MimeDb, inflight: &mut HashSet<Location>, outcomes: Vec<SniffOutcome>) {
    for (location, modified, head) in outcomes {
        inflight.remove(&location);
        let (Some(dir), Some(name)) = (location.parent(), location.path.file_name()) else {
            continue;
        };
        match head {
            Some(bytes) => mime_db.record_sniff(&dir, name, modified, &bytes),
            None => mime_db.record_unsniffable(&dir, name, modified),
        }
    }
}

/// Whether `location`'s backend can serve a local path — the capability
/// that makes reading a file's head free of network cost.
fn has_local_path(location: &Location) -> bool {
    modules::resolve(location).is_some_and(|backend| backend.caps().contains(Caps::LOCAL_PATH))
}

/// Reads the first [`SNIFF_BYTES`] of each file, sequentially.
async fn read_heads(batch: Vec<(Location, SystemTime)>) -> Vec<SniffOutcome> {
    let mut outcomes = Vec::with_capacity(batch.len());
    for (location, modified) in batch {
        let head = read_head(&location).await;
        outcomes.push((location, modified, head));
    }
    outcomes
}

/// The first [`SNIFF_BYTES`] of `location` via `Backend::read`, or `None`
/// on any error (EACCES is normal; the caller records it as unsniffable).
async fn read_head(location: &Location) -> Option<Vec<u8>> {
    let backend = modules::resolve(location)?;
    let mut stream = backend.read(location).await.ok()?;
    let mut head: Vec<u8> = Vec::with_capacity(SNIFF_BYTES);
    // Backends stream in chunks (local: 64 KiB), so usually one iteration.
    // Dropping `stream` afterwards tells the backend's reader to stop —
    // that is how we avoid reading the whole file.
    while head.len() < SNIFF_BYTES {
        match stream.next().await {
            Some(Ok(chunk)) => head.extend_from_slice(&chunk),
            Some(Err(_)) if head.is_empty() => return None,
            Some(Err(_)) | None => break,
        }
    }
    // An empty head would read as "zero-length file" to the sniffer; the
    // caller already skips size-0 files, so an empty head here means the
    // file shrank mid-read — treat it as unreadable rather than guess.
    if head.is_empty() {
        return None;
    }
    head.truncate(SNIFF_BYTES);
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use saola_files::ui::dirview::ThumbCandidate;
    use std::time::Duration;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn candidate(path: &str, size: u64) -> ThumbCandidate {
        ThumbCandidate {
            location: Location::local(path),
            size_bytes: size,
            modified: t(1),
        }
    }

    #[test]
    fn a_local_untyped_file_is_selected_for_sniffing() {
        let db = MimeDb::new();
        let picked = select_for_sniff(&db, &HashSet::new(), &[candidate("/d/picture", 9)], |_| {
            true
        });
        assert_eq!(picked, vec![(Location::local("/d/picture"), t(1))]);
    }

    #[test]
    fn a_backend_without_local_path_queues_nothing() {
        let db = MimeDb::new();
        let picked = select_for_sniff(&db, &HashSet::new(), &[candidate("/d/picture", 9)], |_| {
            false
        });
        assert!(picked.is_empty());
    }

    #[test]
    fn empty_typed_and_in_flight_files_are_skipped() {
        let db = MimeDb::new();
        let mut inflight = HashSet::new();
        inflight.insert(Location::local("/d/busy"));
        let picked = select_for_sniff(
            &db,
            &inflight,
            &[
                candidate("/d/empty", 0),
                candidate("/d/notes.txt", 9),
                candidate("/d/busy", 9),
            ],
            |_| true,
        );
        assert!(picked.is_empty());
    }

    #[test]
    fn a_failed_read_clears_in_flight_and_is_not_retried() {
        let db = MimeDb::new();
        let location = Location::local("/d/locked");
        let mut inflight = HashSet::from([location.clone()]);

        apply_outcomes(&db, &mut inflight, vec![(location.clone(), t(1), None)]);

        assert!(inflight.is_empty());
        let again = select_for_sniff(&db, &inflight, &[candidate("/d/locked", 9)], |_| true);
        assert!(again.is_empty());
    }

    fn scratch_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "saola-files-sniff-test-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn read_head_stops_at_the_sniff_budget() {
        let dir = scratch_dir();
        let path = dir.join("big");
        std::fs::write(&path, vec![7u8; SNIFF_BYTES * 3]).unwrap();

        let head = read_head(&Location::local(&path)).await;
        assert_eq!(head.map(|bytes| bytes.len()), Some(SNIFF_BYTES));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_head_returns_a_short_file_whole_and_none_for_a_missing_one() {
        let dir = scratch_dir();
        let path = dir.join("small");
        std::fs::write(&path, b"abc").unwrap();

        let whole = read_head(&Location::local(&path)).await;
        assert_eq!(whole, Some(b"abc".to_vec()));
        let missing = read_head(&Location::local(dir.join("missing"))).await;
        assert_eq!(missing, None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
