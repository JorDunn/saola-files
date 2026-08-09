//! Inline rename (F2 / the context menu's "Rename…") and the name-
//! uniquification helper New Folder/New File both use.
//!
//! [`RenameState`] lives on `DirectoryView` (`mod.rs`'s private `rename`
//! field) rather than getting its own file-scoped state struct with a
//! `Message`/`update` of its own — there is exactly one row being renamed
//! at a time, per view, the same "one thing, not a collection" shape
//! `path_edit`/`type_ahead` already have, and `mod.rs`'s `Message::Rename*`
//! variants are few enough not to earn a submodule `Message` the way
//! `typeahead`'s own internal state machine did.
//!
//! Renaming itself is deliberately **not** routed through
//! `core::fs::ops`'s engine: it's one `Backend::rename` call, already
//! `async` and already cheap (same-directory, no streaming, no conflict
//! prompt — a name collision just surfaces as an inline error and leaves
//! the field open to retry, see `mod.rs`'s `Message::RenameResult`). Wiring
//! it through the ops engine's progress/conflict machinery for a single
//! metadata-only call would be ceremony with no payoff.

use std::ffi::{OsStr, OsString};

use crate::core::fs::entry::FileEntry;

/// Widget id for the inline rename `text_input`. Shared between "who
/// builds the widget" (`list.rs`/`grid.rs`) and "who issues the focus/
/// select_all operations" (`DirectoryView::start_rename_named`) — the same
/// split `breadcrumbs::PATH_INPUT_ID` already has, and for the same reason.
pub const RENAME_INPUT_ID: &str = "saola-files-rename-input";

/// Inline-rename UI state: `Some` on `DirectoryView` while a row is
/// mid-edit.
#[derive(Debug, Clone)]
pub struct RenameState {
    /// The entry's name *before* editing — what `Message::RenameSubmitted`
    /// compares the typed text against (typing the same name back is a
    /// no-op, not a rename) and what the eventual `Backend::rename` call's
    /// `from` half is built from. Never touched again after
    /// `RenameState::new` — renaming what a stale index would have pointed
    /// at is exactly what this avoids: the target is fixed by name at the
    /// moment F2/"Rename…" is pressed, not re-resolved from a row position
    /// that could shift under a concurrent watch event.
    pub original: OsString,
    /// The live `text_input` buffer.
    pub buffer: String,
    /// Set after a failed `Backend::rename` (a name collision, permission
    /// denial, …) — the row stays in edit mode so the typed name isn't
    /// lost, and the message is worded inline rather than in a modal
    /// (CLAUDE.md's capability-honest posture: EACCES is a normal
    /// Tuesday). Cleared the moment the buffer changes again.
    pub error: Option<String>,
}

impl RenameState {
    pub fn new(original: OsString) -> Self {
        let buffer = original.to_string_lossy().into_owned();
        RenameState {
            original,
            buffer,
            error: None,
        }
    }
}

/// A destination name known not to collide with anything in `existing` —
/// `base`, then `base (2)`, `base (3)`, … the same "keep both" numbering
/// `core::fs::ops::unique_rename_dest` uses for copy conflicts, just
/// without a "(copy)" segment (there is nothing here to be a copy *of*: a
/// fresh "New Folder" and a second fresh "New Folder" are two independent
/// new things, not one file colliding with another).
///
/// Checked against `existing` (the view's already-loaded `entries`) rather
/// than a fresh `Backend::list`/`metadata` round trip — good enough for
/// picking an unclaimed starting name; a genuine race (something else
/// creates the exact same name between this check and the real
/// `Backend::mkdir`/`write` call) still surfaces as an ordinary
/// `VfsError::AlreadyExists` the caller words normally, same as any other
/// backend failure.
pub fn unique_name(existing: &[FileEntry], base: &str) -> OsString {
    if !name_taken(existing, base) {
        return OsString::from(base);
    }
    for n in 2..1000u32 {
        let candidate = format!("{base} ({n})");
        if !name_taken(existing, &candidate) {
            return OsString::from(candidate);
        }
    }
    // Every one of 999 candidates is taken — vanishingly unlikely, but the
    // no-panic rule means this still has to return *something* rather than
    // unwrap a `None`. The caller's `Backend::mkdir`/`write` call will
    // simply surface `AlreadyExists`, which is an honest outcome for a
    // directory that genuinely has a thousand same-prefixed entries.
    OsString::from(base)
}

fn name_taken(existing: &[FileEntry], candidate: &str) -> bool {
    existing
        .iter()
        .any(|entry| entry.name == OsStr::new(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fs::entry::EntryKind;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size: 0,
            modified: None,
            is_symlink: false,
        }
    }

    #[test]
    fn rename_state_seeds_the_buffer_from_the_original_name() {
        let state = RenameState::new(OsString::from("report.txt"));
        assert_eq!(state.buffer, "report.txt");
        assert_eq!(state.original, OsString::from("report.txt"));
        assert!(state.error.is_none());
    }

    #[test]
    fn unique_name_returns_the_base_when_nothing_collides() {
        let existing = vec![entry("readme.txt")];
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder");
    }

    #[test]
    fn unique_name_numbers_past_a_collision() {
        let existing = vec![entry("New Folder"), entry("New Folder (2)")];
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder (3)");
    }

    #[test]
    fn unique_name_fills_a_gap_left_by_a_deleted_middle_candidate() {
        let existing = vec![entry("New Folder"), entry("New Folder (3)")];
        // "New Folder (2)" is free even though "(3)" is taken — the
        // numbering always starts back at 2, it doesn't remember history.
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder (2)");
    }
}
