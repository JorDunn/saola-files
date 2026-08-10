//! `MimeDb` — the mimetype-resolution seam: [`MimeDb::guess`] returns a
//! best-guess mimetype essence string (e.g. `"text/plain"`) for a file
//! name, optionally sharpened by a content sniff, and [`category`]
//! classifies that string into the coarse [`Category`] `crate::icons`
//! differentiates glyph *shape* by (style guide §1: mimetype
//! differentiation is glyph shape only, never hue). iced-free (CLAUDE.md's
//! layering rule) — this file only ever touches `xdg_mime`/`std`.
//!
//! See the dated survey comment at this crate's `xdg-mime` entry in
//! `Cargo.toml` for why that crate (not a hand-rolled `globs2`/sniff
//! parser) is what backs [`MimeDb`].
//!
//! **Why this module never calls `xdg_mime::GuessBuilder::path`/
//! `.metadata`** (both of which do their own `std::fs::metadata`/
//! `File::open` against the *target* file): a location's bytes may live on
//! a remote backend, so [`MimeDb::guess`] only ever builds a guess from a
//! file name plus an already-in-hand `sniff` buffer the caller fetched
//! however its own backend does that (`Backend::read`, for a later stage
//! that wires content sniffing through the open flow). This stage's own
//! call sites (row-glyph selection in `ui::dirview::list`/`grid`,
//! default-app resolution in `main.rs`) all pass `sniff: None` —
//! extension/glob-based guessing, no I/O at all, which is what covers the
//! overwhelming majority of real files and keeps "which icon does this row
//! get" a synchronous, per-frame-cheap lookup rather than a per-row read.
//! A later stage can thread a fetched sniff buffer through the same
//! `sniff` parameter without any signature change here.

use std::ffi::OsStr;
use std::panic::{self, AssertUnwindSafe};

use xdg_mime::SharedMimeInfo;

/// Wraps the loaded shared-MIME-info database. Expensive-ish to build (it
/// walks and parses every `globs2`/`magic`/`subclasses` file in the
/// `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` chain), so CLAUDE.md's shared-cache
/// rule applies: one instance lives on `App` (`main.rs`), never rebuilt
/// per view or per row.
pub struct MimeDb {
    inner: SharedMimeInfo,
}

impl MimeDb {
    /// Loads the system's shared-MIME-info database.
    ///
    /// `xdg_mime::SharedMimeInfo::new()` internally calls
    /// `dirs_next::data_dir().expect(..)`, which panics if *both*
    /// `$XDG_DATA_HOME` and `$HOME` are unset — an edge case real desktop
    /// sessions never hit, but `main.rs`'s own fallback-to-`/` handling for
    /// a HOME-less sandbox shows this codebase takes that possibility
    /// seriously. CLAUDE.md's no-panic rule binds on *any* runtime path,
    /// including one a dependency's own internals could take, so this
    /// catches that one specific panic and degrades to an empty database
    /// (`new_for_directory` on a directory that doesn't exist — every
    /// mimetype guess then falls back to `application/octet-stream`,
    /// [`Category::Generic`]) rather than taking the whole app down over a
    /// missing MIME database.
    pub fn new() -> Self {
        let inner = panic::catch_unwind(AssertUnwindSafe(SharedMimeInfo::new)).unwrap_or_else(
            |_| {
                eprintln!(
                    "saola-files: could not locate a MIME database (no $XDG_DATA_HOME or $HOME) — mimetype guesses will be generic"
                );
                SharedMimeInfo::new_for_directory("/nonexistent-saola-files-mime-fallback")
            },
        );
        MimeDb { inner }
    }

    /// Best-guess mimetype essence (`"text/plain"`, never
    /// `"text/plain; charset=utf-8"`) for `name`, optionally sharpened by
    /// `sniff` — a leading chunk of the file's contents. See the module
    /// docs for why this stage's own callers always pass `None`.
    ///
    /// `name` is lossily converted to `&str` for the glob matcher — the
    /// `xdg_mime` API only accepts UTF-8 file names. This is the one
    /// sanctioned `to_string_lossy` outside a view (CLAUDE.md's OsString
    /// discipline): a non-UTF-8 name still gets *a* mimetype guess off its
    /// lossy substring rather than skipping resolution for it entirely —
    /// the same posture `FileEntry::display_name` takes for rendering.
    ///
    /// **Deliberately never calls `GuessBuilder::data(&[])`** when `sniff`
    /// is `None`. `xdg_mime` 0.4.0's own `get_mime_type_for_data` treats
    /// *any* empty byte slice as proof the file itself is zero-length —
    /// `SharedMimeInfo::get_mime_type_for_data(&[])` unconditionally
    /// returns `application/x-zerosize` at its highest confidence
    /// (verified against the vendored `xdg-mime-0.4.0` source, `src/
    /// lib.rs`'s `get_mime_type_for_data`) — there's no way to tell the
    /// builder "no sniff was taken" apart from "the file is confirmed
    /// empty" once `.data()` has been called at all. So `sniff: None`
    /// bypasses `GuessBuilder` entirely and calls
    /// `get_mime_types_from_file_name` directly — the same glob-only
    /// lookup `guess()` itself would fall through to for an unmatched or
    /// ambiguous name, without ever touching the data-sniffing path.
    pub fn guess(&self, name: &OsStr, sniff: Option<&[u8]>) -> String {
        let lossy = name.to_string_lossy();
        match sniff {
            Some(bytes) => {
                let mut builder = self.inner.guess_mime_type();
                builder.file_name(&lossy);
                builder.data(bytes);
                builder.guess().mime_type().essence_str().to_owned()
            }
            None => self
                .inner
                .get_mime_types_from_file_name(&lossy)
                .first()
                // `get_mime_types_from_file_name` is documented to always
                // return at least `[application/octet-stream]` for an
                // unmatched name rather than an empty `Vec` — this
                // fallback exists for defensiveness (CLAUDE.md's no-panic
                // rule: never assume a dependency's own documented
                // behavior holds forever) rather than a path any test
                // exercises.
                .map(|mime| mime.essence_str().to_owned())
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
        }
    }
}

impl Default for MimeDb {
    fn default() -> Self {
        Self::new()
    }
}

/// The coarse classification [`crate::icons::for_entry`] draws a
/// glyph *shape* from. Deliberately coarser than a raw mimetype string:
/// dozens of `text/x-*` source-language mimetypes all read as
/// [`Category::Code`] — one glyph, the way a real file manager's icon set
/// works, rather than growing a per-language icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Directory,
    Text,
    Code,
    Image,
    Audio,
    Video,
    Archive,
    /// A `.desktop`/ini-shaped config file.
    Config,
    /// Recognized as *some* kind of file, but not one of the categories
    /// above (`application/octet-stream` and friends) — the plain
    /// generic-file glyph, distinct from [`Category::Unknown`]'s
    /// question-mark glyph.
    Generic,
    /// The mimetype string itself was empty — nothing to classify at all,
    /// distinct from [`Category::Generic`], which is a confident "this is
    /// just a binary blob" classification.
    Unknown,
}

/// Classifies a mimetype essence string (as returned by [`MimeDb::guess`])
/// into a [`Category`]. Pure and total — no I/O, so every branch is
/// unit-tested directly against string literals without needing a real
/// [`MimeDb`].
pub fn category(mime: &str) -> Category {
    if mime.is_empty() {
        return Category::Unknown;
    }
    if mime == "inode/directory" {
        return Category::Directory;
    }
    if mime == "application/x-desktop" {
        return Category::Config;
    }
    if is_code_mime(mime) {
        return Category::Code;
    }
    if let Some(top) = mime.split('/').next() {
        match top {
            "text" => return Category::Text,
            "image" => return Category::Image,
            "audio" => return Category::Audio,
            "video" => return Category::Video,
            _ => {}
        }
    }
    if is_archive_mime(mime) {
        return Category::Archive;
    }
    Category::Generic
}

/// Structured/markup/source mimetypes that should read as [`Category::Code`]
/// rather than plain [`Category::Text`] — checked ahead of the top-level
/// `text/*`/`application/*` split so, e.g., `text/html` and
/// `application/json` both land here even though they disagree on top-level
/// type.
fn is_code_mime(mime: &str) -> bool {
    const CODE_ESSENCES: &[&str] = &[
        "application/json",
        "application/xml",
        "application/javascript",
        "application/x-yaml",
        "application/x-sh",
        "application/x-shellscript",
        "text/html",
        "text/css",
    ];
    CODE_ESSENCES.contains(&mime)
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        // shared-mime-info's convention for source-language files, e.g.
        // `text/x-rust`, `text/x-python`, `text/x-csrc`.
        || mime.starts_with("text/x-")
}

fn is_archive_mime(mime: &str) -> bool {
    const ARCHIVE_ESSENCES: &[&str] = &[
        "application/zip",
        "application/x-tar",
        "application/gzip",
        "application/x-gzip",
        "application/x-bzip",
        "application/x-bzip2",
        "application/x-xz",
        "application/x-7z-compressed",
        "application/x-rar",
        "application/vnd.rar",
        "application/java-archive",
        "application/x-compressed-tar",
        "application/x-zstd-compressed-tar",
    ];
    ARCHIVE_ESSENCES.contains(&mime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    // ── `category` — pure, no I/O ───────────────────────────────────────

    #[test]
    fn empty_mime_is_unknown() {
        assert_eq!(category(""), Category::Unknown);
    }

    #[test]
    fn directory_mime_is_directory() {
        assert_eq!(category("inode/directory"), Category::Directory);
    }

    #[test]
    fn plain_text_is_text_not_code() {
        assert_eq!(category("text/plain"), Category::Text);
    }

    #[test]
    fn markup_and_structured_data_are_code() {
        assert_eq!(category("text/html"), Category::Code);
        assert_eq!(category("text/css"), Category::Code);
        assert_eq!(category("application/json"), Category::Code);
        assert_eq!(category("application/xml"), Category::Code);
        assert_eq!(category("application/vnd.api+json"), Category::Code);
        assert_eq!(category("text/x-rust"), Category::Code);
        assert_eq!(category("text/x-python"), Category::Code);
    }

    #[test]
    fn image_audio_video_split_by_top_level_type() {
        assert_eq!(category("image/png"), Category::Image);
        assert_eq!(category("audio/flac"), Category::Audio);
        assert_eq!(category("video/mp4"), Category::Video);
    }

    #[test]
    fn known_archive_essences_are_archive() {
        assert_eq!(category("application/zip"), Category::Archive);
        assert_eq!(category("application/x-tar"), Category::Archive);
        assert_eq!(category("application/x-7z-compressed"), Category::Archive);
    }

    #[test]
    fn desktop_entries_are_config() {
        assert_eq!(category("application/x-desktop"), Category::Config);
    }

    #[test]
    fn unrecognized_binary_mimetype_is_generic() {
        assert_eq!(category("application/octet-stream"), Category::Generic);
        assert_eq!(
            category("application/x-made-up-vendor-type"),
            Category::Generic
        );
    }

    // ── `MimeDb::guess` — needs the real system MIME database ───────────
    //
    // Every Linux dev/CI environment this app targets ships a
    // shared-mime-info database (`/usr/share/mime`) with at least the core
    // freedesktop.org.xml glob rules — the same "assume a working POSIX
    // environment" posture `modules::local`'s own tests already take
    // (temp dirs, `id -u`, …). `png`/`txt` are in the core spec every
    // shared-mime-info install ships, so these are about as safe an
    // integration test against real system data as this can be.

    #[test]
    fn guess_resolves_common_extensions_from_the_real_mime_db() {
        let db = MimeDb::new();
        assert_eq!(db.guess(OsStr::new("photo.png"), None), "image/png");
        assert_eq!(db.guess(OsStr::new("readme.txt"), None), "text/plain");
    }

    #[test]
    fn guess_falls_back_to_octet_stream_for_unknown_extensions() {
        let db = MimeDb::new();
        let guess = db.guess(OsStr::new("mystery.saola-made-up-extension"), None);
        assert_eq!(guess, "application/octet-stream");
    }

    #[test]
    fn guess_handles_a_non_utf8_name_without_panicking() {
        use std::os::unix::ffi::OsStrExt;
        let db = MimeDb::new();
        let raw = OsString::from(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        // Doesn't panic; the lossy conversion still leaves the `.txt`
        // extension intact for the glob matcher.
        assert_eq!(db.guess(&raw, None), "text/plain");
    }
}
