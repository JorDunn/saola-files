//! Place-provider registry: merges the sidebar's non-live rows — the
//! user's home, the well-known XDG user directories (hand-parsed from
//! `user-dirs.dirs`), GTK-format bookmarks, saved `[[server]]` config
//! entries (Stage 13 wires their "Connect" action), and a Trash entry —
//! into one ordered [`Place`] list. iced-free, like every other `core/`
//! module (CLAUDE.md's layering rule): `ui::sidebar` is the only thing
//! that turns this into widgets.
//!
//! [`core::udisks`](crate::core::udisks)'s live [`Mount`](crate::core::udisks::Mount)s
//! are deliberately **not** merged in here — they're a separate,
//! continuously-updating section `ui::sidebar` composes alongside this
//! registry's output. Merging them would mean re-running [`build`] every
//! time a USB drive appears, which would also re-read the bookmarks and
//! `user-dirs.dirs` files off disk for no reason; keeping the two apart
//! means the (rare, disk-backed) static list and the (frequent, D-Bus-fed)
//! live list can update completely independently.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::config::SavedServer;
use crate::core::vfs::Location;

/// What kind of shortcut a [`Place`] is — the sidebar reads this to pick
/// an icon ([`crate::icons::for_place`]); nothing else in `core/`
/// branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    Home,
    Desktop,
    Downloads,
    Documents,
    Music,
    Pictures,
    Videos,
    Bookmark,
    Server,
    Trash,
}

/// One row the places sidebar can render, independent of *how* it got
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub location: Location,
    pub kind: PlaceKind,
}

/// The well-known XDG user directories, as parsed from `user-dirs.dirs`.
/// Every field is `None` when the file is missing, unreadable, or simply
/// doesn't mention that directory — `xdg-user-dirs-update` only writes the
/// ones a desktop session actually created, so a fresh/minimal system
/// legitimately has gaps here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserDirs {
    pub desktop: Option<PathBuf>,
    pub download: Option<PathBuf>,
    pub templates: Option<PathBuf>,
    pub public_share: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    pub music: Option<PathBuf>,
    pub pictures: Option<PathBuf>,
    pub videos: Option<PathBuf>,
}

impl UserDirs {
    /// Reads and parses `$XDG_CONFIG_HOME/user-dirs.dirs` (falling back to
    /// `~/.config/user-dirs.dirs`) — the standard file `xdg-user-dirs`
    /// writes, honored by every mainstream Linux desktop, not a
    /// Saola-specific format. A missing or unreadable file degrades to
    /// every field `None`, silently — the same posture `Config::load`
    /// takes for `files.toml` (CLAUDE.md: "no file → silent defaults").
    /// There are no per-knob warnings here (unlike `Config::parse`)
    /// because there are no knobs to name, just directories that either
    /// resolved or didn't.
    pub fn load(home: &Path) -> Self {
        let Some(config_home) = xdg_config_home(home) else {
            return Self::default();
        };
        let Ok(contents) = std::fs::read_to_string(config_home.join("user-dirs.dirs")) else {
            return Self::default();
        };
        Self::parse(&contents, home)
    }

    /// The testable core of [`Self::load`]: a pure function of the file's
    /// contents and the home directory to expand a leading `$HOME` token
    /// against — no environment reads here (CLAUDE.md: never
    /// `std::env::set_var` in a test; resolve env at a thin wrapper, which
    /// [`Self::load`] is).
    pub fn parse(contents: &str, home: &Path) -> Self {
        let mut dirs = UserDirs::default();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let Some(path) = parse_quoted_path(value.trim(), home) else {
                continue;
            };
            match key.trim() {
                "XDG_DESKTOP_DIR" => dirs.desktop = Some(path),
                "XDG_DOWNLOAD_DIR" => dirs.download = Some(path),
                "XDG_TEMPLATES_DIR" => dirs.templates = Some(path),
                "XDG_PUBLICSHARE_DIR" => dirs.public_share = Some(path),
                "XDG_DOCUMENTS_DIR" => dirs.documents = Some(path),
                "XDG_MUSIC_DIR" => dirs.music = Some(path),
                "XDG_PICTURES_DIR" => dirs.pictures = Some(path),
                "XDG_VIDEOS_DIR" => dirs.videos = Some(path),
                _ => {}
            }
        }
        dirs
    }
}

/// `"$HOME/Downloads"` → `/home/jordan/Downloads`. `xdg-user-dirs` quotes
/// every value and only ever expands a *leading* literal `$HOME` token
/// (never arbitrary shell substitution, never `$HOME` mid-string) — this
/// mirrors exactly that, not a general shell-quote parser. Built from
/// `OsString` pieces (not `Path::display`/formatting) so a non-UTF-8 home
/// directory still round-trips correctly into the resulting `PathBuf`;
/// only the `$HOME`-relative *suffix* is guaranteed to be UTF-8 (it comes
/// straight out of the text file).
fn parse_quoted_path(value: &str, home: &Path) -> Option<PathBuf> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let path = match inner.strip_prefix("$HOME") {
        Some(rest) => {
            let mut combined = home.as_os_str().to_os_string();
            combined.push(rest);
            PathBuf::from(combined)
        }
        None => PathBuf::from(inner),
    };
    (!path.as_os_str().is_empty()).then_some(path)
}

fn xdg_config_home(home: &Path) -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home.join(".config")))
}

/// One bookmarked folder — a user-curated shortcut, distinct from the
/// well-known [`UserDirs`] (those are *discovered*; these are *chosen*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub label: String,
    pub location: Location,
}

/// Reads and parses `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (falling back to
/// `~/.config/gtk-3.0/bookmarks`) — the de facto standard bookmarks file
/// every mainstream Linux file manager (Nautilus, Thunar, PCManFM, …)
/// reads and writes, which is why Saola reads it too rather than inventing
/// a second, disconnected bookmarks store. Missing/unreadable file → no
/// bookmarks, silently (same posture as [`UserDirs::load`]).
pub fn load_bookmarks(home: &Path) -> Vec<Bookmark> {
    let Some(config_home) = xdg_config_home(home) else {
        return Vec::new();
    };
    match std::fs::read_to_string(config_home.join("gtk-3.0/bookmarks")) {
        Ok(contents) => parse_bookmarks(&contents),
        Err(_) => Vec::new(),
    }
}

/// The testable core of [`load_bookmarks`]. One bookmark per line: a
/// `file://`-scheme, percent-encoded URI, optionally followed by a space
/// and a display label. A line with an unrecognized scheme (a `sftp://`
/// or `smb://` bookmark left by another file manager — nothing Saola can
/// browse yet) or that otherwise fails to decode is skipped — a
/// hand-edited or partially-foreign bookmarks file loses that one line,
/// never the rest (the same per-entry degradation `config.rs`'s
/// `[[action]]`/`[[server]]` parsing uses, just without the warnings: an
/// unrecognized *scheme* here is routine, not a typo to flag).
pub fn parse_bookmarks(contents: &str) -> Vec<Bookmark> {
    contents.lines().filter_map(parse_bookmark_line).collect()
}

fn parse_bookmark_line(line: &str) -> Option<Bookmark> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (uri, label) = match line.split_once(' ') {
        Some((uri, label)) => (uri, Some(label.trim())),
        None => (line, None),
    };
    let encoded_path = uri.strip_prefix("file://")?;
    let path_bytes = decode_percent(encoded_path);
    let path = PathBuf::from(OsString::from_vec(path_bytes));

    let label = match label {
        Some(label) if !label.is_empty() => label.to_owned(),
        _ => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
    };

    Some(Bookmark {
        label,
        location: Location::local(path),
    })
}

/// The inverse of [`parse_bookmarks`]. Not wired to any UI action yet (no
/// "add bookmark" affordance exists until a later stage), but a real
/// round-trip needs a real writer — and having one is what lets the unit
/// test below prove `parse_bookmarks` is lossless for the cases it claims
/// to support, rather than just asserting against a hand-written fixture
/// string.
pub fn format_bookmarks(bookmarks: &[Bookmark]) -> String {
    let mut out = String::new();
    for bookmark in bookmarks {
        out.push_str("file://");
        out.push_str(&encode_percent(
            bookmark.location.path.as_os_str().as_bytes(),
        ));
        out.push(' ');
        out.push_str(&bookmark.label);
        out.push('\n');
    }
    out
}

/// Decodes `%XX` escapes in an ASCII URI path into raw bytes, leaving
/// every other byte untouched. A byte sequence with a truncated or
/// non-hex `%` escape near the end keeps the `%` literally rather than
/// panicking on an out-of-range slice — a malformed bookmark line should
/// degrade to a slightly wrong label, never crash the sidebar.
///
/// `pub(crate)` (Stage 9): `core::fs::trash` reuses this for `.trashinfo`
/// `Path=` values, which the freedesktop Trash spec percent-encodes the
/// same way a bookmarks URI does — one escaping scheme, not two hand-
/// rolled copies.
pub(crate) fn decode_percent(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The inverse of [`decode_percent`]: everything in RFC 3986's `unreserved`
/// set (plus `/`, so paths stay readable) passes through as-is; everything
/// else — spaces, and any byte a non-UTF-8 filename might carry — becomes
/// a `%XX` escape. Not a general RFC 3986 encoder (same documented
/// simplification `core::apps::file_uri` already takes for `Exec=` field
/// codes): good enough for round-tripping what [`decode_percent`] produces.
///
/// `pub(crate)`: see [`decode_percent`]'s doc comment — `core::fs::trash`
/// is the second caller, encoding `.trashinfo` `Path=` values.
pub(crate) fn encode_percent(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The location the sidebar's Trash entry navigates to. **Stage 9 update:**
/// no backend actually implements the `trash` scheme (there is no
/// `modules::trash` — see `core::fs::trash`'s module doc comment on why
/// trash stays a standalone local-only module rather than a `Backend`
/// impl). Instead, `main.rs::App::navigate_active` special-cases this exact
/// `scheme == "trash"` sentinel to swap in `ui::trashview::TrashView`
/// instead of navigating a `DirectoryView` — `pub` (not `pub(crate)`: the
/// binary crate `main.rs` lives in is a separate crate from this library,
/// per `lib.rs`'s doc comment on the split, so `main.rs` needs the same
/// visibility any other external consumer of this crate's public API
/// would) so `main.rs` can recognize the same value this function hands
/// the sidebar, without a second hand-written copy of the sentinel
/// drifting out of sync with it.
pub fn trash_location() -> Location {
    Location {
        scheme: "trash".to_owned(),
        authority: None,
        path: PathBuf::from("/"),
    }
}

/// Builds the sidebar's non-live place list, in display order: Home, the
/// well-known XDG directories that actually resolved, user bookmarks,
/// Trash, then saved servers last (Stage 13's "Connect" section — see the
/// module docs on why this list stays separate from the live udisks
/// mounts). Every input is already-parsed data (`UserDirs`, `Bookmark`s,
/// `SavedServer`s) rather than a home directory this function reads files
/// from itself — callers own *how* each was loaded (real files vs. a test
/// fixture), which is what makes the merge order itself unit-testable
/// without touching disk.
pub fn build(
    home: &Path,
    user_dirs: &UserDirs,
    bookmarks: &[Bookmark],
    servers: &[SavedServer],
) -> Vec<Place> {
    let mut places = vec![Place {
        label: "Home".to_owned(),
        location: Location::local(home.to_path_buf()),
        kind: PlaceKind::Home,
    }];

    let mut push_dir = |label: &str, kind: PlaceKind, dir: &Option<PathBuf>| {
        if let Some(dir) = dir {
            places.push(Place {
                label: label.to_owned(),
                location: Location::local(dir.clone()),
                kind,
            });
        }
    };
    push_dir("Desktop", PlaceKind::Desktop, &user_dirs.desktop);
    push_dir("Downloads", PlaceKind::Downloads, &user_dirs.download);
    push_dir("Documents", PlaceKind::Documents, &user_dirs.documents);
    push_dir("Music", PlaceKind::Music, &user_dirs.music);
    push_dir("Pictures", PlaceKind::Pictures, &user_dirs.pictures);
    push_dir("Videos", PlaceKind::Videos, &user_dirs.videos);

    for bookmark in bookmarks {
        places.push(Place {
            label: bookmark.label.clone(),
            location: bookmark.location.clone(),
            kind: PlaceKind::Bookmark,
        });
    }

    places.push(Place {
        label: "Trash".to_owned(),
        location: trash_location(),
        kind: PlaceKind::Trash,
    });

    for server in servers {
        places.push(Place {
            label: server.name.clone(),
            location: Location::parse(&server.uri),
            kind: PlaceKind::Server,
        });
    }

    places
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── xdg-user-dirs parsing ────────────────────────────────────────────

    const SAMPLE_USER_DIRS: &str = "\
# This file is written by xdg-user-dirs-update
# If you want to change or add directories, just edit the line you're
# interested in. All local changes will be retained on the next run.
XDG_DESKTOP_DIR=\"$HOME/Desktop\"
XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"
XDG_TEMPLATES_DIR=\"$HOME/Templates\"
XDG_PUBLICSHARE_DIR=\"$HOME/Public\"
XDG_DOCUMENTS_DIR=\"$HOME/Documents\"
XDG_MUSIC_DIR=\"$HOME/Music\"
XDG_PICTURES_DIR=\"$HOME/Pictures\"
XDG_VIDEOS_DIR=\"$HOME/Videos\"
";

    #[test]
    fn user_dirs_parse_expands_home_and_skips_comments() {
        let home = Path::new("/home/jordan");
        let dirs = UserDirs::parse(SAMPLE_USER_DIRS, home);
        assert_eq!(dirs.desktop, Some(PathBuf::from("/home/jordan/Desktop")));
        assert_eq!(dirs.download, Some(PathBuf::from("/home/jordan/Downloads")));
        assert_eq!(dirs.videos, Some(PathBuf::from("/home/jordan/Videos")));
    }

    #[test]
    fn user_dirs_tolerates_a_directory_pointed_outside_home() {
        // xdg-user-dirs allows an absolute path with no `$HOME` prefix at
        // all (a symlinked-elsewhere Music folder, say).
        let dirs = UserDirs::parse(
            "XDG_MUSIC_DIR=\"/mnt/library/Music\"",
            Path::new("/home/jordan"),
        );
        assert_eq!(dirs.music, Some(PathBuf::from("/mnt/library/Music")));
    }

    #[test]
    fn user_dirs_ignores_unrecognized_keys_and_malformed_lines() {
        let dirs = UserDirs::parse(
            "SOME_OTHER_KEY=\"$HOME/Whatever\"\nnot even a valid line\nXDG_MUSIC_DIR=\"$HOME/Music\"",
            Path::new("/home/jordan"),
        );
        assert_eq!(dirs.music, Some(PathBuf::from("/home/jordan/Music")));
        assert_eq!(dirs.desktop, None);
    }

    #[test]
    fn user_dirs_default_is_every_field_none() {
        assert_eq!(UserDirs::default(), UserDirs::parse("", Path::new("/x")));
    }

    // ── bookmarks round-trip ─────────────────────────────────────────────

    #[test]
    fn bookmarks_parse_plain_and_labeled_entries() {
        let contents = "file:///home/jordan/Projects Projects\nfile:///home/jordan/Notes\n";
        let bookmarks = parse_bookmarks(contents);
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].label, "Projects");
        assert_eq!(
            bookmarks[0].location,
            Location::local("/home/jordan/Projects")
        );
        // No label given: falls back to the path's basename.
        assert_eq!(bookmarks[1].label, "Notes");
    }

    #[test]
    fn bookmarks_skip_unrecognized_schemes_but_keep_the_rest() {
        let contents = "smb://server/share Share\nfile:///home/jordan/Projects Projects\n";
        let bookmarks = parse_bookmarks(contents);
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].label, "Projects");
    }

    #[test]
    fn bookmarks_decode_percent_escaped_spaces() {
        let bookmarks = parse_bookmarks("file:///home/jordan/My%20Documents\n");
        assert_eq!(
            bookmarks[0].location,
            Location::local("/home/jordan/My Documents")
        );
        assert_eq!(bookmarks[0].label, "My Documents");
    }

    #[test]
    fn bookmarks_round_trip_through_format_and_parse() {
        let original = vec![
            Bookmark {
                label: "Projects".to_owned(),
                location: Location::local("/home/jordan/Projects"),
            },
            Bookmark {
                label: "My Documents".to_owned(),
                location: Location::local("/home/jordan/My Documents"),
            },
        ];
        let formatted = format_bookmarks(&original);
        let round_tripped = parse_bookmarks(&formatted);
        assert_eq!(round_tripped, original);
    }

    #[test]
    fn empty_bookmarks_file_yields_no_bookmarks() {
        assert!(parse_bookmarks("").is_empty());
    }

    // ── provider merge order ─────────────────────────────────────────────

    #[test]
    fn build_orders_home_then_dirs_then_bookmarks_then_trash_then_servers() {
        let home = Path::new("/home/jordan");
        let user_dirs = UserDirs {
            download: Some(PathBuf::from("/home/jordan/Downloads")),
            music: Some(PathBuf::from("/home/jordan/Music")),
            ..UserDirs::default()
        };
        let bookmarks = vec![Bookmark {
            label: "Projects".to_owned(),
            location: Location::local("/home/jordan/Projects"),
        }];
        let servers = vec![SavedServer {
            name: "homelab".to_owned(),
            uri: "sftp://jordan@10.0.0.10/srv".to_owned(),
        }];

        let places = build(home, &user_dirs, &bookmarks, &servers);
        let labels: Vec<&str> = places.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Home", "Downloads", "Music", "Projects", "Trash", "homelab"]
        );
        assert_eq!(places[0].kind, PlaceKind::Home);
        assert_eq!(places[1].kind, PlaceKind::Downloads);
        assert_eq!(places[2].kind, PlaceKind::Music);
        assert_eq!(places[3].kind, PlaceKind::Bookmark);
        assert_eq!(places[4].kind, PlaceKind::Trash);
        assert_eq!(places[5].kind, PlaceKind::Server);
        assert_eq!(
            places[5].location,
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@10.0.0.10".to_owned()),
                path: PathBuf::from("/srv"),
            }
        );
    }

    #[test]
    fn build_skips_user_dirs_that_never_resolved() {
        let places = build(Path::new("/home/jordan"), &UserDirs::default(), &[], &[]);
        // Home + Trash only — no XDG dirs, no bookmarks, no servers.
        let labels: Vec<&str> = places.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["Home", "Trash"]);
    }
}
