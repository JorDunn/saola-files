//! Foreign (Wayland) clipboard interop (Stage 10) — writing what another
//! file manager's Paste expects to see on this app's Copy/Cut, and a
//! best-effort read of what another app's Copy/Cut left behind for this
//! app's Paste. This is deliberately **not** `core::fs::ops::Clipboard`
//! (that stays "the internal `{op, locations}` model CLAUDE.md calls
//! authoritative" — see that type's own doc comment): this module never
//! stores anything, it only translates `Location`s to/from the two MIME
//! payloads a real desktop clipboard understands.
//!
//! **Two MIME types, one write.** [`write()`] offers both
//! [`URI_LIST_MIME`] (`text/uri-list`, RFC 2483 — every clipboard consumer
//! on Linux understands this one, including browsers/GTK/Qt apps with no
//! notion of "file manager clipboard" at all) and
//! [`GNOME_COPIED_FILES_MIME`] (`x-special/gnome-copied-files` — the
//! Nautilus-originated format that additionally carries the copy/cut
//! distinction, and which Dolphin, PCManFM, and every other mainstream
//! Linux file manager also reads) in the same `copy_multi` call, so a
//! paste into *any* of them sees a coherent, correctly-labeled file list.
//!
//! **Reading prefers the richer format.** [`read`] tries
//! [`GNOME_COPIED_FILES_MIME`] first and only falls back to
//! [`URI_LIST_MIME`] if that MIME type wasn't offered — a source that only
//! ever writes a plain URI list (a browser's dragged-file clipboard entry,
//! say) has no copy/cut concept at all, so a fallback read is always
//! treated as [`crate::core::fs::ops::ClipboardOp::Copy`] — the
//! conservative reading: worst case a paste-then-manual-delete costs one
//! extra step, never a surprise deletion of something the user only meant
//! to copy.
//!
//! **Best-effort, always — never blocks, never panics, never surfaces a
//! hard error to the caller of [`read`].** No Wayland compositor at all
//! (`$WAYLAND_DISPLAY` unset — every headless CI sandbox this crate's own
//! test suite runs in) is `wl_clipboard_rs`'s ordinary
//! `ConnectError::NoCompositor`, not a panic; `read` turns *every* failure
//! into `None` rather than propagating one, per PLAN.md's explicit "never
//! block paste on a foreign clipboard" — the internal clipboard
//! (`core::fs::ops::Clipboard`) always wins when it has contents, and a
//! foreign-clipboard failure or empty result just means "nothing to
//! paste", exactly like an empty internal clipboard already means today.
//! [`write()`]'s failures *are* surfaced (as a `Result`) since a Copy/Cut is
//! a deliberate, visible user action whose caller (`main.rs`) already
//! words every other backend failure to stderr the same way.
//!
//! **`spawn_blocking`, not async — `wl_clipboard_rs` is a synchronous
//! library** (it opens its own Wayland connection and either serves
//! copy-paste requests off a background `std::thread` it spawns itself, or
//! blocks the calling thread reading a pipe until data arrives). Every
//! public function here wraps its blocking core in
//! `tokio::task::spawn_blocking`, the same shape `modules::local::
//! run_blocking`/`ui::trashview`'s own `run_blocking` already use for
//! exactly this reason (CLAUDE.md: "one async runtime … never construct a
//! second" — this never spins up its own executor, it borrows a blocking-
//! pool thread from the one iced already owns).

use std::ffi::OsStr;
use std::io::Read as _;
// Unconditional, not `#[cfg(unix)]` — this crate is Linux-only already
// (niri is Wayland-only; see the top-level docs), the same posture
// `core::fs::trash`/`core::places` already take importing this same path.
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use wl_clipboard_rs::copy::{MimeSource, MimeType as CopyMimeType, Options, Source};
use wl_clipboard_rs::paste::{self, ClipboardType, MimeType as PasteMimeType, Seat};

use crate::core::fs::ops::ClipboardOp;
use crate::core::places::{decode_percent, encode_percent};
use crate::core::vfs::Location;

pub const URI_LIST_MIME: &str = "text/uri-list";
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

/// What a best-effort foreign-clipboard [`read`] found: the copy/cut verb
/// (defaulted to `Copy` for a bare `text/uri-list` source — see the module
/// doc comment) and every `file://` URI it could parse back into a
/// `Location`. A line this app can't act on (a non-`file://` URI — an
/// `http://` link dragged in from a browser, say) is silently dropped
/// rather than turned into a bogus local path; an all-dropped result still
/// comes back as `Some` with an empty `locations` — `main.rs` treats that
/// exactly like an empty internal clipboard, no special-casing needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignClipboard {
    pub op: ClipboardOp,
    pub locations: Vec<Location>,
}

// ── Writing (Copy/Cut) ───────────────────────────────────────────────────

/// Writes `locations` to the Wayland clipboard under both
/// [`URI_LIST_MIME`] and [`GNOME_COPIED_FILES_MIME`] — see the module doc
/// comment's "Two MIME types, one write". An empty `locations` is a no-op
/// (nothing to offer, and offering an empty file list would just clear
/// whatever's already on the clipboard for no reason `main.rs`'s own
/// Copy/Cut call sites — which already guard `selection_targets().is_empty()`
/// before bubbling `Event::CopyRequested`/`CutRequested` — ever intend).
pub async fn write(op: ClipboardOp, locations: Vec<Location>) -> Result<(), String> {
    if locations.is_empty() {
        return Ok(());
    }
    run_blocking(move || write_blocking(op, &locations)).await
}

fn write_blocking(op: ClipboardOp, locations: &[Location]) -> Result<(), String> {
    let sources = vec![
        MimeSource {
            source: Source::Bytes(uri_list_body(locations).into_boxed_slice()),
            mime_type: CopyMimeType::Specific(URI_LIST_MIME.to_owned()),
        },
        MimeSource {
            source: Source::Bytes(gnome_copied_files_body(op, locations).into_boxed_slice()),
            mime_type: CopyMimeType::Specific(GNOME_COPIED_FILES_MIME.to_owned()),
        },
    ];
    Options::new()
        .copy_multi(sources)
        .map_err(|err| err.to_string())
}

/// The `text/uri-list` payload: one `file://` URI per line, **CRLF-joined
/// with a trailing CRLF** — RFC 2483's own line ending, which every reader
/// tested against this format tolerates (several *require* it).
fn uri_list_body(locations: &[Location]) -> Vec<u8> {
    let mut out = String::new();
    for location in locations {
        out.push_str(&file_uri(location));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// The `x-special/gnome-copied-files` payload: `copy`/`cut` on its own
/// first line, then one `file://` URI per line, **LF-joined, no trailing
/// newline** — Nautilus's own on-the-wire format. Deliberately *not*
/// CRLF like [`uri_list_body`] above: this is the one difference between
/// the two payloads, and it matters — at least one mainstream reader
/// (PCManFM) treats a trailing `\r` as part of the path rather than
/// trimming it, so writing CRLF here would corrupt every pasted file name
/// by one invisible character on that reader.
fn gnome_copied_files_body(op: ClipboardOp, locations: &[Location]) -> Vec<u8> {
    let verb = match op {
        ClipboardOp::Copy => "copy",
        ClipboardOp::Cut => "cut",
    };
    let mut out = String::from(verb);
    for location in locations {
        out.push('\n');
        out.push_str(&file_uri(location));
    }
    out.into_bytes()
}

/// Builds one `file://<percent-encoded-path>` URI. **Percent-encodes the
/// location's raw path bytes (`OsStrExt::as_bytes`), never
/// `to_string_lossy`** — a non-UTF-8 name (SFTP names are bytes too, per
/// CLAUDE.md's OsString discipline) must round-trip through the clipboard
/// exactly, not get mangled into `�` first. Reuses
/// `core::places::encode_percent` — the same percent-encoding scheme this
/// crate's `.trashinfo` `Path=` writer already uses (`core::fs::trash`),
/// one scheme, not two.
fn file_uri(location: &Location) -> String {
    let bytes = location.path.as_os_str().as_bytes();
    format!("file://{}", encode_percent(bytes))
}

/// The inverse of [`file_uri`]: `None` for anything that isn't a `file://`
/// URI (a foreign clipboard offered a scheme this app can't act on) rather
/// than a half-decoded garbage path.
fn parse_file_uri(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("file://")?;
    let bytes = decode_percent(rest);
    Some(PathBuf::from(OsStr::from_bytes(&bytes)))
}

// ── Reading (Paste) ──────────────────────────────────────────────────────

/// Best-effort read of the foreign clipboard for `Event::PasteRequested`'s
/// fallback path (`main.rs::App::start_paste`, only ever reached when the
/// *internal* clipboard is empty). See the module doc comment's "Best-
/// effort, always" section for exactly what degrades to `None` here and
/// why nothing this function does can block the UI thread or panic.
pub async fn read() -> Option<ForeignClipboard> {
    run_blocking(read_blocking).await.ok().flatten()
}

fn read_blocking() -> Result<Option<ForeignClipboard>, String> {
    if let Ok((mut pipe, _mime)) = paste::get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        PasteMimeType::Specific(GNOME_COPIED_FILES_MIME),
    ) {
        let mut buf = Vec::new();
        if pipe.read_to_end(&mut buf).is_ok() {
            return Ok(Some(parse_gnome_copied_files(&buf)));
        }
    }

    match paste::get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        PasteMimeType::Specific(URI_LIST_MIME),
    ) {
        Ok((mut pipe, _mime)) => {
            let mut buf = Vec::new();
            if pipe.read_to_end(&mut buf).is_err() {
                return Ok(None);
            }
            Ok(Some(ForeignClipboard {
                op: ClipboardOp::Copy,
                locations: parse_uri_list(&buf),
            }))
        }
        // Every failure here — no compositor, no seats, an empty
        // clipboard, neither MIME type offered — is an ordinary "nothing
        // to paste from outside", per the module doc comment; never
        // propagated as an `Err` a caller might be tempted to surface.
        Err(_) => Ok(None),
    }
}

fn parse_uri_list(buf: &[u8]) -> Vec<Location> {
    String::from_utf8_lossy(buf)
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(parse_file_uri)
        .map(Location::local)
        .collect()
}

fn parse_gnome_copied_files(buf: &[u8]) -> ForeignClipboard {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.lines();
    let op = match lines.next() {
        Some("cut") => ClipboardOp::Cut,
        // "copy", a missing first line, or anything else unrecognized —
        // the same conservative default the module doc comment explains
        // for a bare `text/uri-list` source.
        _ => ClipboardOp::Copy,
    };
    let locations = lines
        .filter(|line| !line.is_empty())
        .filter_map(parse_file_uri)
        .map(Location::local)
        .collect();
    ForeignClipboard { op, locations }
}

// ── spawn_blocking bridge ────────────────────────────────────────────────

async fn run_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(format!(
            "internal error talking to the Wayland clipboard: {join_err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── file_uri / parse_file_uri round trip ────────────────────────────

    #[test]
    fn file_uri_round_trips_an_ordinary_path() {
        let location = Location::local("/home/jordan/My File.txt");
        let uri = file_uri(&location);
        assert_eq!(uri, "file:///home/jordan/My%20File.txt");
        assert_eq!(parse_file_uri(&uri), Some(location.path));
    }

    #[test]
    fn file_uri_round_trips_a_non_utf8_name() {
        let raw = OsStr::from_bytes(b"caf\xe9.txt");
        let location = Location::local(PathBuf::from("/tmp").join(raw));
        let uri = file_uri(&location);
        let parsed = parse_file_uri(&uri).unwrap();
        assert_eq!(parsed, location.path);
        assert_eq!(
            parsed.file_name().unwrap().as_bytes(),
            raw.as_bytes(),
            "the raw bytes survive the URI round trip untouched"
        );
    }

    #[test]
    fn parse_file_uri_rejects_a_non_file_scheme() {
        assert_eq!(parse_file_uri("http://example.com/a.txt"), None);
    }

    // ── uri_list_body / gnome_copied_files_body shape ───────────────────

    #[test]
    fn uri_list_body_is_crlf_joined_with_a_trailing_crlf() {
        let body = uri_list_body(&[Location::local("/a"), Location::local("/b")]);
        assert_eq!(body, b"file:///a\r\nfile:///b\r\n".to_vec());
    }

    #[test]
    fn gnome_copied_files_body_starts_with_the_verb_and_is_lf_joined() {
        let body = gnome_copied_files_body(
            ClipboardOp::Cut,
            &[Location::local("/a"), Location::local("/b")],
        );
        assert_eq!(body, b"cut\nfile:///a\nfile:///b".to_vec());
    }

    #[test]
    fn gnome_copied_files_body_words_copy_correctly_too() {
        let body = gnome_copied_files_body(ClipboardOp::Copy, &[Location::local("/a")]);
        assert!(String::from_utf8(body).unwrap().starts_with("copy\n"));
    }

    // ── parse_uri_list / parse_gnome_copied_files ───────────────────────

    #[test]
    fn parse_uri_list_skips_blank_lines_and_non_file_uris() {
        let buf = b"file:///a\r\n\r\nhttp://example.com/x\r\nfile:///b\r\n";
        let locations = parse_uri_list(buf);
        assert_eq!(
            locations,
            vec![Location::local("/a"), Location::local("/b")]
        );
    }

    #[test]
    fn parse_gnome_copied_files_reads_the_cut_verb_and_every_uri() {
        let buf = b"cut\nfile:///a\nfile:///b";
        let parsed = parse_gnome_copied_files(buf);
        assert_eq!(parsed.op, ClipboardOp::Cut);
        assert_eq!(
            parsed.locations,
            vec![Location::local("/a"), Location::local("/b")]
        );
    }

    #[test]
    fn parse_gnome_copied_files_defaults_to_copy_for_an_unrecognized_first_line() {
        // A bare `text/uri-list`-shaped payload happened to be offered
        // under this MIME type by some other producer — still parsed as a
        // list, just conservatively as `Copy` (see the module doc comment).
        let buf = b"file:///a";
        let parsed = parse_gnome_copied_files(buf);
        assert_eq!(parsed.op, ClipboardOp::Copy);
        assert!(
            parsed.locations.is_empty(),
            "the \"verb\" line was consumed, not a path"
        );
    }

    // ── write/read against a real environment: best-effort, never panics ─
    //
    // This crate's own test runs (and most CI) have no live Wayland
    // compositor, so these can't assert a successful round trip the way
    // `core::fs::ops`/`core::fs::trash`'s real-I/O tests do — what they
    // verify instead is exactly the module doc comment's contract: no
    // panic, no hang, and `read()` degrading to `None` rather than
    // propagating an error. A live niri session is the manual-check item
    // for the real round trip (see the handoff).

    #[tokio::test]
    async fn write_degrades_to_an_error_rather_than_panicking_with_no_compositor() {
        // Whatever the sandbox's actual Wayland state is, this must return
        // rather than hang or panic — the assertion is "it returned",
        // deliberately not "it returned Ok" or "it returned Err".
        let _ = write(ClipboardOp::Copy, vec![Location::local("/tmp/x")]).await;
    }

    #[tokio::test]
    async fn write_with_no_locations_is_a_no_op_that_never_touches_the_compositor() {
        assert_eq!(write(ClipboardOp::Copy, Vec::new()).await, Ok(()));
    }

    #[tokio::test]
    async fn read_never_panics_and_always_returns_without_hanging() {
        let _ = read().await;
    }
}
