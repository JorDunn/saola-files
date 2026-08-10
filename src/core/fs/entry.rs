//! `FileEntry` — one row in a directory listing. Backends build these from
//! `symlink_metadata`-equivalents (never followed), so the model never
//! silently resolves a symlink into whatever it points at.
//!
//! Names stay `OsString` end-to-end: SFTP names arrive as bytes too, and
//! Linux paths are not necessarily valid UTF-8. `to_string_lossy` (via
//! [`FileEntry::display_name`]) happens only where a row is *drawn* — see
//! CLAUDE.md's OsString discipline.

use std::borrow::Cow;
use std::ffi::OsString;
use std::time::SystemTime;

/// What `symlink_metadata` reported for the entry itself — never resolved
/// through a symlink.
///
/// A symlink's own `file_type()` is neither "is_file" nor "is_dir" (that's
/// `is_symlink()`, tracked separately by [`FileEntry::is_symlink`]), so a
/// symlink entry's `kind` is [`EntryKind::Other`] regardless of what it
/// points at — including a symlink to a directory, which most file
/// managers would otherwise show as a folder. Resolving a symlink's target
/// kind for display (a common nicety) needs an extra stat call the backend
/// doesn't make in this stage; a later stage can add it as an
/// explicitly-opted-into resolution step, not a silent follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

/// One row a `Backend::list` call returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: OsString,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// True when `symlink_metadata` reported this entry itself as a
    /// symlink. Backends never follow it to classify `kind` — see
    /// [`EntryKind`]'s docs. The UI differentiates it with a glyph, never a
    /// color (style guide: mimetype/kind differentiation is glyph shape
    /// only).
    pub is_symlink: bool,
    /// Unix permission bits (`st_mode & 0o7777`), when the backend can
    /// report them. `modules::local` always sets this (a plain
    /// `PermissionsExt::mode()` read, already available from the same
    /// `symlink_metadata`/`DirEntry::metadata` call that builds the rest of
    /// this struct); a future non-Unix or protocol backend that has no such
    /// concept (or chooses not to expose it) leaves this `None`, and
    /// `ui::dialogs::properties` renders no permissions row rather than
    /// fabricating one. **Read-only today**: `Caps::SET_PERMISSIONS` is
    /// still "a future trait method — no backend sets this bit yet" (see
    /// that flag's own doc comment) — this field exists so the properties
    /// dialog has real data to *show*, not because anything can change it
    /// yet.
    pub mode: Option<u32>,
}

impl FileEntry {
    /// Lossy display form of `name` — the one sanctioned `to_string_lossy`
    /// call site in the row-rendering path; every other consumer keeps the
    /// `OsString` untouched.
    pub fn display_name(&self) -> Cow<'_, str> {
        self.name.to_string_lossy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn non_utf8_name_round_trips_and_displays_lossily() {
        let raw = OsStr::from_bytes(b"\xff\xfe").to_os_string();
        let entry = FileEntry {
            name: raw.clone(),
            kind: EntryKind::File,
            size: 0,
            modified: None,
            is_symlink: false,
            mode: None,
        };

        // The bytes survive untouched in the model...
        assert_eq!(entry.name, raw);
        assert_eq!(entry.name.as_bytes(), b"\xff\xfe");
        // ...and the display path never panics, degrading to replacement
        // characters instead of losing the row entirely.
        assert!(entry.display_name().contains('\u{FFFD}'));
    }

    #[test]
    fn utf8_name_displays_unchanged() {
        let entry = FileEntry {
            name: OsString::from("résumé.pdf"),
            kind: EntryKind::File,
            size: 1024,
            modified: None,
            is_symlink: false,
            mode: None,
        };
        assert_eq!(entry.display_name(), "résumé.pdf");
    }
}
