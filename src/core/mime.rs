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
//! through `Backend::read`.
//!
//! **Content sniffing** (this module's other half) is deliberately
//! *opt-in and asynchronous*. Per-frame glyph lookups still resolve from
//! the name alone ([`MimeDb::guess_entry`] is a hash lookup, no I/O). Only
//! when the name gives no type ([`needs_sniff`]) does the app read the
//! first [`SNIFF_BYTES`] of a *visible, local* file in the background and
//! hand them to [`MimeDb::record_sniff`]; the verdict is cached per
//! (directory, name, mtime), so a file is not re-read until its mtime
//! changes (or the bounded cache is wiped), and a `.txt` (a name that already says what it is) is never
//! sniffed at all.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::panic::{self, AssertUnwindSafe};
use std::time::SystemTime;

use xdg_mime::SharedMimeInfo;

use crate::core::vfs::Location;

/// How many leading bytes a content sniff reads. 4 KiB covers every magic
/// signature in shared-mime-info that matters for a glyph, and is small
/// enough that reading it never feels like "opening" the file.
pub const SNIFF_BYTES: usize = 4096;

/// Upper bound on cached sniff verdicts. When reached the whole cache is
/// dropped (a cheap, predictable policy, deliberately not an LRU): every
/// verdict is then recomputed lazily, so a wiped file is re-read once more
/// the next time it is on screen.
const SNIFF_CACHE_CAPACITY: usize = 8192;

/// Whether a name-only mimetype is too vague to trust, so the file's
/// leading bytes are worth reading. Pure, so the decision is unit-tested
/// without touching a file system.
///
/// Only "no type" (`application/octet-stream`, which is what the glob
/// matcher returns for an unmatched name such as `README` or `picture`)
/// qualifies. A name that already resolves to a specific type (`.txt`,
/// `.png`, `.rs`) is trusted and never re-sniffed.
pub fn needs_sniff(name_mime: &str) -> bool {
    name_mime.is_empty() || name_mime == "application/octet-stream"
}

/// One cached sniff verdict: the mtime it was computed for, and the result.
type SniffVerdict = (SystemTime, String);

/// Sniff verdicts, keyed by directory, then file name. The two-level shape
/// lets a per-frame lookup borrow `&Location` and `&OsStr` directly
/// (`HashMap::get` accepts any borrowed form of the key), so a render pass
/// allocates nothing.
#[derive(Default)]
struct SniffCache {
    dirs: HashMap<Location, HashMap<OsString, SniffVerdict>>,
    len: usize,
}

impl SniffCache {
    /// The verdict for `name`, but only if it was computed for exactly
    /// `modified` — a changed mtime is a clean miss, never a stale answer.
    fn get(&self, dir: &Location, name: &OsStr, modified: SystemTime) -> Option<&str> {
        let (mtime, mime) = self.dirs.get(dir)?.get(name)?;
        (*mtime == modified).then_some(mime.as_str())
    }

    fn insert(&mut self, dir: &Location, name: &OsStr, modified: SystemTime, mime: String) {
        if self.len >= SNIFF_CACHE_CAPACITY {
            self.dirs.clear();
            self.len = 0;
        }
        let names = self.dirs.entry(dir.clone()).or_default();
        if names.insert(name.to_owned(), (modified, mime)).is_none() {
            self.len += 1;
        }
    }
}

/// Wraps the loaded shared-MIME-info database. Expensive-ish to build (it
/// walks and parses every `globs2`/`magic`/`subclasses` file in the
/// `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` chain), so CLAUDE.md's shared-cache
/// rule applies: one instance lives on `App` (`main.rs`), never rebuilt
/// per view or per row.
pub struct MimeDb {
    inner: SharedMimeInfo,
    /// Content-sniff verdicts. `RefCell` because `view()` only holds
    /// `&MimeDb` yet the update path records into it; both run on the one
    /// UI thread and no borrow crosses an `.await`, so it never panics
    /// from a double borrow (every borrow below is short and non-nested).
    sniffed: RefCell<SniffCache>,
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
        MimeDb {
            inner,
            sniffed: RefCell::default(),
        }
    }

    /// The mimetype to show for the file `name` inside `dir`: the cached
    /// content-sniff verdict when one exists for exactly this `modified`
    /// time, otherwise the name-only guess. Never does I/O, so it is safe
    /// to call per row, per frame.
    pub fn guess_entry(
        &self,
        dir: &Location,
        name: &OsStr,
        modified: Option<SystemTime>,
    ) -> String {
        let by_name = self.guess(name, None);
        if !needs_sniff(&by_name) {
            return by_name;
        }
        modified
            .and_then(|mtime| {
                self.sniffed
                    .borrow()
                    .get(dir, name, mtime)
                    .map(str::to_owned)
            })
            .unwrap_or(by_name)
    }

    /// Whether this file still wants a content sniff: its name gives no
    /// type *and* no verdict is cached for this `modified` time. A file
    /// with no known mtime is never sniffed (nothing to key the cache on).
    pub fn wants_sniff(&self, dir: &Location, name: &OsStr, modified: Option<SystemTime>) -> bool {
        let Some(mtime) = modified else {
            return false;
        };
        needs_sniff(&self.guess(name, None))
            && self.sniffed.borrow().get(dir, name, mtime).is_none()
    }

    /// Records the verdict for `bytes` (the file's leading bytes) so later
    /// [`Self::guess_entry`] calls for the same `modified` time return it.
    /// An unrecognised file is recorded too (as `application/octet-stream`)
    /// so it is not re-read on every refresh (only after its mtime changes
    /// or the cache is wiped).
    pub fn record_sniff(&self, dir: &Location, name: &OsStr, modified: SystemTime, bytes: &[u8]) {
        let mime = self.guess(name, Some(bytes));
        self.sniffed.borrow_mut().insert(dir, name, modified, mime);
    }

    /// Records that reading this file's bytes failed (permissions, a race
    /// with a delete), so the name-only guess stands and the read is not
    /// retried on every scroll until the file's mtime changes.
    pub fn record_unsniffable(&self, dir: &Location, name: &OsStr, modified: SystemTime) {
        let generic = "application/octet-stream".to_owned();
        self.sniffed
            .borrow_mut()
            .insert(dir, name, modified, generic);
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

    // ── content sniffing ────────────────────────────────────────────────

    #[test]
    fn only_an_untyped_name_needs_a_sniff() {
        assert!(needs_sniff("application/octet-stream"));
        assert!(needs_sniff(""));
        // A name that already says what it is is trusted: `.txt` is never
        // re-sniffed.
        assert!(!needs_sniff("text/plain"));
        assert!(!needs_sniff("image/png"));
    }

    /// The PNG signature plus an IHDR chunk header, enough for
    /// shared-mime-info's magic rule.
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn sniffing_an_extensionless_png_finds_the_image_type() {
        let db = MimeDb::new();
        assert_eq!(
            db.guess(OsStr::new("picture"), Some(PNG_MAGIC)),
            "image/png"
        );
    }

    #[test]
    fn extensionless_file_is_sniffed_once_then_served_from_the_cache() {
        let db = MimeDb::new();
        let dir = Location::local("/scratch");
        let name = OsStr::new("picture");
        assert!(db.wants_sniff(&dir, name, Some(t(1))));
        // Before the sniff lands the glyph falls back to the name guess.
        assert_eq!(
            db.guess_entry(&dir, name, Some(t(1))),
            "application/octet-stream"
        );

        db.record_sniff(&dir, name, t(1), PNG_MAGIC);
        assert!(!db.wants_sniff(&dir, name, Some(t(1))));
        assert_eq!(db.guess_entry(&dir, name, Some(t(1))), "image/png");
    }

    #[test]
    fn a_changed_mtime_invalidates_the_cached_verdict() {
        let db = MimeDb::new();
        let dir = Location::local("/scratch");
        let name = OsStr::new("picture");
        db.record_sniff(&dir, name, t(1), PNG_MAGIC);

        assert!(db.wants_sniff(&dir, name, Some(t(2))));
        assert_eq!(
            db.guess_entry(&dir, name, Some(t(2))),
            "application/octet-stream"
        );
    }

    #[test]
    fn the_same_name_in_another_directory_does_not_share_a_verdict() {
        let db = MimeDb::new();
        let name = OsStr::new("picture");
        db.record_sniff(&Location::local("/a"), name, t(1), PNG_MAGIC);
        assert!(db.wants_sniff(&Location::local("/b"), name, Some(t(1))));
    }

    #[test]
    fn a_typed_name_never_wants_a_sniff() {
        let db = MimeDb::new();
        let dir = Location::local("/scratch");
        assert!(!db.wants_sniff(&dir, OsStr::new("notes.txt"), Some(t(1))));
        // ...and its name guess wins even if bytes were somehow recorded.
        db.record_sniff(&dir, OsStr::new("notes.txt"), t(1), PNG_MAGIC);
        assert_eq!(
            db.guess_entry(&dir, OsStr::new("notes.txt"), Some(t(1))),
            "text/plain"
        );
    }

    #[test]
    fn a_file_with_no_known_mtime_is_not_sniffed() {
        let db = MimeDb::new();
        assert!(!db.wants_sniff(&Location::local("/s"), OsStr::new("picture"), None));
    }

    #[test]
    fn an_unrecognised_file_is_recorded_so_it_is_not_re_read() {
        let db = MimeDb::new();
        let dir = Location::local("/scratch");
        let name = OsStr::new("blob");
        db.record_sniff(&dir, name, t(1), &[0xfe, 0xed, 0x00, 0x01, 0x02, 0x03]);
        assert!(!db.wants_sniff(&dir, name, Some(t(1))));
    }

    #[test]
    fn the_cache_is_bounded_and_keeps_working_after_a_wipe() {
        let mut cache = SniffCache::default();
        let dir = Location::local("/d");
        let mtime = t(1);
        for i in 0..SNIFF_CACHE_CAPACITY {
            cache.insert(&dir, OsStr::new(&format!("f{i}")), mtime, "a/b".to_owned());
        }
        assert_eq!(cache.len, SNIFF_CACHE_CAPACITY);
        assert_eq!(cache.get(&dir, OsStr::new("f0"), mtime), Some("a/b"));

        // One past the cap wipes everything, then stores the new verdict.
        cache.insert(&dir, OsStr::new("late"), mtime, "image/png".to_owned());
        assert_eq!(cache.len, 1);
        assert_eq!(cache.get(&dir, OsStr::new("f0"), mtime), None);
        assert_eq!(
            cache.get(&dir, OsStr::new("late"), mtime),
            Some("image/png")
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
