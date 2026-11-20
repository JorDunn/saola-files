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
//! # Where copies live
//!
//! `<runtime dir>/saola-files/open/<slot>/<original name>`, where the
//! runtime dir is `$XDG_RUNTIME_DIR` (already per-user, mode 0700, cleared at
//! logout). With no runtime dir the fallback is
//! `<temp dir>/saola-files-<uid>/open/…`. Every directory this module
//! creates is mode 0700 and, when it already exists, must be a real
//! directory (not a symlink) owned by this user with no group/other
//! access, or [`create_slot`] refuses: a predictable name in a shared `/tmp`
//! is exactly what another local user could pre-create or symlink.
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
//! instead of looking like it worked.
//!
//! # Cleanup
//!
//! Copies are never removed while the app runs, because an application may
//! still have one open. They go when: the open fails or is cancelled
//! ([`remove_slot`], immediately); the user logs out (the runtime dir);
//! or a later start finds a slot older than [`STALE_AFTER`]
//! ([`sweep_stale`]). On Linux, deleting a file an application already holds
//! open does not break that application, so even the sweep is safe for
//! long-lived windows; the age bound just avoids pulling files out from
//! under an app that reopens them by path.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::vfs::{Caps, Location};

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

/// The directory that holds all slots, from already-resolved inputs (so
/// tests pass their own; nothing here reads the environment).
pub fn open_root(runtime_dir: Option<&Path>, temp_dir: &Path, uid: u32) -> PathBuf {
    match runtime_dir {
        Some(dir) => dir.join("saola-files").join("open"),
        None => temp_dir.join(format!("saola-files-{uid}")).join("open"),
    }
}

/// [`open_root`] from the real environment: the one thin wrapper. An empty
/// `$XDG_RUNTIME_DIR` counts as unset (the project's env rule).
pub fn open_root_from_env() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    open_root(runtime.as_deref(), &std::env::temp_dir(), current_uid())
}

/// The uid this process runs as, without `libc`/`unsafe`: the owner of
/// `/proc/self`. `u32::MAX` if even that is unreadable, which then fails the
/// ownership check below rather than matching anything.
pub fn current_uid() -> u32 {
    fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .unwrap_or(u32::MAX)
}

static SLOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates (and returns) a new, empty, private slot directory under `root`,
/// creating `root` and any missing parents privately first.
pub fn create_slot(root: &Path) -> io::Result<PathBuf> {
    let uid = current_uid();
    // The app's own directory (`…/saola-files`) and `root` itself are both
    // ours to police; whatever holds them (`$XDG_RUNTIME_DIR`, `/tmp`) is not.
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
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not find an unused name for the download folder",
    ))
}

/// Makes `dir` (and missing ancestors below an existing one) a private
/// directory owned by `uid`; checks an existing one instead of trusting it.
fn ensure_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    // Parents first, so only the components *we* make are checked/created
    // privately; a parent that already exists (like `$XDG_RUNTIME_DIR`
    // itself, or `/tmp`) is left alone.
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && fs::symlink_metadata(parent).is_err()
    {
        ensure_private_dir(parent, uid)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    // It exists. `symlink_metadata` does not follow a symlink, so a planted
    // link is seen as a link and refused.
    let meta = fs::symlink_metadata(dir)?;
    let private = meta.is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o077 == 0;
    if private {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the download folder exists but is not private to this user",
        ))
    }
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

/// Makes the downloaded `names` in `slot` read-only (files 0444, slot 0500)
/// and reads each head. Fails if a file is missing or cannot be locked, so
/// an unprotected copy is never opened and called read-only.
pub fn seal(slot: &Path, names: &[OsString]) -> io::Result<Vec<SealedCopy>> {
    let mut sealed = Vec::with_capacity(names.len());
    for name in names {
        let path = copy_path(slot, name);
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
/// [`seal`] removed it.
pub fn remove_slot(slot: &Path) {
    let _ = fs::set_permissions(slot, fs::Permissions::from_mode(0o700));
    let _ = fs::remove_dir_all(slot);
}

/// Removes slots under `root` last modified more than `max_age` before
/// `now`. Returns how many were removed. A missing `root` is simply nothing
/// to do. Only real directories are touched; a symlink planted in `root` is
/// ignored, never followed.
pub fn sweep_stale(root: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if meta.is_dir() && old {
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
        // A `file` location on a backend claiming no LOCAL_PATH downloads;
        // a remote-looking one on a backend claiming it opens directly.
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
        // A root path has no file name at all.
        assert_eq!(
            open_plan(Caps::empty(), &remote("/")),
            OpenPlan::Refused(Refusal::UnsafeName)
        );
    }

    // --- where copies live ---

    #[test]
    fn the_root_prefers_the_runtime_dir_and_falls_back_to_a_per_user_temp_dir() {
        assert_eq!(
            open_root(Some(Path::new("/run/user/1000")), Path::new("/tmp"), 1000),
            PathBuf::from("/run/user/1000/saola-files/open")
        );
        assert_eq!(
            open_root(None, Path::new("/tmp"), 1000),
            PathBuf::from("/tmp/saola-files-1000/open")
        );
    }

    #[test]
    fn slots_are_private_unique_and_created_with_their_parents() {
        let base = scratch("slots");
        let root = open_root(Some(&base), &base, current_uid());
        let a = create_slot(&root).expect("first slot");
        let b = create_slot(&root).expect("second slot");
        assert_ne!(a, b, "two opens never share a directory");
        assert!(a.starts_with(&root) && b.starts_with(&root));
        for dir in [&a, &b, &root, &base.join("saola-files")] {
            assert_eq!(mode(dir), 0o700, "{dir:?}");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_pre_existing_open_to_others_root_is_refused() {
        let base = scratch("loose");
        let ours = base.join("saola-files");
        let root = ours.join("open");
        fs::create_dir_all(&root).expect("root");
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = create_slot(&root).expect_err("a world-readable root must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_root_is_refused() {
        let base = scratch("link");
        let target = base.join("elsewhere");
        fs::create_dir(&target).expect("target");
        let root = base.join("saola-files").join("open");
        fs::create_dir(base.join("saola-files")).expect("ours");
        fs::set_permissions(base.join("saola-files"), fs::Permissions::from_mode(0o700))
            .expect("chmod");
        std::os::unix::fs::symlink(&target, &root).expect("symlink");
        assert!(create_slot(&root).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    // --- read-only and cleanup ---

    #[test]
    fn sealing_locks_the_file_and_the_slot_and_reads_the_head() {
        let base = scratch("seal");
        let slot = create_slot(&base.join("saola-files").join("open")).expect("slot");
        let name = OsString::from("note.txt");
        fs::write(copy_path(&slot, &name), b"hello").expect("write");
        let sealed = seal(&slot, std::slice::from_ref(&name)).expect("seal");
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].head.as_deref(), Some(&b"hello"[..]));
        assert_eq!(mode(&sealed[0].path), 0o444);
        assert_eq!(mode(&slot), 0o500);
        // Saving by rename-over needs the directory writable: it must fail.
        assert!(fs::write(slot.join("note.txt.new"), b"x").is_err() || current_uid() == 0);
        remove_slot(&slot);
        assert!(!slot.exists(), "a sealed slot is still removable");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sealing_a_missing_file_fails_instead_of_pretending() {
        let base = scratch("seal-missing");
        let slot = create_slot(&base.join("saola-files").join("open")).expect("slot");
        assert!(seal(&slot, &[OsString::from("never-downloaded")]).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_empty_file_has_no_head() {
        let base = scratch("seal-empty");
        let slot = create_slot(&base.join("saola-files").join("open")).expect("slot");
        fs::write(slot.join("empty"), b"").expect("write");
        let sealed = seal(&slot, &[OsString::from("empty")]).expect("seal");
        assert_eq!(sealed[0].head, None);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_sweep_removes_only_old_slots() {
        let base = scratch("sweep");
        let root = base.join("saola-files").join("open");
        let old = create_slot(&root).expect("old");
        let fresh = create_slot(&root).expect("fresh");
        fs::write(old.join("f"), b"x").expect("write");
        seal(&old, &[OsString::from("f")]).expect("seal");
        // "Now" is two days on: both are older than a day, so shift the
        // clock per slot by sweeping with different ages instead.
        let now = SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60);
        assert_eq!(
            sweep_stale(&root, now, Duration::from_secs(3 * 24 * 60 * 60)),
            0
        );
        assert!(old.exists() && fresh.exists());
        assert_eq!(sweep_stale(&root, now, STALE_AFTER), 2);
        assert!(!old.exists() && !fresh.exists());
        // Nothing there at all is fine.
        assert_eq!(sweep_stale(&base.join("none"), now, STALE_AFTER), 0);
        let _ = fs::remove_dir_all(&base);
    }
}
