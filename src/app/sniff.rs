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

use iced::Task;
use iced::futures::StreamExt;

use core::mime::SNIFF_BYTES;
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
        let mut batch: Vec<(Location, SystemTime)> = Vec::new();
        for candidate in candidates {
            let location = &candidate.location;
            let (Some(dir), Some(name)) = (location.parent(), location.path.file_name()) else {
                continue;
            };
            if candidate.size_bytes == 0
                || self.sniff_inflight.contains(location)
                || !self
                    .mime_db
                    .wants_sniff(&dir, name, Some(candidate.modified))
                || !has_local_path(location)
            {
                continue;
            }
            batch.push((location.clone(), candidate.modified));
        }
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
        for (location, modified, head) in outcomes {
            self.sniff_inflight.remove(&location);
            let (Some(dir), Some(name)) = (location.parent(), location.path.file_name()) else {
                continue;
            };
            match head {
                Some(bytes) => self.mime_db.record_sniff(&dir, name, modified, &bytes),
                None => self.mime_db.record_unsniffable(&dir, name, modified),
            }
        }
        Task::none()
    }

    /// The mtime of the selected entry that `location` names in view `id`,
    /// so opening it can use that entry's cached sniff verdict.
    pub(super) fn selected_mtime(
        &self,
        id: ui::explorer::ViewId,
        location: &Location,
    ) -> Option<SystemTime> {
        let name = location.path.file_name()?;
        ui::explorer::find(&self.views, id)?
            .selected_entries()
            .into_iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.modified)
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
