//! Opening a file that has no local path: where its downloaded copy lives.
//!
//! A backend without `Caps::LOCAL_PATH` (SFTP today; WebDAV, SMB, MTP and
//! archives later) cannot hand an application a path. AGENTS.md's
//! capability-honest rule says "open" then means: download a copy, open the
//! copy, and say plainly that it is a read-only copy. This module is the
//! pure/`std::fs` half of that: the decision ([`open_plan`]) and the place
//! the copy lives (the *slot*). The download itself is the ops engine's
//! ordinary `OpKind::Copy`, driven by `app::remote_open`.
//!
//! # Why this module uses `std::fs` directly
//!
//! "All file access goes through a `Backend`" is about the user's files.
//! The slot is the app's own private storage under its cache directory, the
//! same sanctioned exception as `core::state`, `core::thumbs` and
//! `config.rs`. The downloaded bytes themselves still arrive through the
//! source `Backend` and are written through the local `Backend` by the ops
//! engine; only the app-private directory bookkeeping (create, chmod, sweep)
//! is `std::fs` here.
//!
//! # Where copies live
//!
//! `$XDG_CACHE_HOME/saola-files/open/<slot>/<original name>` (fallback
//! `~/.cache/saola-files/open/…`), resolved like `core::thumbs` resolves its
//! cache root ([`open_root_from`] takes the environment as arguments; one
//! thin wrapper reads it). Deliberately **not** `$XDG_RUNTIME_DIR`: that is
//! usually a small RAM-backed tmpfs shared with the session's sockets, and a
//! large open would eat memory and starve the session. There is no shared
//! temp-dir fallback either: with no resolvable cache directory the open is
//! refused with a worded failure ([`SlotError::NoCacheDir`]).
//!
//! Both app directories (`saola-files` and `open`) are mode 0700 and, when
//! they already exist, must be real directories (checked with `lstat`, so a
//! symlink is a failure), owned by this user, with no group/other bits
//! ([`is_private_dir`]), or [`create_slot`] refuses and [`sweep_stale`] does
//! nothing. The cache directory above them is the user's own and is not
//! policed.
//!
//! A *slot* is a fresh directory per open action (`<pid>-<nanos>-<counter>`,
//! created exclusively), so two files with the same name never collide and
//! the file inside keeps its original name: the application shows it and
//! picks its handler from the real extension.
//!
//! # Read-only, honestly
//!
//! [`seal`] makes the file mode 0444 *and* the slot directory 0500. The
//! directory matters: many editors save by writing a new file and renaming
//! it over the old one, which needs write permission on the directory, not
//! the file. With both set, an application's attempt to save fails loudly
//! instead of looking like it worked. [`seal`] also checks the copy's length
//! against the size the server reported, so a truncated download is never
//! sealed and opened as if complete.
//!
//! # Cleanup
//!
//! Copies are never removed while the app runs, because an application may
//! still have one open. They go when: the open fails or is cancelled
//! ([`remove_slot`], immediately); or a later start finds a slot older than
//! [`STALE_AFTER`] ([`sweep_stale`]). Because the cache survives logout, a
//! crash (or a kill) leaves its copies until the next start sweeps them.
//! The sweep removes only direct children of the verified root whose names
//! have the slot shape, that are real directories (never symlinks) owned by
//! this user; on Linux, deleting a file an application already holds open
//! does not break that application, and the age bound avoids pulling files
//! out from under one that reopens by path.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::vfs::{Caps, Location, VfsError};
use crate::tr;

/// Slots older than this are removed by [`sweep_stale`] at the next start.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// What opening one location needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPlan {
    /// The backend has a real local path: hand it to the application as is.
    Direct(PathBuf),
    /// No local path: download `source` first, as a file called `name`.
    DownloadFirst { source: Location, name: OsString },
    /// Not safe to turn into a local file name; refuse.
    Refused(Refusal),
}

/// Why [`open_plan`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The name is empty, `.`/`..`, or contains `/` or NUL, so it cannot be
    /// used as a single local file name.
    UnsafeName,
}

/// Decides how to open `location` on a backend with `caps`. Pure: the
/// decision comes from the capability, never from the scheme.
pub fn open_plan(caps: Caps, location: &Location) -> OpenPlan {
    if caps.contains(Caps::LOCAL_PATH) {
        return OpenPlan::Direct(location.path.clone());
    }
    match location.path.file_name() {
        Some(name) if is_safe_name(name) => OpenPlan::DownloadFirst {
            source: location.clone(),
            name: name.to_owned(),
        },
        _ => OpenPlan::Refused(Refusal::UnsafeName),
    }
}

/// Whether `name` can be one local path component. A remote server controls
/// this string; it must never be able to name `..` or a nested path.
pub fn is_safe_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes != b"."
        && bytes != b".."
        && !bytes.iter().any(|&b| b == b'/' || b == 0)
}

/// Why a slot could not be made, sealed or trusted. Typed so the notice is
/// worded through the catalogue ([`SlotError::into_vfs`]), never as a raw
/// OS error text standing alone.
#[derive(Debug)]
pub enum SlotError {
    /// Neither `$XDG_CACHE_HOME` nor `$HOME` gave a cache directory.
    NoCacheDir,
    /// The app's directory exists but is not a private directory of ours.
    NotPrivate,
    /// Could not find an unused slot name.
    NoFreeName,
    /// The downloaded file's length is not the size the server reported.
    SizeMismatch,
    /// A file operation failed.
    Io(io::Error),
}

impl From<io::Error> for SlotError {
    fn from(err: io::Error) -> Self {
        SlotError::Io(err)
    }
}

impl SlotError {
    /// The error as a worded [`VfsError`] for the failure notice.
    pub fn into_vfs(self) -> VfsError {
        let message = match self {
            SlotError::NoCacheDir => tr!("open-no-cache"),
            SlotError::NotPrivate => tr!("open-slot-not-private"),
            SlotError::NoFreeName => tr!("open-slot-no-name"),
            SlotError::SizeMismatch => tr!("open-truncated"),
            SlotError::Io(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                return VfsError::PermissionDenied {
                    location: tr!("open-slot-location"),
                };
            }
            // A genuine OS error: its text ends in "(os error N)", which
            // `core::failure` reads to word "out of space" and friends, and
            // otherwise shows inside a catalogue sentence, not on its own.
            SlotError::Io(err) => err.to_string(),
        };
        VfsError::Other { message }
    }
}

/// The directory that holds all slots, from already-resolved environment
/// values (tests pass their own; nothing here reads the environment). An
/// empty value counts as unset, the same XDG rule `core::thumbs` follows.
pub fn open_root_from(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // An empty or relative `$XDG_CACHE_HOME` counts as unset (XDG says
    // relative paths are invalid; `core::state` does the same).
    if let Some(xdg) = xdg_cache_home
        && Path::new(&xdg).is_absolute()
    {
        return Some(PathBuf::from(xdg).join("saola-files").join("open"));
    }
    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".cache/saola-files/open"))
}

/// [`open_root_from`] from the real environment: the one thin wrapper.
pub fn open_root_from_env() -> Option<PathBuf> {
    open_root_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// The uid this process runs as, without `libc`/`unsafe`: the owner of
/// `/proc/self`. `u32::MAX` if even that is unreadable, which then fails
/// the ownership check below rather than matching anything.
///
/// Fails closed: a process that is not "dumpable" (after a setuid exec, or
/// `PR_SET_DUMPABLE 0`) sees a root-owned `/proc/self`, so this returns 0 and
/// no directory of ours passes [`is_private_dir`]; the open is refused
/// rather than trusting a directory it cannot prove is ours.
pub fn current_uid() -> u32 {
    fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .unwrap_or(u32::MAX)
}

/// Whether metadata says "a real directory (not a symlink; the caller must
/// have used `lstat`), owned by `uid`, with no group/other access". Takes
/// the uid as an argument so the foreign-owner case is testable.
pub fn is_private_dir(meta: &fs::Metadata, uid: u32) -> bool {
    meta.is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o077 == 0
}

/// `lstat` + [`is_private_dir`]; `Ok(())` only if `dir` exists and passes.
fn verify_private_dir(dir: &Path, uid: u32) -> Result<(), SlotError> {
    let meta = fs::symlink_metadata(dir)?;
    if is_private_dir(&meta, uid) {
        Ok(())
    } else {
        Err(SlotError::NotPrivate)
    }
}

/// Verifies, without creating anything, that `root` and its parent (the
/// app's own directory) are private directories of ours.
fn verify_root(root: &Path, uid: u32) -> Result<(), SlotError> {
    if let Some(ours) = root.parent() {
        verify_private_dir(ours, uid)?;
    }
    verify_private_dir(root, uid)
}

static SLOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates (and returns) a new, empty, private slot directory under `root`,
/// creating `root` and the app directory above it privately first.
pub fn create_slot(root: &Path) -> Result<PathBuf, SlotError> {
    let uid = current_uid();
    // The app's own directory (`…/saola-files`) and `root` itself are both
    // ours to police; the cache directory above them is the user's own.
    if let Some(ours) = root.parent() {
        ensure_private_dir(ours, uid)?;
    }
    ensure_private_dir(root, uid)?;
    for _ in 0..8 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let counter = SLOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let slot = root.join(format!("{}-{nanos}-{counter}", std::process::id()));
        // `create` (not `create_dir_all`) fails with `AlreadyExists` if the
        // name is taken, so a slot is never shared or pre-planted.
        match fs::DirBuilder::new().mode(0o700).create(&slot) {
            Ok(()) => return Ok(slot),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(SlotError::NoFreeName)
}

/// Makes `dir` (and missing ancestors below an existing one) a private
/// directory owned by `uid`; checks an existing one instead of trusting it.
fn ensure_private_dir(dir: &Path, uid: u32) -> Result<(), SlotError> {
    // Parents first, so only the components *we* make are created
    // privately; a parent that already exists (the cache directory) is left
    // alone.
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && fs::symlink_metadata(parent).is_err()
    {
        ensure_private_dir(parent, uid)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    // It exists. `symlink_metadata` does not follow a symlink, so a planted
    // link is seen as a link and refused.
    verify_private_dir(dir, uid)
}

/// Where the copy of a file called `name` goes inside `slot`.
pub fn copy_path(slot: &Path, name: &OsStr) -> PathBuf {
    slot.join(name)
}

/// What [`seal`] learned about one finished copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedCopy {
    pub path: PathBuf,
    /// The first [`SNIFF_HEAD`] bytes, for content-sniffing an untyped name
    /// (`None` if unreadable or empty). The remote listing never reads
    /// files; now the bytes are local, so they are free.
    pub head: Option<Vec<u8>>,
}

/// How many leading bytes [`seal`] reads per file (the same budget as
/// `core::mime::SNIFF_BYTES`).
pub const SNIFF_HEAD: usize = crate::core::mime::SNIFF_BYTES;

/// Checks each downloaded `names[i]` in `slot` is `expected[i]` bytes long
/// (when the server reported a size), then makes it read-only (files 0444,
/// slot 0500) and reads its head. Fails if a file is missing, the wrong
/// length (a truncated download), or cannot be locked, so an incomplete or
/// unprotected copy is never opened and called read-only.
pub fn seal(
    slot: &Path,
    names: &[OsString],
    expected: &[Option<u64>],
) -> Result<Vec<SealedCopy>, SlotError> {
    let mut sealed = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let path = copy_path(slot, name);
        if let Some(Some(size)) = expected.get(index)
            && fs::metadata(&path)?.len() != *size
        {
            return Err(SlotError::SizeMismatch);
        }
        let mut head = vec![0u8; SNIFF_HEAD];
        let read = fs::File::open(&path).and_then(|mut file| file.read(&mut head))?;
        head.truncate(read);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        sealed.push(SealedCopy {
            path,
            head: (!head.is_empty()).then_some(head),
        });
    }
    fs::set_permissions(slot, fs::Permissions::from_mode(0o500))?;
    Ok(sealed)
}

/// Deletes `slot` and everything in it. Best-effort (a leftover is retried
/// by [`sweep_stale`]). Restores write permission on the slot first, since
/// [`seal`] removed it. Only call this with a path [`create_slot`] returned
/// or [`sweep_stale`] verified.
pub fn remove_slot(slot: &Path) {
    let _ = fs::set_permissions(slot, fs::Permissions::from_mode(0o700));
    let _ = fs::remove_dir_all(slot);
}

/// Whether `name` has the shape [`create_slot`] gives: `<pid>-<nanos>-<n>`,
/// three non-empty runs of ASCII digits. Anything else under the root is
/// not ours to delete.
pub fn is_slot_name(name: &OsStr) -> bool {
    let Some(text) = name.to_str() else {
        return false;
    };
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Removes slots under `root` last modified more than `max_age` before
/// `now`. Returns how many were removed.
///
/// Runs the same verification as [`create_slot`] first: if `root` or the
/// app directory above it is not a real, private directory owned by `uid`
/// (a symlink, someone else's, loosely permissioned), it does **nothing**.
/// It then touches only direct children whose names have the slot shape and
/// that `lstat` as real directories owned by `uid`; it never follows a
/// symlink and never chmods or deletes through one.
pub fn sweep_stale(root: &Path, now: SystemTime, max_age: Duration, uid: u32) -> usize {
    if verify_root(root, uid).is_err() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !is_slot_name(&entry.file_name()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        // `symlink_metadata` reports a symlink as a symlink, so `is_dir()`
        // is false for one and it is skipped, never chmod-ed or followed.
        if meta.is_dir() && meta.uid() == uid && old {
            remove_slot(&entry.path());
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    /// A fresh scratch directory under the system temp dir (tests never set
    /// env; this only reads it through `std::env::temp_dir`).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "saola-open-copy-{tag}-{}-{}",
            std::process::id(),
            SLOT_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn remote(path: &str) -> Location {
        Location {
            scheme: "sftp".to_owned(),
            authority: Some("host".to_owned()),
            path: PathBuf::from(path),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    /// `<base>/saola-files/open` created privately, the way a real root is.
    fn private_root(base: &Path) -> PathBuf {
        let root = open_root_from(Some(base.as_os_str().to_owned()), None).expect("root");
        create_slot(&root)
            .map(|slot| remove_slot(&slot))
            .expect("slot");
        root
    }

    fn backdated_now() -> SystemTime {
        SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60)
    }

    // --- open_plan ---

    #[test]
    fn a_backend_with_a_local_path_opens_directly() {
        let location = Location::local("/home/me/a.txt");
        let caps = Caps::LOCAL_PATH | Caps::WATCH;
        assert_eq!(
            open_plan(caps, &location),
            OpenPlan::Direct(PathBuf::from("/home/me/a.txt"))
        );
    }

    #[test]
    fn a_backend_without_a_local_path_downloads_first() {
        let location = remote("/srv/report.pdf");
        assert_eq!(
            open_plan(Caps::empty(), &location),
            OpenPlan::DownloadFirst {
                source: location.clone(),
                name: OsString::from("report.pdf"),
            }
        );
    }

    #[test]
    fn the_plan_follows_the_capability_not_the_scheme() {
        assert!(matches!(
            open_plan(Caps::empty(), &Location::local("/a")),
            OpenPlan::DownloadFirst { .. }
        ));
        assert!(matches!(
            open_plan(Caps::LOCAL_PATH, &remote("/a")),
            OpenPlan::Direct(_)
        ));
    }

    #[test]
    fn the_name_is_kept_byte_for_byte() {
        let raw = OsString::from_vec(b"caf\xe9 \xff.txt".to_vec());
        let mut path = PathBuf::from("/srv");
        path.push(&raw);
        let location = Location {
            path,
            ..remote("/")
        };
        match open_plan(Caps::empty(), &location) {
            OpenPlan::DownloadFirst { name, .. } => assert_eq!(name, raw),
            other => panic!("expected a download, got {other:?}"),
        }
        let slot = PathBuf::from("/slot");
        assert_eq!(copy_path(&slot, &raw).file_name(), Some(raw.as_os_str()));
    }

    #[test]
    fn unsafe_names_are_refused() {
        for bad in [&b""[..], b".", b"..", b"a/b", b"a\0b"] {
            assert!(!is_safe_name(OsStr::from_bytes(bad)), "{bad:?}");
        }
        assert!(is_safe_name(OsStr::new("...")));
        assert!(is_safe_name(OsStr::new(".bashrc")));
        assert_eq!(
            open_plan(Caps::empty(), &remote("/")),
            OpenPlan::Refused(Refusal::UnsafeName)
        );
    }

    // --- where copies live ---

    #[test]
    fn the_root_is_under_the_cache_dir_and_never_the_runtime_or_temp_dir() {
        assert_eq!(
            open_root_from(Some("/c".into()), Some("/home/me".into())),
            Some(PathBuf::from("/c/saola-files/open"))
        );
        assert_eq!(
            open_root_from(None, Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.cache/saola-files/open"))
        );
        // Empty counts as unset; with nothing at all there is no root and
        // the open is refused rather than falling back to a shared dir.
        assert_eq!(
            open_root_from(Some("".into()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.cache/saola-files/open"))
        );
        assert_eq!(open_root_from(Some("".into()), Some("".into())), None);
        // A relative XDG_CACHE_HOME is ignored just like an empty one.
        assert_eq!(
            open_root_from(Some("rel/cache".into()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.cache/saola-files/open"))
        );
        assert_eq!(open_root_from(None, None), None);
    }

    #[test]
    fn slots_are_private_unique_and_created_with_their_parents() {
        let base = scratch("slots");
        let root = open_root_from(Some(base.as_os_str().to_owned()), None).expect("root");
        let a = create_slot(&root).expect("first slot");
        let b = create_slot(&root).expect("second slot");
        assert_ne!(a, b, "two opens never share a directory");
        assert!(a.starts_with(&root) && b.starts_with(&root));
        for dir in [&a, &b, &root, &base.join("saola-files")] {
            assert_eq!(mode(dir), 0o700, "{dir:?}");
        }
        assert!(is_slot_name(a.file_name().expect("name")));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_root_with_group_or_other_bits_is_refused() {
        for loose in [0o755, 0o750, 0o705, 0o770] {
            let base = scratch("loose");
            let ours = base.join("saola-files");
            let root = ours.join("open");
            fs::create_dir_all(&root).expect("root");
            fs::set_permissions(&ours, fs::Permissions::from_mode(0o700)).expect("chmod");
            fs::set_permissions(&root, fs::Permissions::from_mode(loose)).expect("chmod");
            assert!(
                matches!(create_slot(&root), Err(SlotError::NotPrivate)),
                "{loose:o} must be refused"
            );
            let _ = fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_not_private() {
        let base = scratch("owner");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).expect("chmod");
        let meta_ours = fs::metadata(&base).expect("meta");
        assert!(is_private_dir(&meta_ours, current_uid()));
        // The same directory judged against a different uid is foreign.
        assert!(!is_private_dir(&meta_ours, current_uid().wrapping_add(1)));
        // And a loose one fails even for its owner.
        fs::set_permissions(&base, fs::Permissions::from_mode(0o770)).expect("chmod");
        assert!(!is_private_dir(
            &fs::metadata(&base).expect("meta"),
            current_uid()
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_root_is_refused() {
        let base = scratch("link");
        let target = base.join("elsewhere");
        fs::create_dir(&target).expect("target");
        let ours = base.join("saola-files");
        fs::create_dir(&ours).expect("ours");
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o700)).expect("chmod");
        std::os::unix::fs::symlink(&target, ours.join("open")).expect("symlink");
        assert!(create_slot(&ours.join("open")).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn typed_errors_are_worded_through_the_catalogue() {
        let words = |err: SlotError| match err.into_vfs() {
            VfsError::Other { message } => message,
            other => panic!("expected worded text, got {other:?}"),
        };
        assert!(words(SlotError::NotPrivate).contains("private"));
        assert!(words(SlotError::NoCacheDir).contains("cache"));
        assert!(!words(SlotError::NoFreeName).is_empty());
        assert!(words(SlotError::SizeMismatch).contains("complete"));
    }

    // --- read-only and cleanup ---

    #[test]
    fn sealing_locks_the_file_and_the_slot_and_reads_the_head() {
        let base = scratch("seal");
        let slot = create_slot(&private_root(&base)).expect("slot");
        let name = OsString::from("note.txt");
        fs::write(copy_path(&slot, &name), b"hello").expect("write");
        let sealed = seal(&slot, std::slice::from_ref(&name), &[Some(5)]).expect("seal");
        assert_eq!(sealed[0].head.as_deref(), Some(&b"hello"[..]));
        assert_eq!(mode(&sealed[0].path), 0o444);
        assert_eq!(mode(&slot), 0o500);
        assert!(fs::write(slot.join("note.txt.new"), b"x").is_err() || current_uid() == 0);
        remove_slot(&slot);
        assert!(!slot.exists(), "a sealed slot is still removable");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_truncated_download_is_never_sealed() {
        let base = scratch("truncated");
        let slot = create_slot(&private_root(&base)).expect("slot");
        fs::write(slot.join("big"), b"half").expect("write");
        let result = seal(&slot, &[OsString::from("big")], &[Some(100)]);
        assert!(matches!(result, Err(SlotError::SizeMismatch)));
        assert_eq!(mode(&slot), 0o700, "an unsealed slot stays removable");
        // Unknown size (a symlink, say) skips the check.
        assert!(seal(&slot, &[OsString::from("big")], &[None]).is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sealing_a_missing_file_fails_instead_of_pretending() {
        let base = scratch("seal-missing");
        let slot = create_slot(&private_root(&base)).expect("slot");
        assert!(seal(&slot, &[OsString::from("never-downloaded")], &[None]).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_empty_file_has_no_head() {
        let base = scratch("seal-empty");
        let slot = create_slot(&private_root(&base)).expect("slot");
        fs::write(slot.join("empty"), b"").expect("write");
        let sealed = seal(&slot, &[OsString::from("empty")], &[Some(0)]).expect("seal");
        assert_eq!(sealed[0].head, None);
        let _ = fs::remove_dir_all(&base);
    }

    // --- the startup sweep ---

    #[test]
    fn the_sweep_removes_only_old_slots() {
        let base = scratch("sweep");
        let root = private_root(&base);
        let old = create_slot(&root).expect("old");
        let fresh = create_slot(&root).expect("fresh");
        fs::write(old.join("f"), b"x").expect("write");
        seal(&old, &[OsString::from("f")], &[Some(1)]).expect("seal");
        let uid = current_uid();
        let now = backdated_now();
        let three_days = Duration::from_secs(3 * 24 * 60 * 60);
        assert_eq!(sweep_stale(&root, now, three_days, uid), 0);
        assert!(old.exists() && fresh.exists());
        assert_eq!(sweep_stale(&root, now, STALE_AFTER, uid), 2);
        assert!(!old.exists() && !fresh.exists());
        assert_eq!(sweep_stale(&base.join("none"), now, STALE_AFTER, uid), 0);
        let _ = fs::remove_dir_all(&base);
    }

    /// A victim directory with a file in it, mode 0755, to be targeted by
    /// planted symlinks; the sweep must leave it entirely alone.
    fn victim(base: &Path) -> PathBuf {
        let dir = base.join("victim");
        fs::create_dir(&dir).expect("victim");
        fs::write(dir.join("precious.txt"), b"keep").expect("file");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        dir
    }

    fn assert_untouched(victim: &Path) {
        assert_eq!(
            fs::read(victim.join("precious.txt")).expect("kept"),
            b"keep"
        );
        assert_eq!(mode(victim), 0o755, "never chmod-ed");
    }

    #[test]
    fn a_symlinked_root_means_the_sweep_touches_nothing() {
        let base = scratch("sweep-root-link");
        let target = victim(&base);
        fs::create_dir(target.join("1-2-3")).expect("slot-shaped child");
        let ours = base.join("saola-files");
        fs::create_dir(&ours).expect("ours");
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o700)).expect("chmod");
        std::os::unix::fs::symlink(&target, ours.join("open")).expect("symlink");
        let removed = sweep_stale(
            &ours.join("open"),
            backdated_now(),
            STALE_AFTER,
            current_uid(),
        );
        assert_eq!(removed, 0);
        assert!(target.join("1-2-3").exists());
        assert_untouched(&target);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_app_directory_means_the_sweep_touches_nothing() {
        let base = scratch("sweep-ours-link");
        let target = victim(&base);
        fs::create_dir(target.join("open")).expect("open");
        fs::create_dir(target.join("open").join("1-2-3")).expect("slot-shaped child");
        std::os::unix::fs::symlink(&target, base.join("saola-files")).expect("symlink");
        let root = base.join("saola-files").join("open");
        assert_eq!(
            sweep_stale(&root, backdated_now(), STALE_AFTER, current_uid()),
            0
        );
        assert!(target.join("open").join("1-2-3").exists());
        assert_untouched(&target);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_loose_or_foreign_root_means_the_sweep_touches_nothing() {
        let base = scratch("sweep-loose");
        let root = private_root(&base);
        let slot = create_slot(&root).expect("slot");
        let now = backdated_now();
        // Judged as another user's directory: nothing is removed.
        assert_eq!(
            sweep_stale(&root, now, STALE_AFTER, current_uid().wrapping_add(1)),
            0
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(sweep_stale(&root, now, STALE_AFTER, current_uid()), 0);
        assert!(slot.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_child_is_neither_followed_nor_chmoded() {
        let base = scratch("sweep-child-link");
        let target = victim(&base);
        let root = private_root(&base);
        std::os::unix::fs::symlink(&target, root.join("1-2-3")).expect("symlink");
        assert_eq!(
            sweep_stale(&root, backdated_now(), STALE_AFTER, current_uid()),
            0
        );
        assert!(root.join("1-2-3").symlink_metadata().is_ok(), "link left");
        assert_untouched(&target);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn foreign_shaped_names_are_left_alone() {
        let base = scratch("sweep-names");
        let root = private_root(&base);
        for name in ["Documents", "1-2", "1-2-3-4", "a-b-c", "1--3", "notes-1-2"] {
            fs::create_dir(root.join(name)).expect("dir");
        }
        assert_eq!(
            sweep_stale(&root, backdated_now(), STALE_AFTER, current_uid()),
            0
        );
        assert_eq!(fs::read_dir(&root).expect("read").count(), 6);
        let _ = fs::remove_dir_all(&base);
    }
}
