//! Shared test fixtures for the `dirview` submodules' tests (compiled
//! only under `cfg(test)`).

use std::ffi::OsString;
use std::time::SystemTime;

use iced::Task;

use crate::config::Config;
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::Location;
use crate::keymap::Action;

use super::{DirectoryView, Event, Message};

pub(super) fn config() -> Config {
    Config::default()
}

pub(super) fn file(name: &str) -> FileEntry {
    FileEntry {
        name: OsString::from(name),
        kind: EntryKind::File,
        size: 10,
        modified: None,
        is_symlink: false,
        mode: None,
    }
}

pub(super) fn dir(name: &str) -> FileEntry {
    FileEntry {
        name: OsString::from(name),
        kind: EntryKind::Directory,
        size: 0,
        modified: None,
        is_symlink: false,
        mode: None,
    }
}

/// A regular file with a known `modified` time — the shape
/// `DirectoryView::thumbnail_candidates` actually requires (`file()`
/// above deliberately has `modified: None`, so it never qualifies —
/// see that helper's use across every pre-Stage-11 test in this
/// module, none of which should start bubbling `Event::
/// ThumbnailsNeeded` just because this stage exists).
pub(super) fn thumbnailable_file(name: &str) -> FileEntry {
    FileEntry {
        name: OsString::from(name),
        kind: EntryKind::File,
        size: 10,
        modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)),
        is_symlink: false,
        mode: None,
    }
}

/// Directly exercises `recompute_visible`/selection plumbing by
/// driving `update` with a synthetic `Listed` message — this is the
/// "selection model (via FakeBackend)" test the stage plan calls for;
/// `FakeBackend` itself is exercised in `core::vfs`'s own tests, and
/// this view intentionally resolves backends by scheme (never takes
/// one as a constructor argument), so we feed its `entries` the same
/// shape a `FakeBackend::list` call would have produced.
pub(super) fn listed_view(entries: Vec<FileEntry>) -> DirectoryView {
    let mut view = DirectoryView::new(Location::local("/home"), &config());
    let (_, event) = view.update(Message::Listed(Location::local("/home"), Ok(entries)));
    assert!(event.is_none());
    view
}

impl DirectoryView {
    /// Test-only shim: `apply_action` is private (only reached via
    /// `Message::Keyboard` normally), but driving it directly keeps
    /// these tests from having to fabricate `iced::keyboard::Event`s.
    pub(super) fn apply_action_for_test(
        &mut self,
        action: Action,
    ) -> (Task<Message>, Option<Event>) {
        self.apply_action(action)
    }
}
