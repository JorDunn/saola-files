//! `LocalBackend` — the only place in the app allowed to touch `std::fs`
//! directly (CLAUDE.md's rule: all file access goes through a `Backend`).
//! Always compiled, no feature gate: local disk access is the baseline
//! every build ships.
//!
//! Every `std::fs` call runs inside `tokio::task::spawn_blocking`. This
//! module deliberately never calls `tokio::fs` (which does its own
//! internal blocking dispatch): going through `std::fs` directly keeps
//! every blocking call visible in one place and keeps us on
//! `DirEntry::metadata`/`fs::symlink_metadata` exactly, which — per the Rust
//! docs — do not follow a symlink, matching the no-panic, never-follow
//! rule.
//!
//! `watch()` returns `None` in this stage; inotify lands in Stage 5.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::SinkExt;
use futures::channel::mpsc;
use futures::stream::{BoxStream, StreamExt};

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::{Backend, Caps, DirEvent, Location, ReadStream, VfsError, WriteSink};

/// Bytes read/written per chunk on `read`/`write` — big enough that a
/// multi-gigabyte file doesn't produce an absurd number of channel sends,
/// small enough that a slow consumer doesn't force buffering a huge chunk.
const CHUNK_SIZE: usize = 64 * 1024;

/// How many chunks may sit in the read/write channel before the producer
/// blocks — bounded, per CLAUDE.md's async-bridging rule (never an
/// unbounded channel).
const CHANNEL_CAPACITY: usize = 4;

#[derive(Debug, Default)]
pub struct LocalBackend;

impl LocalBackend {
    pub const SCHEME: &'static str = "file";

    pub fn new() -> Self {
        LocalBackend
    }
}

/// Turn a `std::io::Error` into a human-worded [`VfsError`], mapping the
/// kinds that have a specific wording and falling back to
/// [`VfsError::Other`] otherwise. `location` names *what the human asked
/// for*, not raw OS path syntax.
fn io_error(location: &Location, err: io::Error) -> VfsError {
    match err.kind() {
        io::ErrorKind::NotFound => VfsError::NotFound {
            location: location.to_string(),
        },
        io::ErrorKind::PermissionDenied => VfsError::PermissionDenied {
            location: location.to_string(),
        },
        io::ErrorKind::AlreadyExists => VfsError::AlreadyExists {
            location: location.to_string(),
        },
        io::ErrorKind::NotADirectory => VfsError::NotADirectory {
            location: location.to_string(),
        },
        io::ErrorKind::IsADirectory => VfsError::IsADirectory {
            location: location.to_string(),
        },
        _ => VfsError::Other {
            message: format!("{location}: {err}"),
        },
    }
}

/// Build a [`FileEntry`] from a `symlink_metadata`-equivalent result —
/// never a followed `metadata()`.
fn entry_from_metadata(name: OsString, meta: &fs::Metadata) -> FileEntry {
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        EntryKind::Other
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    FileEntry {
        name,
        kind,
        size: meta.len(),
        modified: meta.modified().ok(),
        is_symlink: file_type.is_symlink(),
    }
}

/// Run a blocking closure on the blocking pool, turning a `JoinError`
/// (the task panicked) into a worded [`VfsError`] rather than propagating
/// a panic — the no-panic rule extends to background work.
async fn run_blocking<F, T>(location: &Location, f: F) -> Result<T, VfsError>
where
    F: FnOnce() -> Result<T, VfsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(VfsError::Other {
            message: format!("internal error working on {location}: {join_err}"),
        }),
    }
}

fn list_blocking(location: &Location) -> Result<Vec<FileEntry>, VfsError> {
    let read_dir = fs::read_dir(&location.path).map_err(|err| io_error(location, err))?;
    let mut entries = Vec::new();
    for item in read_dir {
        let item = match item {
            Ok(item) => item,
            Err(err) => {
                // One unreadable entry (e.g. a race with a concurrent
                // delete) doesn't sink the whole listing.
                eprintln!("saola-files: skipping an entry in {location}: {err}");
                continue;
            }
        };
        // `DirEntry::metadata` on Unix uses `fstatat` without
        // `AT_SYMLINK_FOLLOW` — it does not traverse a symlink, matching
        // `symlink_metadata` exactly, without a second path lookup.
        let meta = match item.metadata() {
            Ok(meta) => meta,
            Err(err) => {
                eprintln!(
                    "saola-files: skipping {:?} in {location}: {err}",
                    item.file_name()
                );
                continue;
            }
        };
        entries.push(entry_from_metadata(item.file_name(), &meta));
    }
    Ok(entries)
}

fn metadata_blocking(location: &Location) -> Result<FileEntry, VfsError> {
    let meta = fs::symlink_metadata(&location.path).map_err(|err| io_error(location, err))?;
    let name = location
        .path
        .file_name()
        .map(OsString::from)
        .unwrap_or_default();
    Ok(entry_from_metadata(name, &meta))
}

fn mkdir_blocking(location: &Location) -> Result<(), VfsError> {
    fs::create_dir(&location.path).map_err(|err| io_error(location, err))
}

fn rename_blocking(from: &Location, to: &Location) -> Result<(), VfsError> {
    fs::rename(&from.path, &to.path).map_err(|err| io_error(from, err))
}

/// Non-recursive: removes an empty directory or a single file/symlink. A
/// recursive delete (and trash integration) belongs to the ops engine in
/// a later stage, not this trait method.
fn remove_blocking(location: &Location) -> Result<(), VfsError> {
    let meta = fs::symlink_metadata(&location.path).map_err(|err| io_error(location, err))?;
    if meta.is_dir() {
        fs::remove_dir(&location.path).map_err(|err| io_error(location, err))
    } else {
        fs::remove_file(&location.path).map_err(|err| io_error(location, err))
    }
}

fn set_times_blocking(
    location: &Location,
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> Result<(), VfsError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&location.path)
        .map_err(|err| io_error(location, err))?;
    let mut times = fs::FileTimes::new();
    if let Some(accessed) = accessed {
        times = times.set_accessed(accessed);
    }
    if let Some(modified) = modified {
        times = times.set_modified(modified);
    }
    file.set_times(times).map_err(|err| io_error(location, err))
}

#[async_trait]
impl Backend for LocalBackend {
    fn scheme(&self) -> &'static str {
        Self::SCHEME
    }

    fn caps(&self) -> Caps {
        // No `WATCH` (inotify lands Stage 5) and no `TRASH` (`remove()` is
        // a real permanent delete — claiming trash here would be a
        // capability lie the UI would word wrong). `SET_PERMISSIONS` has
        // no backing trait method at all yet.
        Caps::RENAME_IN_PLACE | Caps::LOCAL_PATH | Caps::THUMBNAILS
    }

    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError> {
        let loc = location.clone();
        run_blocking(location, move || list_blocking(&loc)).await
    }

    async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
        let loc = location.clone();
        run_blocking(location, move || metadata_blocking(&loc)).await
    }

    async fn read(&self, location: &Location) -> Result<ReadStream, VfsError> {
        let loc = location.clone();
        let file = run_blocking(location, move || {
            fs::File::open(&loc.path).map_err(|err| io_error(&loc, err))
        })
        .await?;

        let (mut tx, rx) = mpsc::channel::<Result<Vec<u8>, VfsError>>(CHANNEL_CAPACITY);
        let loc = location.clone();
        // Detached: the receiver end (`rx`, boxed below) drives how long
        // this runs — dropping it makes the next blocked `send` fail,
        // which the loop treats as "reader gave up" and exits on.
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut file = file;
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if futures::executor::block_on(tx.send(Ok(buf[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = futures::executor::block_on(tx.send(Err(io_error(&loc, err))));
                        break;
                    }
                }
            }
        });

        Ok(rx.boxed())
    }

    async fn write(&self, location: &Location) -> Result<WriteSink, VfsError> {
        let loc = location.clone();
        let file = run_blocking(location, move || {
            fs::File::create(&loc.path).map_err(|err| io_error(&loc, err))
        })
        .await?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let loc = location.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = file;
            while let Some(chunk) = futures::executor::block_on(rx.next()) {
                if let Err(err) = file.write_all(&chunk) {
                    eprintln!("saola-files: write to {loc} failed: {err}");
                    break;
                }
            }
        });

        let closed_message = format!("write channel to {location} closed");
        let sink = tx.sink_map_err(move |_| VfsError::Other {
            message: closed_message.clone(),
        });
        Ok(Box::pin(sink))
    }

    async fn mkdir(&self, location: &Location) -> Result<(), VfsError> {
        let loc = location.clone();
        run_blocking(location, move || mkdir_blocking(&loc)).await
    }

    async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError> {
        let (f, t) = (from.clone(), to.clone());
        run_blocking(from, move || rename_blocking(&f, &t)).await
    }

    async fn remove(&self, location: &Location) -> Result<(), VfsError> {
        let loc = location.clone();
        run_blocking(location, move || remove_blocking(&loc)).await
    }

    async fn set_times(
        &self,
        location: &Location,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), VfsError> {
        let loc = location.clone();
        run_blocking(location, move || {
            set_times_blocking(&loc, accessed, modified)
        })
        .await
    }

    fn watch(&self, _location: &Location) -> Option<BoxStream<'static, DirEvent>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn backend() -> LocalBackend {
        LocalBackend::new()
    }

    // Every test below that touches a `Backend` method needs a live Tokio
    // runtime for `tokio::task::spawn_blocking` to run on (see the
    // `[dev-dependencies]` survey comment in `Cargo.toml`) — `#[tokio::test]`
    // provides a throwaway current-thread one per test, torn down when the
    // test function returns.

    #[tokio::test]
    async fn lists_a_temp_directory() {
        let dir = tempdir();
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();

        let entries = backend().list(&Location::local(&dir)).await.unwrap();
        let mut names: Vec<_> = entries
            .iter()
            .map(|e| e.display_name().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.txt".to_owned(), "sub".to_owned()]);

        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert_eq!(sub.kind, EntryKind::Directory);
        let file = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, 2);

        cleanup(dir);
    }

    #[tokio::test]
    async fn lists_a_non_utf8_named_entry() {
        let dir = tempdir();
        let raw_name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        std::fs::write(dir.join(raw_name), b"x").unwrap();

        let entries = backend().list(&Location::local(&dir)).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_bytes(), raw_name.as_bytes());

        cleanup(dir);
    }

    #[tokio::test]
    async fn missing_directory_is_not_found() {
        let missing = Location::local("/nonexistent/saola-files-test-dir");
        let result = backend().list(&missing).await;
        assert!(matches!(result, Err(VfsError::NotFound { .. })));
    }

    #[tokio::test]
    async fn permission_denied_is_worded_not_panicked() {
        // Skip under a UID that ignores permission bits (root, some CI
        // sandboxes) rather than asserting a false failure.
        if running_as_root() {
            return;
        }
        let dir = tempdir();
        std::fs::create_dir(dir.join("locked")).unwrap();
        let locked = dir.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = backend().list(&Location::local(&locked)).await;

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        cleanup(dir);

        assert!(matches!(result, Err(VfsError::PermissionDenied { .. })));
    }

    #[tokio::test]
    async fn symlinks_are_never_followed_for_kind() {
        let dir = tempdir();
        std::fs::create_dir(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();

        let entries = backend().list(&Location::local(&dir)).await.unwrap();
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_symlink);
        // Never resolved to `Directory`, even though the target is one.
        assert_eq!(link.kind, EntryKind::Other);

        cleanup(dir);
    }

    #[test]
    fn watch_returns_none_until_stage_5() {
        assert!(backend().watch(&Location::local("/")).is_none());
    }

    #[test]
    fn caps_do_not_claim_watch_or_trash() {
        // Capability-honest: `watch()` really does return `None` above,
        // and `remove()` really does permanently delete (no trash dir
        // involved) — claiming either bit here would be a lie the UI
        // would word wrong.
        let caps = backend().caps();
        assert!(!caps.contains(Caps::WATCH));
        assert!(!caps.contains(Caps::TRASH));
        assert!(caps.contains(Caps::LOCAL_PATH));
    }

    #[tokio::test]
    async fn round_trips_metadata_mkdir_rename_remove() {
        let dir = tempdir();
        let child = Location::local(dir.join("child"));

        backend().mkdir(&child).await.unwrap();
        let meta = backend().metadata(&child).await.unwrap();
        assert_eq!(meta.kind, EntryKind::Directory);

        let renamed = Location::local(dir.join("renamed"));
        backend().rename(&child, &renamed).await.unwrap();
        assert!(dir.join("renamed").is_dir());

        backend().remove(&renamed).await.unwrap();
        assert!(!dir.join("renamed").exists());

        cleanup(dir);
    }

    #[tokio::test]
    async fn read_streams_a_files_full_contents_in_chunks() {
        let dir = tempdir();
        let path = dir.join("payload.bin");
        // Bigger than one chunk so the stream actually has to loop.
        let payload = vec![7u8; CHUNK_SIZE + 128];
        std::fs::write(&path, &payload).unwrap();

        let mut stream = backend().read(&Location::local(&path)).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend(chunk.unwrap());
        }

        assert_eq!(collected, payload);
        cleanup(dir);
    }

    #[tokio::test]
    async fn write_creates_a_file_from_streamed_chunks() {
        let dir = tempdir();
        let path = dir.join("out.bin");

        {
            let mut sink = backend().write(&Location::local(&path)).await.unwrap();
            sink.send(vec![1, 2, 3]).await.unwrap();
            sink.send(vec![4, 5]).await.unwrap();
            // Dropping `sink` closes the channel; the detached blocking
            // writer task sees the close, flushes, and exits — its OS
            // thread runs independently of this async task, so the
            // runtime stays alive (we haven't returned from the test
            // function yet) while we poll for it below.
        }

        // Bounded poll, not a busy loop: 200 x 5ms is a 1s ceiling.
        let mut contents = Vec::new();
        for _ in 0..200 {
            contents = std::fs::read(&path).unwrap_or_default();
            if contents.len() == 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(contents, vec![1, 2, 3, 4, 5]);

        cleanup(dir);
    }

    // ── tiny temp-dir helpers (no `tempfile` dependency for this alone) ──

    fn tempdir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-test-{}-{}",
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

    fn cleanup(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    use std::os::unix::fs::PermissionsExt;

    /// Lets the permission-denied test skip under a UID that ignores mode
    /// bits (root, some CI sandboxes) without pulling in `libc`/`rustix`
    /// for one syscall — shells out to `id -u`, present on every Linux
    /// system this app targets.
    fn running_as_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .is_some_and(|uid| uid.trim() == "0")
    }
}
