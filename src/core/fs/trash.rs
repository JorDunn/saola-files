//! Freedesktop Trash spec (v1.0) implementation — hand-rolled, local-only.
//! [`trash`] moves a path into the appropriate trash directory (the home
//! trash, or a cross-device `$topdir/.Trash[-uid]` fallback) and writes its
//! `.trashinfo` sidecar; [`restore`] is its exact inverse — see
//! [`TrashId`]'s doc comment, which is also precisely what a future undo
//! stack's "undo a trash-delete" must call. [`list`]/[`empty`] serve
//! `ui::trashview`; [`delete_permanently`] is the local-only recursive
//! "skip the trash entirely" path Shift+Delete and TRASH-incapable
//! backends fall back to.
//!
//! **Layering note (binding decision, made this stage).** CLAUDE.md's rule
//! is "the app never calls `std::fs` directly outside `src/modules/`" —
//! this file is a second, deliberate exception, the `std::fs` counterpart
//! to `core/thumbs.rs`'s documented `iced` exception. The reasoning is the
//! same shape: the freedesktop Trash spec is inherently a *local
//! filesystem* concept (topdir/mount-point detection via `st_dev`,
//! `.Trash-$uid` sticky-bit validation, `rename(2)` semantics that must
//! never cross a device) that doesn't generalize across `Backend`
//! implementations the way list/read/write do — there is no sane
//! `Backend::trash()` to add to the trait (what would "the freedesktop
//! Trash spec" even mean for an SFTP server?). Rather than force this
//! through `modules/local.rs` (which would make that one file both "the
//! `Backend` impl" and "a spec-compliance module" at once) or invent a
//! fake multi-backend abstraction for a feature only one backend can ever
//! honor, this stays a standalone `core/fs/` module that only ever gets
//! *called* for local locations — checked at the UI call site
//! (`main.rs`'s delete handling, via `Location::is_local()`) before any
//! function here is ever reached. `Caps::TRASH` is what tells the UI a
//! given backend can use it (`modules::local::LocalBackend::caps` claims it
//! as of this stage); this module itself doesn't branch on capabilities at
//! all, because it only ever runs for the one backend that claims it.
//!
//! **Known, stated gap:** `DeletionDate=` is written in UTC, not true
//! local wall-clock time — see [`format_deletion_date_now`]'s doc comment
//! for why, and why it's cosmetic rather than a correctness bug.
//!
//! **Known, stated gap:** [`list`]/[`empty`] only ever look at the *home*
//! trash (`$XDG_DATA_HOME/Trash`) — see [`list`]'s doc comment. [`trash`]
//! itself fully supports the cross-device `$topdir/.Trash[-uid]` fallback
//! (an item trashed from a different filesystem really does get trashed,
//! `gio trash --list` just won't be the tool that shows it), but
//! `ui::trashview` has no registry of "every topdir something has ever
//! been trashed from" to enumerate there too.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::core::fs::entry::EntryKind;
use crate::core::places::{decode_percent, encode_percent};
use crate::core::vfs::VfsError;

/// Trash-dir subdirectory names, per spec.
const FILES: &str = "files";
const INFO: &str = "info";
const TRASHINFO_SUFFIX: &str = "trashinfo";

/// The sticky bit (`S_ISVTX`), hardcoded rather than pulling in `libc` for
/// one POSIX-standard octal constant — the same "the value is standard,
/// not platform-specific" posture `modules::local`'s own test fixtures
/// already take with raw `0o000`/`0o755` mode literals.
const STICKY_BIT: u32 = 0o1000;

/// Identifies one trashed item well enough to restore it — [`restore`]'s
/// only input, and everything a future undo stack needs to remember about
/// one trash-delete. **Undoing a trash-delete is exactly `restore(&id)` —
/// nothing else to build, no reversed op request, no re-derived path.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashId {
    info_path: PathBuf,
    files_path: PathBuf,
    /// `Some(topdir)` for a cross-device `$topdir/.Trash[-uid]` entry
    /// (whose `.trashinfo` `Path=` is topdir-relative, per spec); `None`
    /// for the home trash (whose `Path=` is always absolute).
    topdir: Option<PathBuf>,
}

impl TrashId {
    /// Where the trashed bytes actually live right now. `ui::trashview`
    /// doesn't need this itself (it only ever restores/empties by
    /// `TrashId`), but a future "preview before restoring" affordance
    /// would, so it's exposed rather than kept fully opaque.
    pub fn files_path(&self) -> &Path {
        &self.files_path
    }
}

/// One row `ui::trashview` renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedItem {
    pub id: TrashId,
    /// Where it came from — restoring puts it back exactly here.
    pub original_path: PathBuf,
    /// The `.trashinfo`'s raw `DeletionDate=` value — already
    /// `YYYY-MM-DDThh:mm:ss`-shaped, so `ui::trashview` renders it as-is
    /// rather than re-parsing it (see the module doc comment's UTC-not-
    /// local-time gap: it's display-only either way, nothing here parses
    /// it back).
    pub deletion_date: String,
    pub kind: EntryKind,
    pub size: u64,
    pub is_symlink: bool,
}

// ── Public API ───────────────────────────────────────────────────────────

/// Moves `path` into the trash, never following it if it's itself a
/// symlink — `fs::rename` operates on the link itself, not whatever it
/// points at, so no special-casing is needed to honor "never follow
/// symlinks when trashing". Writes the `.trashinfo` sidecar *before* the
/// move (the spec-recommended ordering): if that fails, nothing has moved
/// yet; if the move fails afterward, the sidecar is rolled back — either
/// way `path` is never left "trashed but untraceable" by a partial
/// failure.
pub fn trash(path: &Path) -> Result<TrashId, VfsError> {
    let home_trash = home_trash_dir().ok_or_else(|| VfsError::Other {
        message: "no $HOME to resolve a trash directory from".to_owned(),
    })?;
    trash_into(path, &home_trash)
}

/// The exact inverse of [`trash`] — moves the item back to its original
/// location and removes the `.trashinfo` sidecar. Fails without touching
/// anything if the original parent directory no longer exists (nothing to
/// restore into — this module never recreates directories on a caller's
/// behalf, the same "ask, don't assume" posture `core::fs::ops` takes for
/// conflicts).
pub fn restore(id: &TrashId) -> Result<PathBuf, VfsError> {
    let contents = fs::read_to_string(&id.info_path).map_err(|err| path_err(&id.info_path, err))?;
    let info = TrashInfo::parse(&contents).ok_or_else(|| VfsError::Other {
        message: format!("{} isn't a valid .trashinfo file", id.info_path.display()),
    })?;

    let original = match &id.topdir {
        Some(topdir) => topdir.join(&info.path),
        None => info.path,
    };
    let parent_exists = match original.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.exists(),
        _ => true,
    };
    if !parent_exists {
        return Err(VfsError::NotFound {
            location: original
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        });
    }

    fs::rename(&id.files_path, &original).map_err(|err| path_err(&id.files_path, err))?;
    if let Err(err) = fs::remove_file(&id.info_path) {
        // The item is safely back — an orphaned `.trashinfo` is cosmetic
        // clutter, not a lost file, so this doesn't fail the restore.
        eprintln!(
            "saola-files: restored {} but couldn't remove its trash record {}: {err}",
            original.display(),
            id.info_path.display()
        );
    }
    Ok(original)
}

/// Enumerates the home trash's contents for `ui::trashview`. **Only the
/// home trash** (`$XDG_DATA_HOME/Trash`) — see the module doc comment's
/// stated gap on why cross-device `$topdir/.Trash[-uid]` entries aren't
/// enumerable here. One bad `.trashinfo`/missing `files/` entry is skipped
/// with a stderr warning rather than failing the whole listing, matching
/// `modules::local::list_blocking`'s own "one bad entry doesn't sink the
/// rest" posture.
pub fn list() -> Result<Vec<TrashedItem>, VfsError> {
    match home_trash_dir() {
        Some(home_trash) => list_from(&home_trash),
        None => Ok(Vec::new()),
    }
}

/// Empties the home trash: removes every entry under `files/`/`info/`,
/// best-effort per entry (one stubborn file — permission-denied, or still
/// open elsewhere — doesn't abort the rest). Never follows a symlinked
/// trash entry — see [`remove_path_never_following_symlinks`].
pub fn empty() -> Result<(), VfsError> {
    match home_trash_dir() {
        Some(home_trash) => empty_from(&home_trash),
        None => Ok(()),
    }
}

/// Recursively removes `path` without ever following a symlink — the
/// local-only "skip the trash" path for Shift+Delete and for any backend
/// `Caps::TRASH` doesn't cover (today, only the local backend claims it —
/// see the module doc comment).
pub fn delete_permanently(path: &Path) -> Result<(), VfsError> {
    remove_path_never_following_symlinks(path).map_err(|err| path_err(path, err))
}

// ── Testable cores (env-free — see each public wrapper above) ─────────────

/// `pub(crate)`, not private: `core::fs::undo`'s own temp-dir tests reuse
/// this exact testable core to produce a real `TrashId` to restore
/// (Stage 10) — the alternative would be touching the real `$HOME` trash
/// via the public [`trash`] wrapper, which CLAUDE.md's "never
/// `std::env::set_var` in a test" rule rules out redirecting.
pub(crate) fn trash_into(path: &Path, home_trash: &Path) -> Result<TrashId, VfsError> {
    let target = resolve_trash_target(path, home_trash)?;
    let name = path.file_name().ok_or_else(|| VfsError::Other {
        message: format!("{} has no file name to trash", path.display()),
    })?;
    let (files_path, info_path) = pick_unique_slot(&target.files_dir, &target.info_dir, name)?;

    let path_field_source: &Path = match &target.topdir {
        Some(topdir) => path.strip_prefix(topdir).unwrap_or(path),
        None => path,
    };
    let encoded_path = encode_percent(path_field_source.as_os_str().as_bytes());
    let contents = format!(
        "[Trash Info]\nPath={encoded_path}\nDeletionDate={}\n",
        format_deletion_date_now()
    );

    write_new_file(&info_path, contents.as_bytes()).map_err(|err| path_err(&info_path, err))?;

    if let Err(err) = fs::rename(path, &files_path) {
        // Roll back the sidecar we just wrote — see the doc comment on
        // `trash` for why this ordering keeps a failed trash a true no-op.
        let _ = fs::remove_file(&info_path);
        return Err(path_err(path, err));
    }

    Ok(TrashId {
        info_path,
        files_path,
        topdir: target.topdir,
    })
}

fn list_from(home_trash: &Path) -> Result<Vec<TrashedItem>, VfsError> {
    let info_dir = home_trash.join(INFO);
    let files_dir = home_trash.join(FILES);

    let entries = match fs::read_dir(&info_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(path_err(&info_dir, err)),
    };

    let mut items = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let info_path = entry.path();
        if info_path.extension().and_then(|e| e.to_str()) != Some(TRASHINFO_SUFFIX) {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&info_path) else {
            eprintln!("saola-files: couldn't read {}", info_path.display());
            continue;
        };
        let Some(info) = TrashInfo::parse(&contents) else {
            eprintln!(
                "saola-files: {} isn't a valid .trashinfo file — skipping",
                info_path.display()
            );
            continue;
        };
        let Some(stem) = info_path.file_stem() else {
            continue;
        };
        let files_path = files_dir.join(stem);
        let Ok(meta) = fs::symlink_metadata(&files_path) else {
            eprintln!(
                "saola-files: {} has no matching trashed file at {} — skipping",
                info_path.display(),
                files_path.display()
            );
            continue;
        };
        items.push(TrashedItem {
            id: TrashId {
                info_path,
                files_path,
                topdir: None,
            },
            original_path: info.path,
            deletion_date: info.deletion_date,
            kind: kind_from_metadata(&meta),
            size: meta.len(),
            is_symlink: meta.file_type().is_symlink(),
        });
    }
    Ok(items)
}

fn empty_from(home_trash: &Path) -> Result<(), VfsError> {
    for sub in [FILES, INFO] {
        let dir = home_trash.join(sub);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue; // nothing trashed yet — an absent dir is not an error
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if let Err(err) = remove_path_never_following_symlinks(&entry.path()) {
                eprintln!(
                    "saola-files: couldn't remove {} while emptying the trash: {err}",
                    entry.path().display()
                );
            }
        }
    }
    Ok(())
}

// ── Trash-directory resolution (home vs. cross-device topdir) ─────────────

/// A resolved trash directory pair, ready to receive an item.
struct TrashTarget {
    files_dir: PathBuf,
    info_dir: PathBuf,
    topdir: Option<PathBuf>,
}

/// Picks (and ensures exist) the trash directory `path` should go into,
/// per the spec's fallback chain: `home_trash` if `path` lives on the same
/// device as it, else [`resolve_topdir_trash`]'s `$topdir/.Trash[-uid]`
/// chain. A read-only mount where nothing can be created surfaces as a
/// plain [`VfsError`] naming permanent delete as the alternative — the
/// "fail with wording offering permanent delete" behavior the stage calls
/// for; actually falling back to [`delete_permanently`] is the UI's
/// decision, not this function's.
fn resolve_trash_target(path: &Path, home_trash: &Path) -> Result<TrashTarget, VfsError> {
    let item_dev = dev_of(path).map_err(|err| path_err(path, err))?;
    // The home trash directory itself may not exist yet — fall back to its
    // parent (`$XDG_DATA_HOME`, which almost always does) purely to learn
    // which device it would land on.
    let home_dev = dev_of(home_trash)
        .or_else(|_| dev_of(home_trash.parent().unwrap_or(home_trash)))
        .ok();

    if home_dev == Some(item_dev) {
        let files_dir = home_trash.join(FILES);
        let info_dir = home_trash.join(INFO);
        create_trash_subdirs(&files_dir, &info_dir)?;
        return Ok(TrashTarget {
            files_dir,
            info_dir,
            topdir: None,
        });
    }

    let topdir = find_topdir(path);
    let uid = current_uid()?;
    resolve_topdir_trash(&topdir, uid)
}

/// The cross-device half of [`resolve_trash_target`], factored out purely
/// so it's unit-testable against a real temp directory standing in for
/// `topdir` — genuinely triggering the *device-comparison* decision above
/// needs two real filesystems, which isn't something a unit test can fake,
/// but everything this function does (sticky-bit/symlink validation,
/// falling back to `.Trash-$uid`) is our own logic over an ordinary
/// directory and is exactly what the stage's "topdir fallback" test
/// exercises.
fn resolve_topdir_trash(topdir: &Path, uid: u32) -> Result<TrashTarget, VfsError> {
    let shared = topdir.join(".Trash");
    if is_valid_shared_trash(&shared) {
        let uid_dir = shared.join(uid.to_string());
        if ensure_uid_owned_dir(&uid_dir).is_ok() {
            let files_dir = uid_dir.join(FILES);
            let info_dir = uid_dir.join(INFO);
            if create_trash_subdirs(&files_dir, &info_dir).is_ok() {
                return Ok(TrashTarget {
                    files_dir,
                    info_dir,
                    topdir: Some(topdir.to_path_buf()),
                });
            }
        }
    }

    let fallback = topdir.join(format!(".Trash-{uid}"));
    ensure_uid_owned_dir(&fallback).map_err(|err| VfsError::Other {
        message: format!(
            "couldn't create a trash directory on {} ({err}) — delete permanently instead",
            topdir.display()
        ),
    })?;
    let files_dir = fallback.join(FILES);
    let info_dir = fallback.join(INFO);
    create_trash_subdirs(&files_dir, &info_dir)?;
    Ok(TrashTarget {
        files_dir,
        info_dir,
        topdir: Some(topdir.to_path_buf()),
    })
}

/// `$topdir/.Trash` must exist, be a real directory (never a symlink — the
/// spec: "must not be a symbolic link"), and have the sticky bit set, or
/// it's untrusted (any user on a shared mount could otherwise point it at
/// an arbitrary directory) and the spec says to skip straight to
/// `.Trash-$uid`.
fn is_valid_shared_trash(shared: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(shared) else {
        return false;
    };
    meta.is_dir() && (meta.mode() & STICKY_BIT != 0)
}

/// Creates `dir` (mode `0700`) if it doesn't exist; if it does, only
/// accepts it when it's a real directory, never a symlink — the same
/// "don't trust a symlinked trash directory" posture [`is_valid_shared_trash`]
/// takes for the shared `.Trash` case.
fn ensure_uid_owned_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and isn't a directory", dir.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(dir)
        }
        Err(err) => Err(err),
    }
}

fn create_trash_subdirs(files_dir: &Path, info_dir: &Path) -> Result<(), VfsError> {
    for dir in [files_dir, info_dir] {
        fs::create_dir_all(dir).map_err(|err| path_err(dir, err))?;
    }
    Ok(())
}

/// A unique `(files/name, info/name.trashinfo)` pair for `base`: `base`
/// itself if free, else `base.2`, `base.3`, … — the exact numbering
/// doesn't matter for spec compliance (restore reads the *original* name
/// back out of `Path=`, never this on-disk name), only uniqueness does.
/// Bounded at 1000 attempts, the same "vanishingly unlikely, still has to
/// return something" ceiling `core::fs::ops::unique_rename_dest`/
/// `ui::dirview::rename::unique_name` already use.
fn pick_unique_slot(
    files_dir: &Path,
    info_dir: &Path,
    base: &OsStr,
) -> Result<(PathBuf, PathBuf), VfsError> {
    for n in 0..1000u32 {
        let mut candidate = base.to_os_string();
        if n > 0 {
            candidate.push(format!(".{n}"));
        }
        let files_path = files_dir.join(&candidate);
        let mut info_name = candidate;
        info_name.push(".");
        info_name.push(TRASHINFO_SUFFIX);
        let info_path = info_dir.join(info_name);
        if !files_path.exists() && !info_path.exists() {
            return Ok((files_path, info_path));
        }
    }
    Err(VfsError::Other {
        message: format!(
            "couldn't find a free trash slot for {}",
            base.to_string_lossy()
        ),
    })
}

fn write_new_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)
}

/// Removes `path`: recursively, if it's a real directory (never a symlink
/// — `symlink_metadata`'s `is_dir()` is `false` for a symlink even when it
/// points at a directory, matching `modules::local::entry_from_metadata`'s
/// own "never resolved" posture); as a single `remove_file` (which
/// unlinks, not follows) otherwise — covering plain files and symlinks
/// alike in one branch.
fn remove_path_never_following_symlinks(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_path_never_following_symlinks(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

fn kind_from_metadata(meta: &fs::Metadata) -> EntryKind {
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        EntryKind::Other
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

// ── Device / mount-point plumbing ──────────────────────────────────────────

fn dev_of(path: &Path) -> io::Result<u64> {
    fs::symlink_metadata(path).map(|m| m.dev())
}

/// Walks upward from `path`'s parent directory to find its mount point:
/// the highest ancestor that still shares `path`'s device id. Falls back
/// to `/` if `path` (or an ancestor) can't be stat'd — a conservative
/// choice that only affects *which* cross-device trash directory gets
/// tried, never whether trashing is attempted at all.
fn find_topdir(path: &Path) -> PathBuf {
    let Ok(target_dev) = dev_of(path) else {
        return PathBuf::from("/");
    };
    let mut topdir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut current = topdir.parent().map(Path::to_path_buf);
    while let Some(dir) = current {
        match dev_of(&dir) {
            Ok(dev) if dev == target_dev => {
                current = dir.parent().map(Path::to_path_buf);
                topdir = dir;
            }
            _ => break,
        }
    }
    topdir
}

/// The running process's real uid, read from `/proc/self/status`'s `Uid:`
/// line. `std` has no safe `getuid()` — the `libc`/`nix` crates do, but
/// this app is Linux-only already (see this crate's top-level docs: niri
/// is Wayland-only), so reading the standard Linux `/proc` file is the
/// same "lean on a guaranteed-present platform interface instead of an FFI
/// dependency" posture `modules::local`'s own tests take shelling out to
/// `id -u` for the identical reason, just without spawning a process for
/// something this cheap to read directly.
fn current_uid() -> Result<u32, VfsError> {
    let status = fs::read_to_string("/proc/self/status").map_err(|err| VfsError::Other {
        message: format!("couldn't read /proc/self/status: {err}"),
    })?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| VfsError::Other {
            message: "couldn't parse the current user id from /proc/self/status".to_owned(),
        })
}

// ── `$XDG_DATA_HOME/Trash` resolution ──────────────────────────────────────

fn home_trash_dir() -> Option<PathBuf> {
    xdg_data_home().map(|data_home| data_home.join("Trash"))
}

/// Resolved at this one thin wrapper (CLAUDE.md: never `std::env::set_var`
/// in a test) — the testable core is [`data_home_from`].
fn xdg_data_home() -> Option<PathBuf> {
    data_home_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// The testable core of [`xdg_data_home`] — same shape as
/// `core::places::xdg_config_home`/`config::config_dir_from`: every env
/// var arrives as a plain argument, and a var set to the empty string
/// counts as unset.
fn data_home_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = xdg_data_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share"))
}

// ── `.trashinfo` format ─────────────────────────────────────────────────────

struct TrashInfo {
    path: PathBuf,
    deletion_date: String,
}

impl TrashInfo {
    /// Hand-parses a `.trashinfo` file's two keys line by line (not a
    /// general INI parser — the spec's own format is exactly this simple:
    /// a `[Trash Info]` header, then `Key=value` lines). `None` when
    /// `Path=` never showed up — a `.trashinfo` without one isn't
    /// recoverable, so the caller treats it the way it treats any other
    /// unreadable trash record (skip, or surface as an error, per call
    /// site).
    fn parse(contents: &str) -> Option<Self> {
        let mut path = None;
        let mut deletion_date = None;
        for line in contents.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("Path=") {
                let bytes = decode_percent(value);
                path = Some(PathBuf::from(OsString::from_vec(bytes)));
            } else if let Some(value) = line.strip_prefix("DeletionDate=") {
                deletion_date = Some(value.to_owned());
            }
        }
        Some(TrashInfo {
            path: path?,
            deletion_date: deletion_date.unwrap_or_default(),
        })
    }
}

/// Formats "now" as `YYYY-MM-DDThh:mm:ss`, the trash spec's `DeletionDate=`
/// shape. **Written in UTC, not true local wall-clock time** — `std` has
/// no timezone-database access without either an `unsafe`
/// `libc::localtime_r` FFI call or a `chrono`-class dependency, and
/// neither is justified for one informational timestamp field nothing in
/// this module ever parses back (`list`/`ui::trashview` just display the
/// raw string). A future stage could add proper local-time support if
/// precise wall-clock display becomes an actual requirement; until then
/// this is a stated, cosmetic gap, not a silent one.
fn format_deletion_date_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

/// Howard Hinnant's `civil_from_days` — the exact algorithm
/// `ui::dirview::list::civil_from_days` already uses for the same reason
/// (a dependency-free days-since-epoch → calendar-date conversion),
/// re-implemented here (not shared) because `core/` must not import `ui/`
/// (CLAUDE.md's layering rule) and this is 8 lines of well-known integer
/// arithmetic, not business logic worth threading a shared module through
/// the layer boundary for. <http://howardhinnant.github.io/date_algorithms.html>
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Turns a `std::io::Error` into a human-worded [`VfsError`] keyed by
/// `path` — the same mapping `modules::local::io_error` does for a
/// `Location`, duplicated here in miniature because this module works in
/// `&Path`, not `Location` (trash is local-only — see the module doc
/// comment on why).
fn path_err(path: &Path, err: io::Error) -> VfsError {
    let location = path.display().to_string();
    match err.kind() {
        io::ErrorKind::NotFound => VfsError::NotFound { location },
        io::ErrorKind::PermissionDenied => VfsError::PermissionDenied { location },
        io::ErrorKind::AlreadyExists => VfsError::AlreadyExists { location },
        io::ErrorKind::NotADirectory => VfsError::NotADirectory { location },
        io::ErrorKind::IsADirectory => VfsError::IsADirectory { location },
        _ => VfsError::Other {
            message: format!("{location}: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    // ── tiny temp-dir helpers, matching `modules::local`'s/`core::fs::ops`'s
    // own copies (each test module keeps its own — see those files' docs) ──

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-trash-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    // ── data_home_from ──────────────────────────────────────────────────

    #[test]
    fn xdg_data_home_wins_when_set() {
        assert_eq!(
            data_home_from(os("/data"), os("/home/j")),
            Some(PathBuf::from("/data"))
        );
    }

    #[test]
    fn falls_back_to_home_local_share() {
        assert_eq!(
            data_home_from(None, os("/home/j")),
            Some(PathBuf::from("/home/j/.local/share"))
        );
    }

    #[test]
    fn empty_env_vars_count_as_unset() {
        assert_eq!(
            data_home_from(os(""), os("/home/j")),
            Some(PathBuf::from("/home/j/.local/share"))
        );
    }

    #[test]
    fn no_home_means_no_data_home() {
        assert_eq!(data_home_from(None, None), None);
    }

    // ── civil_from_days / format_deletion_date_now ──────────────────────

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn format_deletion_date_now_matches_the_spec_shape() {
        let formatted = format_deletion_date_now();
        assert_eq!(formatted.len(), 19, "got {formatted:?}");
        assert_eq!(formatted.as_bytes()[4], b'-');
        assert_eq!(formatted.as_bytes()[7], b'-');
        assert_eq!(formatted.as_bytes()[10], b'T');
        assert_eq!(formatted.as_bytes()[13], b':');
        assert_eq!(formatted.as_bytes()[16], b':');
    }

    // ── TrashInfo parsing ────────────────────────────────────────────────

    #[test]
    fn trash_info_parses_path_and_deletion_date() {
        let contents =
            "[Trash Info]\nPath=/home/jordan/My%20File.txt\nDeletionDate=2026-08-09T12:30:00\n";
        let info = TrashInfo::parse(contents).unwrap();
        assert_eq!(info.path, PathBuf::from("/home/jordan/My File.txt"));
        assert_eq!(info.deletion_date, "2026-08-09T12:30:00");
    }

    #[test]
    fn trash_info_without_a_path_is_unparseable() {
        assert!(TrashInfo::parse("[Trash Info]\nDeletionDate=2026-08-09T12:30:00\n").is_none());
    }

    #[test]
    fn trash_info_tolerates_extra_or_reordered_lines() {
        let contents = "[Trash Info]\nDeletionDate=2026-08-09T12:30:00\nPath=/a/b\n";
        let info = TrashInfo::parse(contents).unwrap();
        assert_eq!(info.path, PathBuf::from("/a/b"));
    }

    // ── pick_unique_slot ─────────────────────────────────────────────────

    #[test]
    fn pick_unique_slot_uses_the_base_name_when_free() {
        let dir = tempdir();
        let files_dir = dir.join(FILES);
        let info_dir = dir.join(INFO);
        std::fs::create_dir_all(&files_dir).unwrap();
        std::fs::create_dir_all(&info_dir).unwrap();

        let (files_path, info_path) =
            pick_unique_slot(&files_dir, &info_dir, OsStr::new("report.txt")).unwrap();
        assert_eq!(files_path, files_dir.join("report.txt"));
        assert_eq!(info_path, info_dir.join("report.txt.trashinfo"));

        cleanup(dir);
    }

    #[test]
    fn pick_unique_slot_numbers_past_a_collision() {
        let dir = tempdir();
        let files_dir = dir.join(FILES);
        let info_dir = dir.join(INFO);
        std::fs::create_dir_all(&files_dir).unwrap();
        std::fs::create_dir_all(&info_dir).unwrap();
        std::fs::write(files_dir.join("a.txt"), b"x").unwrap();
        std::fs::write(info_dir.join("a.txt.trashinfo"), b"x").unwrap();

        let (files_path, info_path) =
            pick_unique_slot(&files_dir, &info_dir, OsStr::new("a.txt")).unwrap();
        assert_eq!(files_path, files_dir.join("a.txt.1"));
        assert_eq!(info_path, info_dir.join("a.txt.1.trashinfo"));

        cleanup(dir);
    }

    // ── is_valid_shared_trash / resolve_topdir_trash ────────────────────

    #[test]
    fn is_valid_shared_trash_requires_a_real_sticky_directory() {
        let dir = tempdir();
        let shared = dir.join(".Trash");

        assert!(!is_valid_shared_trash(&shared), "missing dir");

        std::fs::create_dir(&shared).unwrap();
        assert!(!is_valid_shared_trash(&shared), "no sticky bit yet");

        let mut perms = std::fs::metadata(&shared).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o1777);
        std::fs::set_permissions(&shared, perms).unwrap();
        assert!(is_valid_shared_trash(&shared), "sticky bit now set");

        cleanup(dir);
    }

    #[test]
    fn is_valid_shared_trash_rejects_a_symlink() {
        let dir = tempdir();
        let real = dir.join("real-trash");
        std::fs::create_dir(&real).unwrap();
        let mut perms = std::fs::metadata(&real).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o1777);
        std::fs::set_permissions(&real, perms).unwrap();

        let shared = dir.join(".Trash");
        std::os::unix::fs::symlink(&real, &shared).unwrap();
        assert!(!is_valid_shared_trash(&shared));

        cleanup(dir);
    }

    #[test]
    fn resolve_topdir_trash_uses_the_shared_uid_subdir_when_valid() {
        let topdir = tempdir();
        let shared = topdir.join(".Trash");
        std::fs::create_dir(&shared).unwrap();
        let mut perms = std::fs::metadata(&shared).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o1777);
        std::fs::set_permissions(&shared, perms).unwrap();

        let target = resolve_topdir_trash(&topdir, 1000).unwrap();
        assert_eq!(target.files_dir, shared.join("1000").join(FILES));
        assert_eq!(target.info_dir, shared.join("1000").join(INFO));
        assert_eq!(target.topdir.as_deref(), Some(topdir.as_path()));
        assert!(target.files_dir.is_dir());

        cleanup(topdir);
    }

    #[test]
    fn resolve_topdir_trash_falls_back_when_shared_trash_is_missing() {
        let topdir = tempdir();
        // No `.Trash` at all.
        let target = resolve_topdir_trash(&topdir, 1000).unwrap();
        assert_eq!(target.files_dir, topdir.join(".Trash-1000").join(FILES));
        assert!(target.files_dir.is_dir());
        cleanup(topdir);
    }

    #[test]
    fn resolve_topdir_trash_falls_back_when_shared_trash_has_no_sticky_bit() {
        let topdir = tempdir();
        std::fs::create_dir(topdir.join(".Trash")).unwrap();
        // Default temp-dir permissions have no sticky bit.
        let target = resolve_topdir_trash(&topdir, 1000).unwrap();
        assert_eq!(target.files_dir, topdir.join(".Trash-1000").join(FILES));
        cleanup(topdir);
    }

    // ── trash_into / restore round trip ─────────────────────────────────

    #[test]
    fn trash_into_and_restore_round_trip_an_ordinary_file() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let path = source_dir.join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();

        let id = trash_into(&path, &home_trash).unwrap();
        assert!(!path.exists(), "the original name is gone from its folder");
        assert!(id.files_path().exists(), "the bytes landed in files/");
        assert_eq!(std::fs::read(id.files_path()).unwrap(), b"hello");

        let restored = restore(&id).unwrap();
        assert_eq!(restored, path);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(!id.files_path().exists(), "removed from files/ on restore");
        assert!(
            !id.info_path.exists(),
            "the .trashinfo sidecar is cleaned up too"
        );

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn trash_into_and_restore_round_trip_a_non_utf8_name() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let raw_name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        let path = source_dir.join(raw_name);
        std::fs::write(&path, b"x").unwrap();

        let id = trash_into(&path, &home_trash).unwrap();
        let restored = restore(&id).unwrap();
        assert_eq!(restored, path);
        assert_eq!(
            restored.file_name().unwrap().as_bytes(),
            raw_name.as_bytes()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"x");

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn trash_into_writes_a_spec_shaped_trashinfo_file() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let path = source_dir.join("doc.odt");
        std::fs::write(&path, b"x").unwrap();

        let id = trash_into(&path, &home_trash).unwrap();
        let contents = std::fs::read_to_string(&id.info_path).unwrap();
        assert!(contents.starts_with("[Trash Info]\n"));
        assert!(contents.contains(&format!(
            "Path={}\n",
            encode_percent(path.as_os_str().as_bytes())
        )));
        assert!(contents.contains("DeletionDate="));

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn trash_into_never_follows_a_symlinked_directory() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let real_target = source_dir.join("real-dir");
        std::fs::create_dir(&real_target).unwrap();
        std::fs::write(real_target.join("inside.txt"), b"x").unwrap();
        let link = source_dir.join("link-to-dir");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let id = trash_into(&link, &home_trash).unwrap();

        // The symlink itself moved into the trash, not the directory it
        // pointed at — the real directory is untouched at its original
        // location, and the trashed entry is still a symlink.
        assert!(real_target.join("inside.txt").exists());
        let meta = std::fs::symlink_metadata(id.files_path()).unwrap();
        assert!(meta.file_type().is_symlink());

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn restore_fails_without_moving_anything_when_the_original_parent_is_gone() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let path = source_dir.join("orphan.txt");
        std::fs::write(&path, b"x").unwrap();

        let id = trash_into(&path, &home_trash).unwrap();
        // Remove the folder the file used to live in.
        std::fs::remove_dir_all(&source_dir).unwrap();

        let result = restore(&id);
        assert!(result.is_err());
        assert!(id.files_path().exists(), "the trashed copy is untouched");

        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    // ── list_from / empty_from ──────────────────────────────────────────

    #[test]
    fn list_from_reports_every_trashed_item() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let a = source_dir.join("a.txt");
        let b = source_dir.join("b.txt");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bb").unwrap();
        trash_into(&a, &home_trash).unwrap();
        trash_into(&b, &home_trash).unwrap();

        let mut items = list_from(&home_trash).unwrap();
        items.sort_by(|x, y| x.original_path.cmp(&y.original_path));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].original_path, a);
        assert_eq!(items[0].size, 3);
        assert_eq!(items[0].kind, EntryKind::File);
        assert_eq!(items[1].original_path, b);

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn list_from_an_empty_or_absent_trash_is_an_empty_list() {
        let home_trash = tempdir().join("Trash");
        assert!(list_from(&home_trash).unwrap().is_empty());
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[test]
    fn empty_from_removes_files_and_directory_trees_alike() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let file = source_dir.join("solo.txt");
        std::fs::write(&file, b"x").unwrap();
        let dir = source_dir.join("a-folder");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("nested.txt"), b"x").unwrap();

        trash_into(&file, &home_trash).unwrap();
        trash_into(&dir, &home_trash).unwrap();
        assert_eq!(list_from(&home_trash).unwrap().len(), 2);

        empty_from(&home_trash).unwrap();
        assert!(list_from(&home_trash).unwrap().is_empty());
        assert!(
            std::fs::read_dir(home_trash.join(FILES))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            std::fs::read_dir(home_trash.join(INFO))
                .unwrap()
                .next()
                .is_none()
        );

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    // ── delete_permanently ───────────────────────────────────────────────

    #[test]
    fn delete_permanently_removes_a_plain_file() {
        let dir = tempdir();
        let file = dir.join("gone.txt");
        std::fs::write(&file, b"x").unwrap();
        delete_permanently(&file).unwrap();
        assert!(!file.exists());
        cleanup(dir);
    }

    #[test]
    fn delete_permanently_removes_a_directory_tree() {
        let dir = tempdir();
        let sub = dir.join("tree");
        std::fs::create_dir_all(sub.join("nested")).unwrap();
        std::fs::write(sub.join("nested/leaf.txt"), b"x").unwrap();
        delete_permanently(&sub).unwrap();
        assert!(!sub.exists());
        cleanup(dir);
    }

    #[test]
    fn delete_permanently_never_follows_a_symlinked_directory() {
        let dir = tempdir();
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"x").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        delete_permanently(&link).unwrap();

        assert!(!link.exists(), "the symlink itself is gone");
        assert!(
            real.join("keep.txt").exists(),
            "the real target is untouched"
        );

        cleanup(dir);
    }
}
