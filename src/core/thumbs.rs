//! Freedesktop thumbnail cache (Stage 11) — the one documented iced
//! exception in `core/` (CLAUDE.md: "`core/` stays iced-free except
//! `core/thumbs.rs` … produces `iced::widget::image::Handle`s"). Every
//! other file under `core/` must not import `iced`; this one does, exactly
//! once, in [`ThumbHandle`].
//!
//! **Cache layout, per the spec
//! (<https://specifications.freedesktop.org/thumbnail-spec/>).** Thumbnails
//! live under `$XDG_CACHE_HOME/thumbnails/<size-dir>/<md5-of-uri>.png`
//! ([`resolve_cache_root`]/[`cache_path`]). The URI is the canonical
//! `file://<percent-encoded-absolute-path>` form ([`canonical_uri`]) —
//! reuses `core::places::encode_percent`, the same percent-encoding scheme
//! `core::clipboard_interop::file_uri` already uses for the exact same
//! `file://` shape (one scheme app-wide, not two).
//!
//! **PNG, deliberately, per the spec** — this was surveyed and decided at
//! the plan level (WebP was considered and rejected for interop: every
//! other desktop's thumbnailer, and every desktop that might read *this*
//! app's cache entries, expects PNG under this path). Every cache file
//! carries the spec's required `tEXt` metadata: `Thumb::URI` (the
//! canonical URI, so a hash collision or a stale entry from a deleted-and-
//! recreated file is still caught) and `Thumb::MTime` (decimal seconds
//! since the epoch — the freshness check). [`generate_blocking`] validates
//! *both* before ever trusting a cached PNG.
//!
//! **Read-existing-then-generate.** [`thumbnail_for`] (the public async
//! entry point `main.rs` calls) always tries the disk cache first; only a
//! miss or a stale entry (URI/MTime mismatch, corrupt PNG, unreadable
//! metadata) falls through to actually decoding the source image. A freshly
//! generated thumbnail is written back atomically (temp file in the same
//! directory, then `rename` — the spec's own durability rule, so no reader
//! ever observes a half-written PNG) with `0600` permissions (a thumbnail
//! can reveal a private image's content, so the spec asks that only the
//! owning user can read the cache file).
//!
//! **`Thumbnailer` registry**, dispatched by mimetype essence string
//! (`"image/png"`, never the full `"image/png; …"` form — matches
//! `core::mime::MimeDb::guess`'s own contract). [`ImageThumbnailer`] is the
//! only implementation this stage ships; a future video/PDF thumbnailer is
//! purely additive — implement [`Thumbnailer`], `Registry::register` it
//! ahead of (or behind) the image one, done. `Thumbnailer::generate` is a
//! plain, non-async fn: every implementation is blocking CPU/disk work, and
//! [`thumbnail_for`] already runs the *entire* read-or-generate pipeline
//! inside one `spawn_blocking` call — an implementation must never itself
//! spawn async work.
//!
//! **Bounded concurrency.** [`thumbnail_for`] acquires a permit from a
//! shared `tokio::sync::Semaphore` (owned by `App`, sized once at startup —
//! see `main.rs`'s `THUMB_MAX_CONCURRENT`) before doing any work, so a
//! directory with thousands of visible candidates never spawns thousands of
//! concurrent blocking-pool tasks at once (CLAUDE.md: "never unbounded task
//! spawns").
//!
//! **`ThumbCache`** is the ~512-entry LRU of already-decoded
//! [`ThumbHandle`]s that lives on `App` (CLAUDE.md: "Shared caches … live
//! on the App, never per-view"). It's a second, smaller, in-memory cache
//! *on top of* the disk cache above — a hit here skips even the disk read.
//! Keyed by `(Location, mtime)`, so a file that changes on disk is a clean
//! miss rather than serving a stale decoded image.
//!
//! **Sizes.** All four spec directories exist ([`ThumbSize`]: `normal` 128,
//! `large` 256, `x-large` 512, `xx-large` 1024). The caller picks one via
//! [`ThumbSize::for_pixels`] and puts it in [`ThumbRequest::size`].
//!
//! **Fail cache.** When a thumbnailer returns `Err`, a 1x1 PNG with the same
//! `Thumb::URI`/`Thumb::MTime` chunks is written to
//! `thumbnails/fail/saola-files/<md5>.png`. While URI and mtime match, the
//! file is not decoded again (across restarts); a changed mtime retries once.
//! A type no thumbnailer claims leaves no marker.
//!
//! **Known gaps, stated plainly:**
//! - No `Thumb::Size` metadata key (optional in the spec).
//! - Dispatch is a plain mimetype check ([`Thumbnailer::handles`]), not the
//!   spec's `MimeInfo` per-thumbnailer metadata.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io::BufReader;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;

use crate::core::places::encode_percent;
use crate::core::vfs::Location;

// ── md5 (RFC 1321) ──────────────────────────────────────────────────────

/// A small, dependency-free MD5 implementation. The freedesktop thumbnail
/// spec's cache filenames are `md5(canonical_uri) + ".png"`, and — unlike
/// `image`/`png` below (already resident transitively via `iced`'s own
/// `image` feature, see this crate's `Cargo.toml`) — this sandbox's cargo
/// registry cache has no `md5`/`md-5` crate at all (checked 2026-08-09 via
/// `find ~/.cargo/registry -iname "*md5*"`, empty). MD5 is not used here
/// for anything security-sensitive — it only ever names a cache *file*,
/// nothing an adversary gains by forging — and this codebase already has a
/// precedent for hand-rolling a comparably fiddly, precisely-specified
/// algorithm instead of taking on a dependency for it
/// (`ui::dirview::list::civil_from_days`, Howard Hinnant's date algorithm).
/// Same posture here: verified against RFC 1321 §A.5's own test vectors in
/// this module's tests, not just derived from memory.
mod md5 {
    #[rustfmt::skip]
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    // K[i] = floor(2^32 * abs(sin(i + 1))) — RFC 1321's own precomputed
    // table, transcribed directly from the RFC text.
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    /// The raw 16-byte RFC 1321 digest of `input`.
    fn digest(input: &[u8]) -> [u8; 16] {
        let mut a0: u32 = 0x67452301;
        let mut b0: u32 = 0xefcdab89;
        let mut c0: u32 = 0x98badcfe;
        let mut d0: u32 = 0x10325476;

        // `as_chunks::<N>()` splits a slice into `&[[u8; N]]` plus a leftover
        // tail. The chunk size is part of the type, so every `chunk` below is
        // statically a `&[u8; 64]` and every `word` a `&[u8; 4]` — no length
        // checks, no indexing, nothing that can panic. `pad` always returns a
        // multiple of 64 bytes, so the (discarded) tail is empty.
        let padded = pad(input);
        let (blocks, _tail) = padded.as_chunks::<64>();
        for chunk in blocks {
            let mut m = [0u32; 16];
            let (words, _tail) = chunk.as_chunks::<4>();
            // Pair each of the 16 message words with its little-endian bytes;
            // `zip` stops at the shorter side, so no index is needed.
            for (slot, word) in m.iter_mut().zip(words) {
                *slot = u32::from_le_bytes(*word);
            }

            let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
            for i in 0..64 {
                let (f, g) = match i {
                    0..=15 => ((b & c) | (!b & d), i),
                    16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f.rotate_left(S[i]));
            }

            a0 = a0.wrapping_add(a);
            b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c);
            d0 = d0.wrapping_add(d);
        }

        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&a0.to_le_bytes());
        out[4..8].copy_from_slice(&b0.to_le_bytes());
        out[8..12].copy_from_slice(&c0.to_le_bytes());
        out[12..16].copy_from_slice(&d0.to_le_bytes());
        out
    }

    /// RFC 1321's padding: a `1` bit, zeros up to 56 mod 64 bytes, then the
    /// original bit length as a little-endian `u64`.
    fn pad(input: &[u8]) -> Vec<u8> {
        let bit_len = (input.len() as u64).wrapping_mul(8);
        let mut out = input.to_vec();
        out.push(0x80);
        while out.len() % 64 != 56 {
            out.push(0);
        }
        out.extend_from_slice(&bit_len.to_le_bytes());
        out
    }

    /// The lowercase hex digest — exactly the form the freedesktop
    /// thumbnail spec's cache filenames use.
    pub(super) fn hex_digest(input: &[u8]) -> String {
        digest(input)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // RFC 1321 §A.5's own test suite, verbatim.
        #[test]
        fn matches_rfc_1321_test_vectors() {
            assert_eq!(hex_digest(b""), "d41d8cd98f00b204e9800998ecf8427e");
            assert_eq!(hex_digest(b"a"), "0cc175b9c0f1b6a831c399e269772661");
            assert_eq!(hex_digest(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
            assert_eq!(
                hex_digest(b"message digest"),
                "f96b697d7cb7938d525a2f31aaf161d0"
            );
            assert_eq!(
                hex_digest(b"abcdefghijklmnopqrstuvwxyz"),
                "c3fcd3d76192e4007dfb496cca67e13b"
            );
            assert_eq!(
                hex_digest(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
                "d174ab98d277d9f5a5611c2c9f419d9f"
            );
            assert_eq!(
                hex_digest(
                    b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
                ),
                "57edf4a22be3c955ac49da2e2107b67a"
            );
        }

        #[test]
        fn digest_length_is_always_32_hex_characters() {
            assert_eq!(hex_digest(b"file:///home/jordan/photo.jpg").len(), 32);
        }
    }
}

// ── size directories ────────────────────────────────────────────────────

/// Which freedesktop size directory a cache entry lives under. The spec
/// defines exactly these four; a cached PNG's longest side is at most
/// [`ThumbSize::max_pixels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbSize {
    /// 128 px, the spec's `normal` directory.
    Normal,
    /// 256 px, `large`.
    Large,
    /// 512 px, `x-large`.
    XLarge,
    /// 1024 px, `xx-large`.
    XXLarge,
}

impl ThumbSize {
    pub const fn dir_name(self) -> &'static str {
        match self {
            ThumbSize::Normal => "normal",
            ThumbSize::Large => "large",
            ThumbSize::XLarge => "x-large",
            ThumbSize::XXLarge => "xx-large",
        }
    }

    pub const fn max_pixels(self) -> u32 {
        match self {
            ThumbSize::Normal => 128,
            ThumbSize::Large => 256,
            ThumbSize::XLarge => 512,
            ThumbSize::XXLarge => 1024,
        }
    }

    /// The smallest size whose `max_pixels` covers `pixels` (so the
    /// thumbnail is never upscaled on screen), clamped to
    /// [`ThumbSize::XXLarge`] for anything bigger. The caller that knows how
    /// large the thumbnail will be drawn (the zoom level, in device pixels)
    /// chooses; this only maps pixels to a spec directory.
    pub const fn for_pixels(pixels: u32) -> ThumbSize {
        match pixels {
            0..=128 => ThumbSize::Normal,
            129..=256 => ThumbSize::Large,
            257..=512 => ThumbSize::XLarge,
            _ => ThumbSize::XXLarge,
        }
    }
}

// ── cache path / key computation ────────────────────────────────────────

/// The canonical `file://<percent-encoded-path>` URI the spec keys cache
/// entries on. Percent-encodes the path's raw bytes (`OsStrExt::as_bytes`),
/// never `to_string_lossy` — matches CLAUDE.md's OsString discipline and
/// `core::clipboard_interop::file_uri`'s identical choice for the identical
/// reason (a non-UTF-8 name must still resolve to a stable, correct cache
/// key). `path` is expected to already be absolute (every `Location` this
/// module is ever called with is — see `ThumbRequest`'s doc comment); this
/// function does no canonicalization of its own (no symlink resolution, no
/// `..` collapsing), matching the spec's own "canonical" meaning "absolute,
/// as the application understands the path" rather than "resolved by the
/// kernel."
pub fn canonical_uri(path: &Path) -> String {
    format!("file://{}", encode_percent(path.as_os_str().as_bytes()))
}

/// `cache_root/<size-dir>/<md5-of-uri>.png`.
pub fn cache_path(cache_root: &Path, uri: &str, size: ThumbSize) -> PathBuf {
    cache_root
        .join(size.dir_name())
        .join(format!("{}.png", md5::hex_digest(uri.as_bytes())))
}

/// Directory under `fail/` that this app owns, per the spec's
/// `fail/<application-name>/` layout: other thumbnailers keep their own
/// failures, so one app giving up on a file never hides it from another.
const FAIL_APP_DIR: &str = "saola-files";

/// `cache_root/fail/saola-files/<md5-of-uri>.png`, the spec's marker that
/// "this app could not thumbnail this file". Not per-size: a file that
/// cannot be decoded cannot be decoded at any size.
pub fn fail_path(cache_root: &Path, uri: &str) -> PathBuf {
    cache_root
        .join("fail")
        .join(FAIL_APP_DIR)
        .join(format!("{}.png", md5::hex_digest(uri.as_bytes())))
}

/// The testable core of [`resolve_cache_root`]'s environment chain — every
/// variable arrives as a plain argument (CLAUDE.md: never `std::env::
/// set_var` in a test), mirroring `config::config_dir_from`'s identical
/// shape. An env var set to the empty string counts as unset, the same XDG
/// rule `config.rs` already applies to `$XDG_CONFIG_HOME`.
pub fn cache_root_from(
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_cache_home
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("thumbnails"));
    }
    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".cache/thumbnails"))
}

/// `$XDG_CACHE_HOME/thumbnails`, falling back to `~/.cache/thumbnails` —
/// resolved once at startup (`main.rs::App::new`) and cached on `App`,
/// never re-read per request. `None` (no `$HOME` either — a minimal CI
/// sandbox) means thumbnails are still generated per-request but never
/// persisted to disk; see [`generate_blocking`]'s handling of a `None`
/// cache root.
pub fn resolve_cache_root() -> Option<PathBuf> {
    cache_root_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

// ── size gate (files.toml's `thumbnail-max-mb`) ─────────────────────────

/// Whether a file this large should never be thumbnailed, per `files.toml`'s
/// `thumbnail-max-mb` knob — checked by `main.rs` before ever dispatching a
/// [`ThumbRequest`], so an oversized file never even reaches the semaphore/
/// `spawn_blocking` pipeline below.
pub fn exceeds_max_size(size_bytes: u64, max_mb: u64) -> bool {
    size_bytes > max_mb.saturating_mul(1024 * 1024)
}

// ── PNG metadata (tEXt chunks) ──────────────────────────────────────────

const KEY_URI: &str = "Thumb::URI";
const KEY_MTIME: &str = "Thumb::MTime";

/// Decimal seconds since the epoch — the spec's `Thumb::MTime` form. A
/// `modified` time before 1970 (possible on some filesystems/tarballs)
/// degrades to `"0"` rather than erroring — same "not worth a whole
/// calendar dependency, a sentinel is honest enough" posture
/// `list.rs::format_system_time` already takes for the analogous case.
fn mtime_to_string(modified: SystemTime) -> String {
    modified
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// Writes `image` as a PNG to `path`, with `Thumb::URI`/`Thumb::MTime`
/// `tEXt` chunks — the `png` crate directly, not `image`'s own PNG encoder,
/// which has no API for writing arbitrary `tEXt` chunks (see this crate's
/// `Cargo.toml` survey comment on why `png` is a direct dependency here).
fn write_png_with_metadata(
    path: &Path,
    uri: &str,
    mtime: &str,
    image: &image::RgbaImage,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|err| err.to_string())?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .add_text_chunk(KEY_URI.to_owned(), uri.to_owned())
        .map_err(|err| err.to_string())?;
    encoder
        .add_text_chunk(KEY_MTIME.to_owned(), mtime.to_owned())
        .map_err(|err| err.to_string())?;
    let mut writer = encoder.write_header().map_err(|err| err.to_string())?;
    writer
        .write_image_data(image.as_raw())
        .map_err(|err| err.to_string())?;
    writer.finish().map_err(|err| err.to_string())
}

/// Reads back `path`'s `Thumb::URI`/`Thumb::MTime` `tEXt` chunks, if both
/// are present — anything else (unreadable file, not a PNG, missing either
/// key) is `None`, treated by [`generate_blocking`] as an ordinary cache
/// miss, never an error worth surfacing.
fn read_cache_metadata(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(BufReader::new(file));
    let reader = decoder.read_info().ok()?;
    let info = reader.info();

    let mut uri = None;
    let mut mtime = None;
    for chunk in &info.uncompressed_latin1_text {
        match chunk.keyword.as_str() {
            KEY_URI => uri = Some(chunk.text.clone()),
            KEY_MTIME => mtime = Some(chunk.text.clone()),
            _ => {}
        }
    }
    Some((uri?, mtime?))
}

/// Writes `image` under `cache_root` atomically: a temp file in the *same*
/// directory as the final target (guarantees one filesystem, so `rename`
/// is atomic), `0600` permissions (the spec: a thumbnail can reveal a
/// private image's content), then `rename` over the final path — the
/// spec's own durability rule, so a concurrent reader (this app's own next
/// request, or another thumbnailer entirely) never observes a partially
/// written PNG. Best-effort: every error here is returned, not panicked on
/// (a read-only `$XDG_CACHE_HOME` is a normal Tuesday, not a crash) — see
/// [`generate_blocking`]'s caller, which still returns the freshly
/// generated image even when this fails.
fn write_cache_atomic(
    cache_root: &Path,
    uri: &str,
    mtime: &str,
    size: ThumbSize,
    image: &image::RgbaImage,
) -> Result<(), String> {
    write_png_atomic(&cache_path(cache_root, uri, size), uri, mtime, image)
}

/// Records "could not thumbnail `uri` as of `mtime`" in the fail cache: a
/// 1x1 transparent PNG carrying the same `Thumb::URI`/`Thumb::MTime` chunks
/// as a real thumbnail (the spec's own fail-marker shape), so
/// [`fail_marker_is_current`] validates it exactly like a cache hit.
fn write_fail_marker(cache_root: &Path, uri: &str, mtime: &str) -> Result<(), String> {
    let marker = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
    write_png_atomic(&fail_path(cache_root, uri), uri, mtime, &marker)
}

/// Whether the fail cache holds a marker for exactly this URI *and* mtime.
/// A changed mtime (file edited or replaced) makes the marker stale, so the
/// file gets one fresh attempt.
fn fail_marker_is_current(cache_root: &Path, uri: &str, mtime: &str) -> bool {
    read_cache_metadata(&fail_path(cache_root, uri))
        .is_some_and(|(cached_uri, cached_mtime)| cached_uri == uri && cached_mtime == mtime)
}

/// Atomic temp-file-then-rename PNG write to `final_path` (see
/// [`write_cache_atomic`]'s doc for why), creating parent directories.
fn write_png_atomic(
    final_path: &Path,
    uri: &str,
    mtime: &str,
    image: &image::RgbaImage,
) -> Result<(), String> {
    let dir = final_path.parent().ok_or("cache path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;

    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("thumbnail.png");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}-{nanos}", std::process::id()));

    if let Err(err) = write_png_with_metadata(&tmp_path, uri, mtime, image) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Err(err) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.to_string());
    }
    std::fs::rename(&tmp_path, final_path).map_err(|err| err.to_string())
}

// ── Thumbnailer trait + registry ────────────────────────────────────────

/// One thumbnail generator, dispatched by mimetype essence string. Additive
/// by design: a future video/PDF thumbnailer implements this trait and
/// registers alongside [`ImageThumbnailer`] without this trait or
/// [`Registry`] changing shape.
///
/// Plain (not `async_trait`) on purpose — every implementation's `generate`
/// is blocking CPU/disk work, always invoked from inside the one
/// `spawn_blocking` call [`thumbnail_for`] already wraps its whole pipeline
/// in (matching `modules::local::run_blocking`'s posture). An
/// implementation must never itself spawn async work or block on a runtime
/// it doesn't own.
pub trait Thumbnailer: Send + Sync {
    /// Whether this thumbnailer claims `mimetype` (an essence string like
    /// `"image/png"`, per `core::mime::MimeDb::guess`'s own contract — no
    /// `; charset=…` suffix to strip).
    fn handles(&self, mimetype: &str) -> bool;

    /// Decode `path` and return pixels no larger than `max_pixels` on their
    /// longest side, aspect preserved. `Err` means "couldn't thumbnail
    /// this one" (corrupt file, an unsupported sub-format, …) — the
    /// underlying `image` crate reports decode failures as `Result`, never
    /// a panic, so this is a thin wrapper, not a `catch_unwind`.
    fn generate(&self, path: &Path, max_pixels: u32) -> Result<image::RgbaImage, String>;
}

/// The `image` crate thumbnailer — every format that crate's
/// `default-formats` feature decodes (jpeg/png/gif/webp/bmp/ico/tiff/…, see
/// the dated survey comment on this crate's `image` dependency in
/// `Cargo.toml`), dispatched on the `"image/"` mimetype prefix.
pub struct ImageThumbnailer;

impl Thumbnailer for ImageThumbnailer {
    fn handles(&self, mimetype: &str) -> bool {
        mimetype.starts_with("image/")
    }

    fn generate(&self, path: &Path, max_pixels: u32) -> Result<image::RgbaImage, String> {
        // Decode by *content*, not extension: `image::open` trusts the file
        // name, which would fail for an extensionless or misnamed image
        // that the mimetype dispatch (content-sniffed) rightly sent here.
        let decoded = image::ImageReader::open(path)
            .and_then(|reader| reader.with_guessed_format())
            .map_err(|err| err.to_string())?
            .decode()
            .map_err(|err| err.to_string())?;
        // Never upscale: a 32 px icon stays 32 px (the view scales it), so
        // the big cache directories do not fill with blurry enlargements.
        if decoded.width() <= max_pixels && decoded.height() <= max_pixels {
            return Ok(decoded.to_rgba8());
        }
        let resized = decoded.resize(
            max_pixels,
            max_pixels,
            image::imageops::FilterType::Triangle,
        );
        Ok(resized.to_rgba8())
    }
}

/// Dispatches a mimetype to the first registered [`Thumbnailer`] that
/// claims it — first-registered-wins, so a future stage controls
/// precedence purely by registration order, not a separate priority field.
pub struct Registry {
    thumbnailers: Vec<Box<dyn Thumbnailer>>,
}

impl Registry {
    pub fn empty() -> Self {
        Registry {
            thumbnailers: Vec::new(),
        }
    }

    /// What `App` actually builds at startup: just [`ImageThumbnailer`]
    /// today — see this type's own doc comment on why a future
    /// video/PDF thumbnailer is purely additive here.
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        registry.register(Box::new(ImageThumbnailer));
        registry
    }

    pub fn register(&mut self, thumbnailer: Box<dyn Thumbnailer>) {
        self.thumbnailers.push(thumbnailer);
    }

    /// Whether any registered thumbnailer claims `mimetype`. Lets callers
    /// skip work (and not record a "failure") for types nothing can thumbnail.
    pub fn handles(&self, mimetype: &str) -> bool {
        self.find(mimetype).is_some()
    }

    fn find(&self, mimetype: &str) -> Option<&dyn Thumbnailer> {
        self.thumbnailers
            .iter()
            .find(|thumbnailer| thumbnailer.handles(mimetype))
            .map(|boxed| boxed.as_ref())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ── the handle newtype (the one iced import in core/thumbs.rs) ─────────

/// Wraps `iced::widget::image::Handle` so it can sit inside `App` state —
/// CLAUDE.md's iced-0.14 gotcha list: "`image::Handle` has no `Debug` —
/// newtype it before it enters any state struct that derives `Debug`."
/// Cheap to `Clone` (the `Rgba` variant's pixels live behind a
/// reference-counted `Bytes` buffer internally — cloning never copies the
/// decoded image data).
#[derive(Clone)]
pub struct ThumbHandle(iced::widget::image::Handle);

impl ThumbHandle {
    /// The wrapped handle, for `iced::widget::image(..)` call sites in
    /// `ui::dirview::list`/`grid`.
    pub fn handle(&self) -> iced::widget::image::Handle {
        self.0.clone()
    }
}

impl fmt::Debug for ThumbHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ThumbHandle(..)")
    }
}

fn to_handle(image: &image::RgbaImage) -> ThumbHandle {
    ThumbHandle(iced::widget::image::Handle::from_rgba(
        image.width(),
        image.height(),
        image.as_raw().clone(),
    ))
}

// ── the ~512-entry LRU of decoded handles ───────────────────────────────

struct ThumbCacheInner {
    entries: HashMap<Location, (SystemTime, ThumbHandle)>,
    order: VecDeque<Location>,
}

/// A bounded LRU of decoded [`ThumbHandle`]s — the shared, App-owned cache
/// CLAUDE.md calls for ("Shared caches … live on the App, never
/// per-view"). Keyed by [`Location`] *and* the `mtime` a handle was
/// generated for: a stale mtime is a clean miss (see [`Self::get_for`]),
/// not a served-forever stale image.
///
/// Interior mutability (`RefCell`): [`Self::get_for`] is called from
/// `view()`, which only ever holds `&DirectoryView`/`&App`, but still needs
/// to record LRU-touch order on a hit. This never crosses an `.await` or a
/// thread boundary — every touch happens either during a render or inside
/// `App::update`, both on the single UI thread iced drives `view`/`update`
/// from.
pub struct ThumbCache {
    capacity: usize,
    inner: RefCell<ThumbCacheInner>,
}

impl ThumbCache {
    pub fn new(capacity: usize) -> Self {
        ThumbCache {
            // A zero-capacity cache would make `insert` evict what it just
            // inserted — degenerate but not unsound; `.max(1)` just avoids
            // that pointless churn rather than trusting every call site to
            // pass something sane (no-panic-adjacent defensiveness, not a
            // hard requirement here since nothing panics either way).
            capacity: capacity.max(1),
            inner: RefCell::new(ThumbCacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// The cached handle for `location`, but only if it was generated for
    /// exactly `modified` — see the type's own doc comment on why a
    /// mismatch is a miss, not stale data. Touches LRU order on a hit.
    pub fn get_for(&self, location: &Location, modified: SystemTime) -> Option<ThumbHandle> {
        let mut inner = self.inner.borrow_mut();
        let is_current = matches!(
            inner.entries.get(location),
            Some((mtime, _)) if *mtime == modified
        );
        if !is_current {
            return None;
        }
        Self::touch(&mut inner.order, location);
        inner
            .entries
            .get(location)
            .map(|(_, handle)| handle.clone())
    }

    /// Inserts (or replaces) `location`'s handle for `modified`, evicting
    /// the least-recently-touched entry first if this would exceed
    /// `capacity`.
    pub fn insert(&self, location: Location, modified: SystemTime, handle: ThumbHandle) {
        let mut inner = self.inner.borrow_mut();
        if inner.entries.contains_key(&location) {
            inner.entries.insert(location.clone(), (modified, handle));
            Self::touch(&mut inner.order, &location);
            return;
        }
        if inner.entries.len() >= self.capacity
            && let Some(oldest) = inner.order.pop_front()
        {
            inner.entries.remove(&oldest);
        }
        inner.order.push_back(location.clone());
        inner.entries.insert(location, (modified, handle));
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn touch(order: &mut VecDeque<Location>, location: &Location) {
        if let Some(pos) = order.iter().position(|entry| entry == location)
            && let Some(entry) = order.remove(pos)
        {
            order.push_back(entry);
        }
    }
}

// ── the async entry point ───────────────────────────────────────────────

/// Everything [`thumbnail_for`] needs for one entry. `location` is expected
/// local (see [`generate_blocking`]'s guard) — `main.rs` only ever builds
/// one of these for a `DirectoryView` whose backend claims
/// `Caps::THUMBNAILS`, which today only `LocalBackend` does.
#[derive(Debug, Clone)]
pub struct ThumbRequest {
    pub location: Location,
    pub mimetype: String,
    pub modified: SystemTime,
    /// Which spec size directory to read or fill; see [`ThumbSize::for_pixels`].
    pub size: ThumbSize,
}

/// The blocking read-existing-then-generate pipeline — everything from here
/// down runs inside [`thumbnail_for`]'s single `spawn_blocking` call.
/// `None` means "no thumbnail" (unsupported mimetype, decode failure, a
/// non-local location, …) — the caller falls back to the glyph icon.
fn generate_blocking(
    registry: &Registry,
    cache_root: Option<&Path>,
    request: &ThumbRequest,
) -> Option<image::RgbaImage> {
    if !request.location.is_local() {
        return None;
    }
    let uri = canonical_uri(&request.location.path);
    let mtime = mtime_to_string(request.modified);

    if let Some(root) = cache_root {
        let path = cache_path(root, &uri, request.size);
        if let Some((cached_uri, cached_mtime)) = read_cache_metadata(&path)
            && cached_uri == uri
            && cached_mtime == mtime
            && let Ok(cached) = image::open(&path)
        {
            return Some(cached.to_rgba8());
        }
        // A known-bad file at this exact mtime: do not decode it again.
        if fail_marker_is_current(root, &uri, &mtime) {
            return None;
        }
    }

    // No thumbnailer for this type is not a failure of the *file* (a later
    // thumbnailer might claim it), so it leaves no fail marker.
    let thumbnailer = registry.find(&request.mimetype)?;
    let rgba = match thumbnailer.generate(&request.location.path, request.size.max_pixels()) {
        Ok(rgba) => rgba,
        Err(_) => {
            if let Some(root) = cache_root
                && let Err(err) = write_fail_marker(root, &uri, &mtime)
            {
                eprintln!(
                    "saola-files: could not record a thumbnail failure for {}: {err}",
                    request.location
                );
            }
            return None;
        }
    };

    if let Some(root) = cache_root
        && let Err(err) = write_cache_atomic(root, &uri, &mtime, request.size, &rgba)
    {
        eprintln!(
            "saola-files: could not cache a thumbnail for {}: {err}",
            request.location
        );
    }

    Some(rgba)
}

/// Generates or fetches (from the freedesktop disk cache) a thumbnail for
/// `request`, bounded by `semaphore` (Stage 11's "never unbounded task
/// spawns" rule — see `main.rs`'s `THUMB_MAX_CONCURRENT`). The whole body
/// is blocking I/O/CPU work, so it all runs inside one `spawn_blocking`
/// call, matching every other blocking-work wrapper in this crate
/// (`modules::local::run_blocking`, `core::clipboard_interop::
/// run_blocking`). `None` means "no thumbnail" — the caller (`main.rs`)
/// falls back to the glyph icon and remembers not to retry this exact
/// `(location, modified)` pair for the rest of the session.
pub async fn thumbnail_for(
    registry: Arc<Registry>,
    semaphore: Arc<Semaphore>,
    cache_root: Option<PathBuf>,
    request: ThumbRequest,
) -> Option<ThumbHandle> {
    // A semaphore that's been explicitly closed would make `acquire_owned`
    // return `Err` — never happens today (nothing ever calls `close()` on
    // it), but treated the same as "no permit available this round" per
    // the no-panic rule, not an `.unwrap()`.
    let Ok(_permit) = semaphore.acquire_owned().await else {
        return None;
    };
    let image = tokio::task::spawn_blocking(move || {
        generate_blocking(&registry, cache_root.as_deref(), &request)
    })
    .await
    .ok()
    .flatten();
    image.as_ref().map(to_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-thumbs-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn solid_image(width: u32, height: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(width, height, image::Rgba([200, 100, 50, 255]))
    }

    // ── cache path / key computation ────────────────────────────────────

    #[test]
    fn canonical_uri_percent_encodes_the_path() {
        assert_eq!(
            canonical_uri(Path::new("/home/jordan/My File.jpg")),
            "file:///home/jordan/My%20File.jpg"
        );
    }

    #[test]
    fn cache_path_uses_the_md5_of_the_uri_under_the_size_directory() {
        let root = Path::new("/cache/thumbnails");
        let uri = "file:///home/jordan/photo.jpg";
        let path = cache_path(root, uri, ThumbSize::Normal);
        assert_eq!(
            path,
            root.join("normal")
                .join(format!("{}.png", md5::hex_digest(uri.as_bytes())))
        );
    }

    #[test]
    fn cache_path_is_stable_and_deterministic_for_the_same_uri() {
        let root = Path::new("/cache/thumbnails");
        let uri = "file:///a/b.png";
        assert_eq!(
            cache_path(root, uri, ThumbSize::Normal),
            cache_path(root, uri, ThumbSize::Normal)
        );
    }

    #[test]
    fn different_uris_produce_different_cache_paths() {
        let root = Path::new("/cache/thumbnails");
        assert_ne!(
            cache_path(root, "file:///a", ThumbSize::Normal),
            cache_path(root, "file:///b", ThumbSize::Normal)
        );
    }

    #[test]
    fn cache_root_chain_prefers_xdg_then_home_then_none() {
        assert_eq!(
            cache_root_from(os("/xdg-cache"), os("/home/j")),
            Some(PathBuf::from("/xdg-cache/thumbnails"))
        );
        assert_eq!(
            cache_root_from(None, os("/home/j")),
            Some(PathBuf::from("/home/j/.cache/thumbnails"))
        );
        assert_eq!(cache_root_from(None, None), None);
        // Empty env vars count as unset, same rule `config.rs` applies.
        assert_eq!(
            cache_root_from(os(""), os("/home/j")),
            Some(PathBuf::from("/home/j/.cache/thumbnails"))
        );
    }

    #[test]
    fn exceeds_max_size_gates_on_the_configured_cap() {
        assert!(!exceeds_max_size(10 * 1024 * 1024, 64));
        assert!(exceeds_max_size(65 * 1024 * 1024, 64));
        assert!(exceeds_max_size(1, 0));
    }

    // ── tEXt metadata: write then read back ─────────────────────────────

    #[test]
    fn png_metadata_round_trips_through_a_real_file() {
        let dir = tempdir();
        let path = dir.join("thumb.png");
        let image = solid_image(4, 4);

        write_png_with_metadata(&path, "file:///a/b.jpg", "1699999999", &image).unwrap();

        let (uri, mtime) = read_cache_metadata(&path).unwrap();
        assert_eq!(uri, "file:///a/b.jpg");
        assert_eq!(mtime, "1699999999");

        // The pixels themselves also round-trip, not just the metadata.
        let decoded = image::open(&path).unwrap().to_rgba8();
        assert_eq!(decoded, image);

        cleanup(dir);
    }

    #[test]
    fn read_cache_metadata_is_none_for_a_missing_file() {
        let dir = tempdir();
        assert!(read_cache_metadata(&dir.join("nope.png")).is_none());
        cleanup(dir);
    }

    #[test]
    fn read_cache_metadata_is_none_for_a_png_with_no_thumb_chunks() {
        let dir = tempdir();
        let path = dir.join("plain.png");
        let image = solid_image(2, 2);
        image.save(&path).unwrap();

        assert!(read_cache_metadata(&path).is_none());
        cleanup(dir);
    }

    #[test]
    fn write_cache_atomic_writes_0600_permissions_and_a_readable_entry() {
        let dir = tempdir();
        let image = solid_image(8, 8);
        let uri = canonical_uri(&dir.join("source.png"));

        write_cache_atomic(&dir, &uri, "42", ThumbSize::Normal, &image).unwrap();

        let path = cache_path(&dir, &uri, ThumbSize::Normal);
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let (cached_uri, cached_mtime) = read_cache_metadata(&path).unwrap();
        assert_eq!(cached_uri, uri);
        assert_eq!(cached_mtime, "42");

        // No leftover temp file after a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("normal"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());

        cleanup(dir);
    }

    // ── generate_blocking: disk-cache hit vs. miss ──────────────────────

    #[test]
    fn generate_blocking_generates_and_caches_on_a_miss_then_hits_the_cache_next_time() {
        let dir = tempdir();
        let cache_root = dir.join("cache");
        let source_path = dir.join("source.png");
        solid_image(20, 10).save(&source_path).unwrap();

        let request = ThumbRequest {
            location: Location::local(&source_path),
            mimetype: "image/png".to_owned(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000),
            size: ThumbSize::Normal,
        };
        let registry = Registry::with_defaults();

        let first = generate_blocking(&registry, Some(&cache_root), &request);
        assert!(first.is_some());
        assert!(cache_path(&cache_root, &canonical_uri(&source_path), ThumbSize::Normal).exists());

        // Delete the source — a second call must come back from the disk
        // cache alone (proves the cache, not a fresh decode, served this).
        std::fs::remove_file(&source_path).unwrap();
        let second = generate_blocking(&registry, Some(&cache_root), &request);
        assert!(second.is_some());

        cleanup(dir);
    }

    #[test]
    fn generate_blocking_ignores_a_cache_entry_with_a_different_mtime() {
        let dir = tempdir();
        let cache_root = dir.join("cache");
        let source_path = dir.join("source.png");
        solid_image(6, 6).save(&source_path).unwrap();

        let uri = canonical_uri(&source_path);
        let stale_image = solid_image(6, 6);
        write_cache_atomic(&cache_root, &uri, "1", ThumbSize::Normal, &stale_image).unwrap();

        let request = ThumbRequest {
            location: Location::local(&source_path),
            mimetype: "image/png".to_owned(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2),
            size: ThumbSize::Normal,
        };
        let registry = Registry::with_defaults();
        let result = generate_blocking(&registry, Some(&cache_root), &request);
        assert!(result.is_some());

        // The cache entry on disk was overwritten with the fresh MTime,
        // not left stale.
        let (_, cached_mtime) =
            read_cache_metadata(&cache_path(&cache_root, &uri, ThumbSize::Normal)).unwrap();
        assert_eq!(cached_mtime, "2");

        cleanup(dir);
    }

    #[test]
    fn generate_blocking_returns_none_for_an_unsupported_mimetype() {
        let dir = tempdir();
        let source_path = dir.join("archive.tar");
        std::fs::write(&source_path, b"not an image").unwrap();

        let request = ThumbRequest {
            location: Location::local(&source_path),
            mimetype: "application/x-tar".to_owned(),
            modified: SystemTime::now(),
            size: ThumbSize::Normal,
        };
        let registry = Registry::with_defaults();
        assert!(generate_blocking(&registry, Some(&dir), &request).is_none());

        cleanup(dir);
    }

    #[test]
    fn generate_blocking_returns_none_for_a_non_local_location() {
        let request = ThumbRequest {
            location: Location {
                scheme: "sftp".to_owned(),
                authority: Some("host".to_owned()),
                path: PathBuf::from("/photo.jpg"),
            },
            mimetype: "image/jpeg".to_owned(),
            modified: SystemTime::now(),
            size: ThumbSize::Normal,
        };
        let registry = Registry::with_defaults();
        assert!(generate_blocking(&registry, None, &request).is_none());
    }

    #[test]
    fn generate_blocking_still_returns_an_image_when_the_cache_root_is_none() {
        let dir = tempdir();
        let source_path = dir.join("source.png");
        solid_image(4, 4).save(&source_path).unwrap();

        let request = ThumbRequest {
            location: Location::local(&source_path),
            mimetype: "image/png".to_owned(),
            modified: SystemTime::now(),
            size: ThumbSize::Normal,
        };
        let registry = Registry::with_defaults();
        assert!(generate_blocking(&registry, None, &request).is_some());

        cleanup(dir);
    }

    // ── registry dispatch by mimetype ───────────────────────────────────

    #[test]
    fn registry_dispatches_image_mimetypes_to_the_image_thumbnailer() {
        let registry = Registry::with_defaults();
        assert!(registry.find("image/png").is_some());
        assert!(registry.find("image/jpeg").is_some());
    }

    #[test]
    fn registry_has_no_thumbnailer_for_an_unclaimed_mimetype() {
        let registry = Registry::with_defaults();
        assert!(registry.find("application/pdf").is_none());
    }

    #[test]
    fn empty_registry_dispatches_nothing() {
        let registry = Registry::empty();
        assert!(registry.find("image/png").is_none());
    }

    #[test]
    fn a_registered_thumbnailer_takes_precedence_over_a_later_one_for_the_same_mimetype() {
        struct AlwaysHandles;
        impl Thumbnailer for AlwaysHandles {
            fn handles(&self, _mimetype: &str) -> bool {
                true
            }
            fn generate(&self, _path: &Path, _max_pixels: u32) -> Result<image::RgbaImage, String> {
                Err("marker: AlwaysHandles was the one dispatched to".to_owned())
            }
        }

        let mut registry = Registry::empty();
        registry.register(Box::new(AlwaysHandles));
        registry.register(Box::new(ImageThumbnailer));
        // `AlwaysHandles` was registered first, so *it* is what `find`
        // returns for a mimetype `ImageThumbnailer` would also claim —
        // proven by actually invoking `generate` and checking which
        // implementation's error came back, not just that dispatch
        // returned `Some`.
        let dispatched = registry.find("image/png").unwrap();
        let err = dispatched
            .generate(Path::new("/irrelevant"), 128)
            .unwrap_err();
        assert_eq!(err, "marker: AlwaysHandles was the one dispatched to");
    }

    // ── sizes, fail cache, mime dispatch (#61) ──────────────────────────

    #[test]
    fn for_pixels_picks_the_smallest_covering_size() {
        assert_eq!(ThumbSize::for_pixels(0), ThumbSize::Normal);
        assert_eq!(ThumbSize::for_pixels(128), ThumbSize::Normal);
        assert_eq!(ThumbSize::for_pixels(129), ThumbSize::Large);
        assert_eq!(ThumbSize::for_pixels(256), ThumbSize::Large);
        assert_eq!(ThumbSize::for_pixels(300), ThumbSize::XLarge);
        assert_eq!(ThumbSize::for_pixels(512), ThumbSize::XLarge);
        assert_eq!(ThumbSize::for_pixels(513), ThumbSize::XXLarge);
        assert_eq!(ThumbSize::for_pixels(u32::MAX), ThumbSize::XXLarge);
    }

    #[test]
    fn size_directories_match_the_spec() {
        let dirs: Vec<_> = [
            ThumbSize::Normal,
            ThumbSize::Large,
            ThumbSize::XLarge,
            ThumbSize::XXLarge,
        ]
        .iter()
        .map(|size| (size.dir_name(), size.max_pixels()))
        .collect();
        assert_eq!(
            dirs,
            [
                ("normal", 128),
                ("large", 256),
                ("x-large", 512),
                ("xx-large", 1024)
            ]
        );
    }

    #[test]
    fn fail_path_lives_under_fail_saola_files() {
        let root = Path::new("/cache/thumbnails");
        let uri = "file:///a/b.png";
        assert_eq!(
            fail_path(root, uri),
            root.join("fail")
                .join("saola-files")
                .join(format!("{}.png", md5::hex_digest(uri.as_bytes())))
        );
    }

    fn request_for(path: &Path, secs: u64, size: ThumbSize, mimetype: &str) -> ThumbRequest {
        ThumbRequest {
            location: Location::local(path),
            mimetype: mimetype.to_owned(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            size,
        }
    }

    #[test]
    fn a_512_px_request_writes_to_x_large_and_reuses_it() {
        let dir = tempdir();
        let cache_root = dir.join("cache");
        let source = dir.join("big.png");
        solid_image(900, 600).save(&source).unwrap();
        let size = ThumbSize::for_pixels(512);
        let request = request_for(&source, 1000, size, "image/png");
        let registry = Registry::with_defaults();

        let first = generate_blocking(&registry, Some(&cache_root), &request).unwrap();
        assert_eq!(first.width(), 512);
        let cached = cache_path(&cache_root, &canonical_uri(&source), size);
        assert!(cached.starts_with(cache_root.join("x-large")));
        assert!(cached.exists());
        assert!(
            !cache_path(&cache_root, &canonical_uri(&source), ThumbSize::Normal).exists(),
            "only the requested size directory is written"
        );

        std::fs::remove_file(&source).unwrap();
        assert!(generate_blocking(&registry, Some(&cache_root), &request).is_some());
        cleanup(dir);
    }

    #[test]
    fn image_thumbnailer_does_not_upscale_small_images() {
        let dir = tempdir();
        let source = dir.join("tiny.png");
        solid_image(16, 8).save(&source).unwrap();
        let out = ImageThumbnailer.generate(&source, 512).unwrap();
        assert_eq!((out.width(), out.height()), (16, 8));
        cleanup(dir);
    }

    #[test]
    fn a_corrupt_image_writes_one_fail_entry_and_is_not_retried_until_mtime_changes() {
        let dir = tempdir();
        let cache_root = dir.join("cache");
        let source = dir.join("broken.png");
        std::fs::write(&source, b"definitely not a png").unwrap();
        let registry = Registry::with_defaults();
        let request = request_for(&source, 1000, ThumbSize::Normal, "image/png");
        let uri = canonical_uri(&source);

        assert!(generate_blocking(&registry, Some(&cache_root), &request).is_none());
        let marker = fail_path(&cache_root, &uri);
        assert!(marker.exists());
        let fail_files = std::fs::read_dir(marker.parent().unwrap()).unwrap().count();
        assert_eq!(fail_files, 1);

        // Replace the file with a valid image *without* changing the
        // recorded mtime: the marker must short-circuit, proving no retry.
        solid_image(8, 8).save(&source).unwrap();
        assert!(generate_blocking(&registry, Some(&cache_root), &request).is_none());

        // A changed mtime invalidates the marker and the file is retried.
        let newer = request_for(&source, 2000, ThumbSize::Normal, "image/png");
        assert!(generate_blocking(&registry, Some(&cache_root), &newer).is_some());
        cleanup(dir);
    }

    #[test]
    fn an_unclaimed_mimetype_leaves_no_fail_marker() {
        let dir = tempdir();
        let cache_root = dir.join("cache");
        let source = dir.join("doc.pdf");
        std::fs::write(&source, b"%PDF").unwrap();
        let request = request_for(&source, 1, ThumbSize::Normal, "application/pdf");
        assert!(
            generate_blocking(&Registry::with_defaults(), Some(&cache_root), &request).is_none()
        );
        assert!(!cache_root.join("fail").exists());
        cleanup(dir);
    }

    #[test]
    fn dispatch_follows_the_mimetype_not_the_file_name() {
        // An image whose name says ".txt" thumbnails when the mimetype says
        // image; a ".png" name with a text mimetype does not.
        let dir = tempdir();
        let source = dir.join("picture.txt");
        solid_image(8, 8)
            .save_with_format(&source, image::ImageFormat::Png)
            .unwrap();
        let registry = Registry::with_defaults();
        let as_image = request_for(&source, 1, ThumbSize::Normal, "image/png");
        assert!(generate_blocking(&registry, None, &as_image).is_some());
        let as_text = request_for(&source, 1, ThumbSize::Normal, "text/plain");
        assert!(generate_blocking(&registry, None, &as_text).is_none());
        assert!(registry.handles("image/webp") && !registry.handles("text/plain"));
        cleanup(dir);
    }

    // ── LRU eviction behavior ────────────────────────────────────────────

    fn handle() -> ThumbHandle {
        ThumbHandle(iced::widget::image::Handle::from_rgba(
            1,
            1,
            vec![0, 0, 0, 255],
        ))
    }

    #[test]
    fn lru_returns_what_was_inserted() {
        let cache = ThumbCache::new(4);
        let location = Location::local("/a.jpg");
        let mtime = SystemTime::now();
        cache.insert(location.clone(), mtime, handle());
        assert!(cache.get_for(&location, mtime).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn lru_misses_on_a_different_mtime_than_what_was_cached() {
        let cache = ThumbCache::new(4);
        let location = Location::local("/a.jpg");
        let original = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        let changed = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2);
        cache.insert(location.clone(), original, handle());
        assert!(cache.get_for(&location, changed).is_none());
    }

    #[test]
    fn lru_evicts_the_least_recently_touched_entry_once_full() {
        let cache = ThumbCache::new(2);
        let mtime = SystemTime::now();
        let a = Location::local("/a.jpg");
        let b = Location::local("/b.jpg");
        let c = Location::local("/c.jpg");

        cache.insert(a.clone(), mtime, handle());
        cache.insert(b.clone(), mtime, handle());
        // Touch `a` so `b` becomes the least-recently-used entry.
        assert!(cache.get_for(&a, mtime).is_some());
        cache.insert(c.clone(), mtime, handle());

        assert_eq!(cache.len(), 2);
        assert!(cache.get_for(&a, mtime).is_some(), "recently touched, kept");
        assert!(cache.get_for(&c, mtime).is_some(), "just inserted, kept");
        assert!(
            cache.get_for(&b, mtime).is_none(),
            "least-recently-used, evicted"
        );
    }

    #[test]
    fn lru_reinserting_an_existing_key_updates_it_without_growing() {
        let cache = ThumbCache::new(4);
        let location = Location::local("/a.jpg");
        let first_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        let second_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2);

        cache.insert(location.clone(), first_mtime, handle());
        cache.insert(location.clone(), second_mtime, handle());

        assert_eq!(cache.len(), 1);
        assert!(cache.get_for(&location, first_mtime).is_none());
        assert!(cache.get_for(&location, second_mtime).is_some());
    }

    #[test]
    fn lru_capacity_is_never_exceeded_across_many_inserts() {
        let cache = ThumbCache::new(8);
        let mtime = SystemTime::now();
        for i in 0..100 {
            cache.insert(Location::local(format!("/{i}.jpg")), mtime, handle());
        }
        assert_eq!(cache.len(), 8);
    }
}
