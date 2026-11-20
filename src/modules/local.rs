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
//! **`watch()` (Stage 5).** Real inotify, via the `inotify` crate's async
//! `stream` feature (see the dated survey comment in `Cargo.toml`) — no
//! polling, per CLAUDE.md's "signal, never poll" rule. [`watch`] itself
//! stays synchronous (`Inotify::init`/`Watches::add` are fast syscalls; a
//! failure there — e.g. the directory doesn't exist, or the process is out
//! of inotify watches — degrades to `None`, same as a backend that never
//! had `Caps::WATCH`), and spawns [`process_watch_stream`] onto the shared
//! tokio runtime to translate raw kernel events into [`DirEvent`]s and
//! `try_send` them across a bounded `futures::channel::mpsc` — the exact
//! same type `iced::futures::channel::mpsc` names (this crate's `core`/
//! `modules` layers stay iced-free by depending on the `futures` crate
//! directly rather than through `iced`; see `core::vfs`'s `ReadStream`/
//! `WriteSink` for the same pattern already established there — `Cargo.lock`
//! only ever resolves one `futures` in the graph, so the types are
//! identical either way `use` names them).
//!
//! Two inotify gotchas this stage handles explicitly (verified against the
//! `inotify` crate's own docs, see `EventMask`):
//! - **Renames split across two raw events** (`IN_MOVED_FROM`/
//!   `IN_MOVED_TO`, correlated by a shared `cookie`) that aren't guaranteed
//!   to arrive back-to-back — [`process_watch_stream`] holds a
//!   `MOVED_FROM` pending for up to [`RENAME_PAIR_WINDOW`] waiting for its
//!   `MOVED_TO` partner; if no partner shows up in time (the file was
//!   moved *out* of the watched directory), the pending half is emitted as
//!   a plain [`DirEvent::Removed`] instead of holding it forever. A bare
//!   `MOVED_TO` with no pending partner (moved *in* from elsewhere) is
//!   emitted immediately as [`DirEvent::Created`] — there's nothing to
//!   wait for.
//! - **Queue overflow** (`EventMask::Q_OVERFLOW`, or this backend's own
//!   bridge channel filling up because the consumer fell behind) means the
//!   backend can no longer promise it reported every change — see
//!   [`DirEvent::Overflow`]'s docs for why the recovery is a full re-list,
//!   not an attempt to reconcile.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::SinkExt;
use futures::channel::mpsc;
use futures::sink::Sink;
use futures::stream::{BoxStream, StreamExt};
use inotify::{EventMask, EventStream, Inotify, WatchMask};
use tokio::task::JoinHandle;
use tokio::time::Instant as TokioInstant;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::{
    Backend, Caps, DirEvent, FsStats, Location, ReadStream, VfsError, WriteOptions, WriteSink,
};

/// Bytes read/written per chunk on `read`/`write` — big enough that a
/// multi-gigabyte file doesn't produce an absurd number of channel sends,
/// small enough that a slow consumer doesn't force buffering a huge chunk.
const CHUNK_SIZE: usize = 64 * 1024;

/// How many chunks may sit in the read/write channel before the producer
/// blocks — bounded, per CLAUDE.md's async-bridging rule (never an
/// unbounded channel).
const CHANNEL_CAPACITY: usize = 4;

/// Raw inotify read buffer size, in bytes — comfortably more than one
/// `inotify_event` plus a `NAME_MAX`-length filename, so an ordinary burst
/// doesn't force multiple syscalls to drain a single wakeup. (The crate's
/// own docs warn a `Vec::with_capacity` alone is wrong here — it has
/// reserved capacity but length `0` — so this is built with `vec![0u8; N]`
/// everywhere it's used.)
const WATCH_BUFFER_BYTES: usize = 4096;

/// How many translated [`DirEvent`]s may sit in [`LocalBackend::watch`]'s
/// bridge channel before `try_send` starts failing (CLAUDE.md: bounded,
/// `try_send`, never blocking) — past this, [`process_watch_stream`] stops
/// trying to deliver individual events and waits for room to deliver one
/// [`DirEvent::Overflow`] instead (see the module docs' "queue overflow"
/// gotcha).
const WATCH_CHANNEL_CAPACITY: usize = 64;

/// How long a `MOVED_FROM` may sit unpaired before [`process_watch_stream`]
/// gives up waiting for its `MOVED_TO` and emits a plain
/// [`DirEvent::Removed`] instead (see the module docs' rename-pairing
/// gotcha).
const RENAME_PAIR_WINDOW: Duration = Duration::from_millis(50);

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
        // `st_mode & 0o7777`: the permission bits alone, without the file-
        // type bits `st_mode`'s upper bits also carry (`kind`/`is_symlink`
        // already cover that) — `Permissions::mode()` returns the full
        // `st_mode`, so this masks it down to just what a `chmod` argument
        // or an `rwxr-xr-x` rendering wants (`ui::dialogs::properties`).
        mode: Some(meta.permissions().mode() & 0o7777),
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

/// Create a symlink. `std::os::unix::fs::symlink` takes the target as a
/// `Path`, which on Unix is raw bytes, so a non-UTF-8 target is stored
/// exactly as given.
fn symlink_blocking(target: &Path, link: &Location) -> Result<(), VfsError> {
    std::os::unix::fs::symlink(target, &link.path).map_err(|err| io_error(link, err))
}

/// chmod. `from_mode` keeps only what the caller passes; masking to
/// `0o7777` drops any file-type bits so a mode copied from `st_mode`
/// cannot confuse the call.
fn set_permissions_blocking(location: &Location, mode: u32) -> Result<(), VfsError> {
    fs::set_permissions(&location.path, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|err| io_error(location, err))
}

/// `statvfs(2)` via rustix (safe, no libc). Block counts are in units of
/// the fragment size (`f_frsize`); the multiplications saturate rather
/// than overflow on a hostile or absurd filesystem.
fn statfs_blocking(location: &Location) -> Result<FsStats, VfsError> {
    let stat = rustix::fs::statvfs(&location.path)
        // `Errno` converts to `io::Error`, so the existing worded mapping
        // (NotFound, PermissionDenied, ...) applies unchanged.
        .map_err(|errno| io_error(location, io::Error::from(errno)))?;
    let unit = stat.f_frsize;
    Ok(FsStats {
        total: stat.f_blocks.saturating_mul(unit),
        free: stat.f_bfree.saturating_mul(unit),
        available: stat.f_bavail.saturating_mul(unit),
    })
}

/// A writer that can also be flushed to stable storage. `fs::File` is the
/// real one; tests implement it for in-memory writers so a failing flush
/// can be injected without a broken disk.
trait SyncWrite: io::Write {
    fn sync(&mut self) -> io::Result<()>;
}

impl SyncWrite for fs::File {
    /// `sync_all` (fsync), not `sync_data` (fdatasync): for a file that was
    /// just created and written, fdatasync must commit the new size and
    /// block allocations anyway, so it saves nothing measurable here, and
    /// fsync is the call every filesystem implements without surprises.
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

/// Opens `location` for [`LocalBackend::write_with`]. `create_new` maps to
/// `O_CREAT | O_EXCL`, which also refuses a symlink at the name rather than
/// following it; otherwise create-or-truncate, like `File::create`.
/// `owner_only` creates the file as `0o600` (`OpenOptionsExt::mode`, which
/// only applies when the file is created; the umask can narrow it further,
/// never widen it).
fn open_for_write(location: &Location, options: WriteOptions) -> Result<fs::File, VfsError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut open = fs::OpenOptions::new();
    open.write(true);
    if options.owner_only {
        open.mode(0o600);
    }
    if options.create_new {
        open.create_new(true);
    } else {
        open.create(true).truncate(true);
    }
    open.open(&location.path)
        .map_err(|err| io_error(location, err))
}

/// The writer thread's body: writes each chunk to `file` in order, stops at
/// the first error and returns it, flushes at the end and, when `sync` is
/// set, flushes to stable storage too (a failed sync is a failed write).
/// Generic over [`SyncWrite`] (not tied to `fs::File`) so a test can inject
/// a writer that fails on chunk N or on the sync. When this returns early
/// the receiver is dropped, so later `send`s on the sink fail too;
/// `poll_close` carries the real error.
fn write_chunks<W: SyncWrite>(
    mut file: W,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    sync: bool,
) -> io::Result<()> {
    while let Some(chunk) = futures::executor::block_on(rx.next()) {
        file.write_all(&chunk)?;
    }
    file.flush()?;
    if sync {
        file.sync()?;
    }
    Ok(())
}

/// [`LocalBackend::same_file`]'s check: same device and inode, following
/// symlinks (`fs::metadata`, not `symlink_metadata`), so a hard link, a
/// symlink to the file and a path through a symlinked folder all count.
/// A side that doesn't exist (a dangling symlink included) is `Ok(false)`;
/// any other failure to look (permission denied, a symlink loop) is the
/// error, because it can't rule out the same file.
fn same_inode(a: &Location, b: &Location) -> Result<bool, VfsError> {
    use std::os::unix::fs::MetadataExt;
    let stat = |location: &Location| match fs::metadata(&location.path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(location, err)),
    };
    match (stat(a)?, stat(b)?) {
        (Some(a), Some(b)) => Ok(a.dev() == b.dev() && a.ino() == b.ino()),
        _ => Ok(false),
    }
}

/// The [`WriteSink`] `LocalBackend::write` returns.
///
/// Wraps the raw `mpsc::Sender<Vec<u8>>` fed to the detached blocking
/// writer thread (see `write`'s doc comment on why the write itself runs
/// on `spawn_blocking`) so that `Sink::close` — the one signal a caller
/// has for "I'm done writing" — actually waits for that thread to finish
/// flushing every queued chunk to disk before resolving.
///
/// **The bug this fixes, and why it mattered.** `mpsc::Sender<T>`'s own
/// `Sink` impl (`futures_channel::mpsc::sink_impl`) makes `poll_close`
/// call `self.disconnect()` and return `Ready` *immediately* — it only
/// closes the channel, it does not wait for the receiver (here, the
/// spawn_blocking thread) to drain it. A caller treating a bare `tx.
/// sink_map_err(..)`'s `close().await` as "the write is durable" (Stage
/// 8's `core::fs::ops::copy_bytes` did, until this was caught by that
/// stage's own `conflict_apply_to_all_answers_every_later_conflict_
/// without_prompting` integration test flaking on the *last* file in a
/// multi-file op — the one with the least slack time before the test's
/// own assertions ran) gets a **false completion signal**: the file can
/// still be mid-`write_all` on the blocking thread when `close()` returns.
/// For an ordinary copy that's a flaky read-back; for `core::fs::ops`'s
/// Move (which deletes the *source* immediately after a successful copy)
/// it was a real, if narrow, data-loss window. Fixing it here — the one
/// place `WriteSink` is actually constructed — benefits every present and
/// future caller of `Backend::write`, not just the one that happened to
/// notice.
struct WriterSink {
    tx: mpsc::Sender<Vec<u8>>,
    /// `None` after `poll_close` has already joined it once — `Sink::
    /// close` can be polled more than once after completing (callers are
    /// allowed to call it repeatedly), and joining an already-joined
    /// `JoinHandle` panics, so this is `take()`n the moment it resolves.
    ///
    /// The thread's result is its first write error, if any: a failed
    /// `write_all` (ENOSPC on the last chunks, say) must reach the caller
    /// through `poll_close`, or a truncated file reports as written.
    join: Option<JoinHandle<io::Result<()>>>,
    location: Location,
}

impl Sink<Vec<u8>> for WriterSink {
    type Error = VfsError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        Pin::new(&mut self.tx)
            .poll_ready(cx)
            .map_err(|_| VfsError::Other {
                message: format!("write channel to {} closed", self.location),
            })
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), VfsError> {
        Pin::new(&mut self.tx)
            .start_send(item)
            .map_err(|_| VfsError::Other {
                message: format!("write channel to {} closed", self.location),
            })
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        Pin::new(&mut self.tx)
            .poll_flush(cx)
            .map_err(|_| VfsError::Other {
                message: format!("write channel to {} closed", self.location),
            })
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        // Closing the channel is what lets the writer thread's `rx.next()`
        // loop see `None` and exit — idempotent (`mpsc::Sender::disconnect`
        // tolerates being called more than once), so no need to guard it
        // with the same `Option`-based "already done" tracking `join` needs.
        let _ = Pin::new(&mut self.tx).poll_close(cx);

        let Some(join) = self.join.as_mut() else {
            // Already joined by an earlier `poll_close` — nothing left to
            // wait for.
            return Poll::Ready(Ok(()));
        };
        match Pin::new(join).poll(cx) {
            Poll::Ready(result) => {
                self.join = None;
                match result {
                    // The blocking task panicked — the no-panic rule
                    // extends to background work (see `run_blocking`'s
                    // identical posture for the read/list/mkdir side).
                    Err(err) => Poll::Ready(Err(VfsError::Other {
                        message: format!("internal error writing {}: {err}", self.location),
                    })),
                    // The writer hit an I/O error: durable-on-close means
                    // the caller must hear about it.
                    Ok(Err(err)) => Poll::Ready(Err(io_error(&self.location, err))),
                    Ok(Ok(())) => Poll::Ready(Ok(())),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[async_trait]
impl Backend for LocalBackend {
    fn scheme(&self) -> &'static str {
        Self::SCHEME
    }

    fn caps(&self) -> Caps {
        // `WATCH` as of Stage 5 (`watch()` below is real inotify). `TRASH`
        // as of Stage 9: `remove()` is still a real permanent delete (the
        // `Backend` trait has no `trash()` method — see
        // `core::fs::trash`'s module doc comment on why that stays a
        // standalone local-only module instead), but the UI's delete
        // handling (`main.rs`) checks this bit and calls
        // `core::fs::trash::trash` directly for local locations rather
        // than going through this trait when it's set, so claiming it here
        // is honest, not a capability lie the way it would have been
        // before this stage existed. `SET_PERMISSIONS`, `SYMLINK` and
        // `STATFS` each have a real method below (std chmod, std symlink,
        // rustix statvfs), so all three are honest too.
        Caps::WATCH
            | Caps::TRASH
            | Caps::RENAME_IN_PLACE
            | Caps::LOCAL_PATH
            | Caps::THUMBNAILS
            | Caps::SET_PERMISSIONS
            | Caps::SYMLINK
            | Caps::STATFS
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
        self.write_with(location, WriteOptions::default()).await
    }

    async fn write_with(
        &self,
        location: &Location,
        options: WriteOptions,
    ) -> Result<WriteSink, VfsError> {
        let loc = location.clone();
        let file = run_blocking(location, move || open_for_write(&loc, options)).await?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let loc = location.clone();
        let join = tokio::task::spawn_blocking(move || {
            let result = write_chunks(file, &mut rx, options.sync);
            if let Err(err) = &result {
                eprintln!("saola-files: write to {loc} failed: {err}");
            }
            result
        });

        Ok(Box::pin(WriterSink {
            tx,
            join: Some(join),
            location: location.clone(),
        }))
    }

    async fn same_file(&self, a: &Location, b: &Location) -> Result<bool, VfsError> {
        if a == b {
            return Ok(true);
        }
        let (a_owned, b_owned) = (a.clone(), b.clone());
        run_blocking(a, move || same_inode(&a_owned, &b_owned)).await
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

    async fn symlink(&self, target: &Path, link: &Location) -> Result<(), VfsError> {
        // Owned copies: the closure runs on another thread and must be
        // `'static`, so it cannot borrow the arguments.
        let (target, link_owned) = (target.to_path_buf(), link.clone());
        run_blocking(link, move || symlink_blocking(&target, &link_owned)).await
    }

    async fn set_permissions(&self, location: &Location, mode: u32) -> Result<(), VfsError> {
        let loc = location.clone();
        run_blocking(location, move || set_permissions_blocking(&loc, mode)).await
    }

    async fn statfs(&self, location: &Location) -> Result<FsStats, VfsError> {
        let loc = location.clone();
        run_blocking(location, move || statfs_blocking(&loc)).await
    }

    fn watch(&self, location: &Location) -> Option<BoxStream<'static, DirEvent>> {
        let inotify = match Inotify::init() {
            Ok(inotify) => inotify,
            Err(err) => {
                eprintln!("saola-files: could not start watching {location}: {err}");
                return None;
            }
        };

        // `MODIFY`/`ATTRIB`/`CLOSE_WRITE` all fold into `DirEvent::Changed`
        // downstream (`process_watch_stream`) — kept as separate mask bits
        // here only because that's how inotify itself reports them.
        let mask = WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::CLOSE_WRITE
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF;
        if let Err(err) = inotify.watches().add(&location.path, mask) {
            eprintln!("saola-files: could not watch {location}: {err}");
            return None;
        }

        let stream = match inotify.into_event_stream(vec![0u8; WATCH_BUFFER_BYTES]) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("saola-files: could not start an inotify stream for {location}: {err}");
                return None;
            }
        };

        let (tx, rx) = mpsc::channel::<DirEvent>(WATCH_CHANNEL_CAPACITY);
        // Detached, like `read`/`write`'s spawned work above: the returned
        // `rx` (boxed below) is what governs how long this runs — dropping
        // it (the view navigated away, or the whole app shut down) makes
        // every subsequent `try_send` below fail, which
        // `process_watch_stream` treats as "nobody's listening anymore"
        // and exits on. Uses `tokio::spawn` (not `spawn_blocking`, unlike
        // this file's other backend calls): the inotify `EventStream` is
        // genuinely non-blocking async I/O against the shared runtime's
        // `AsyncFd`, not a blocking syscall wrapped for the blocking pool.
        tokio::spawn(process_watch_stream(stream, tx));

        Some(rx.boxed())
    }
}

/// Translate one directory's raw inotify events into [`DirEvent`]s and
/// `try_send` them into `tx` — the async task [`LocalBackend::watch`]
/// spawns. See the module docs for the rename-pairing and queue-overflow
/// gotchas this implements.
async fn process_watch_stream(mut events: EventStream<Vec<u8>>, mut tx: mpsc::Sender<DirEvent>) {
    // `MOVED_FROM` names waiting for a same-`cookie` `MOVED_TO`, each with
    // the absolute instant it should be given up on and re-emitted as a
    // plain `Removed` instead (see `RENAME_PAIR_WINDOW`'s docs). Keyed by
    // cookie because that's the only thing that correlates the pair;
    // multiple renames can be in flight at once (e.g. a multi-select move),
    // each with its own independent deadline.
    let mut pending_moves: HashMap<u32, (OsString, TokioInstant)> = HashMap::new();
    // Set once a send has failed (channel full — the consumer fell
    // behind). While set, ordinary events are discarded instead of
    // attempted (there's no point queueing more when the one thing that
    // actually matters, "tell the consumer to re-list", hasn't gotten
    // through yet) — cleared the moment an `Overflow` notice is
    // successfully delivered.
    let mut overflowed = false;

    loop {
        let next_deadline = pending_moves.values().map(|&(_, at)| at).min();

        let next = match next_deadline {
            None => events.next().await,
            Some(deadline) => match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    // At least one pending `MOVED_FROM`'s window closed
                    // with no `MOVED_TO` partner. Only the ones actually
                    // past their deadline are flushed — a different
                    // pending rename with a later deadline keeps waiting.
                    let now = TokioInstant::now();
                    let expired: Vec<OsString> = pending_moves
                        .iter()
                        .filter(|&(_, &(_, at))| at <= now)
                        .map(|(_, (name, _))| name.clone())
                        .collect();
                    pending_moves.retain(|_, &mut (_, at)| at > now);
                    for name in expired {
                        send_or_mark_overflow(&mut tx, &mut overflowed, DirEvent::Removed(name));
                    }
                    continue;
                }
            },
        };

        let Some(result) = next else {
            // The underlying fd closed (the watch was torn down, or the
            // `Inotify` instance was dropped) — nothing more will arrive.
            break;
        };

        let event = match result {
            Ok(event) => event,
            Err(err) => {
                eprintln!("saola-files: inotify read error: {err}");
                continue;
            }
        };

        if event.mask.contains(EventMask::Q_OVERFLOW) {
            // The kernel dropped events we can't identify — any rename
            // we're mid-pairing on is now unrecoverable too.
            pending_moves.clear();
            send_or_mark_overflow(&mut tx, &mut overflowed, DirEvent::Overflow);
            continue;
        }
        if event.mask.contains(EventMask::IGNORED) {
            // The watch itself is gone (directory deleted, filesystem
            // unmounted) — no further events will arrive for it.
            break;
        }

        // Events on the watched directory itself (self-moved, attrib
        // change on the dir) carry no `name` and aren't a row in this
        // directory's listing — nothing for the view to apply.
        let Some(name) = event.name else {
            continue;
        };

        if event.mask.contains(EventMask::MOVED_FROM) {
            pending_moves.insert(
                event.cookie,
                (name, TokioInstant::now() + RENAME_PAIR_WINDOW),
            );
            continue;
        }
        if event.mask.contains(EventMask::MOVED_TO) {
            let dir_event = match pending_moves.remove(&event.cookie) {
                Some((from, _)) => DirEvent::Renamed { from, to: name },
                // No pending `MOVED_FROM` with this cookie — moved in from
                // outside the watched directory, so there's nothing to
                // pair with; it's a plain creation.
                None => DirEvent::Created(name),
            };
            send_or_mark_overflow(&mut tx, &mut overflowed, dir_event);
            continue;
        }
        if event.mask.contains(EventMask::CREATE) {
            send_or_mark_overflow(&mut tx, &mut overflowed, DirEvent::Created(name));
            continue;
        }
        if event.mask.contains(EventMask::DELETE) {
            send_or_mark_overflow(&mut tx, &mut overflowed, DirEvent::Removed(name));
            continue;
        }
        if event
            .mask
            .intersects(EventMask::MODIFY | EventMask::ATTRIB | EventMask::CLOSE_WRITE)
        {
            send_or_mark_overflow(&mut tx, &mut overflowed, DirEvent::Changed(name));
        }
    }
}

/// `try_send`s `event`, per CLAUDE.md's async-bridging rule (bounded
/// channel, never a blocking send inside a message-producing path). A
/// full channel means the consumer fell behind; rather than buffering
/// unboundedly or silently dropping the specific change forever, this
/// escalates to `overflowed = true` so the *next* successful send is a
/// single [`DirEvent::Overflow`] instead of `event` — the view will do a
/// full re-list off that, which supersedes whatever this one event would
/// have told it anyway.
fn send_or_mark_overflow(tx: &mut mpsc::Sender<DirEvent>, overflowed: &mut bool, event: DirEvent) {
    if *overflowed {
        if tx.try_send(DirEvent::Overflow).is_ok() {
            *overflowed = false;
        }
        return;
    }
    if tx.try_send(event).is_err() {
        *overflowed = true;
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

    #[tokio::test]
    async fn watch_on_a_missing_directory_returns_none() {
        // `Watches::add` fails fast (ENOENT) rather than handing back a
        // stream that would just sit there never producing anything —
        // same "degrade to None" posture as any other unavailable
        // capability.
        let missing = Location::local("/nonexistent/saola-files-test-dir");
        assert!(backend().watch(&missing).is_none());
    }

    #[test]
    fn caps_claim_watch_and_trash() {
        // Capability-honest: `watch()` genuinely can signal changes (see
        // the temp-dir tests below), and as of Stage 9 the UI's delete
        // handling genuinely does route local deletes through
        // `core::fs::trash` when this bit is set — see `caps()`'s own doc
        // comment for why claiming it is no longer the lie it would have
        // been before that module existed.
        let caps = backend().caps();
        assert!(caps.contains(Caps::WATCH));
        assert!(caps.contains(Caps::TRASH));
        assert!(caps.contains(Caps::LOCAL_PATH));
    }

    // ── Stage 5: inotify watch ───────────────────────────────────────────
    //
    // Every test below performs a *real* filesystem mutation on a temp
    // directory and asserts the resulting `DirEvent` appears on the
    // returned stream — the "external create/rm/mv appears" integration
    // test the stage calls for, run directly against `LocalBackend::watch`
    // (the layer that actually owns inotify) rather than through the full
    // UI subscription stack, which needs a live iced runtime to drive.

    /// Bounded wait for the stream's next item. A real inotify event
    /// should land in well under this on any sane system (the stage's
    /// manual "≤100ms" criterion is checked live, not by this timeout) —
    /// a genuine hang here is a bug, not flakiness to paper over with a
    /// longer wait.
    async fn next_event(stream: &mut BoxStream<'static, DirEvent>) -> DirEvent {
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for a watch event")
            .expect("watch stream ended unexpectedly")
    }

    #[tokio::test]
    async fn watch_reports_an_external_create() {
        let dir = tempdir();
        let mut stream = backend().watch(&Location::local(&dir)).unwrap();

        std::fs::write(dir.join("new.txt"), b"hi").unwrap();

        assert_eq!(
            next_event(&mut stream).await,
            DirEvent::Created(OsString::from("new.txt"))
        );

        cleanup(dir);
    }

    #[tokio::test]
    async fn watch_reports_an_external_remove() {
        let dir = tempdir();
        std::fs::write(dir.join("gone.txt"), b"hi").unwrap();
        let mut stream = backend().watch(&Location::local(&dir)).unwrap();

        std::fs::remove_file(dir.join("gone.txt")).unwrap();

        assert_eq!(
            next_event(&mut stream).await,
            DirEvent::Removed(OsString::from("gone.txt"))
        );

        cleanup(dir);
    }

    #[tokio::test]
    async fn watch_reports_an_external_rename_as_one_paired_event() {
        let dir = tempdir();
        std::fs::write(dir.join("old.txt"), b"hi").unwrap();
        let mut stream = backend().watch(&Location::local(&dir)).unwrap();

        std::fs::rename(dir.join("old.txt"), dir.join("new.txt")).unwrap();

        assert_eq!(
            next_event(&mut stream).await,
            DirEvent::Renamed {
                from: OsString::from("old.txt"),
                to: OsString::from("new.txt"),
            }
        );

        cleanup(dir);
    }

    #[tokio::test]
    async fn watch_reports_a_move_out_as_a_removal_once_the_pairing_window_closes() {
        let dir = tempdir();
        let outside = tempdir();
        std::fs::write(dir.join("leaving.txt"), b"hi").unwrap();
        let mut stream = backend().watch(&Location::local(&dir)).unwrap();

        // Moves to a directory we don't watch — `dir`'s watch only ever
        // sees the `MOVED_FROM` half, never a matching `MOVED_TO`.
        std::fs::rename(dir.join("leaving.txt"), outside.join("leaving.txt")).unwrap();

        // Give `RENAME_PAIR_WINDOW` (50ms) room to close before falling
        // back to the plain-`Removed` gotcha this test exists to prove —
        // still well inside the 2s bound `next_event` otherwise uses.
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("timed out waiting for the pairing window to close")
            .expect("watch stream ended unexpectedly");
        assert_eq!(event, DirEvent::Removed(OsString::from("leaving.txt")));

        cleanup(dir);
        cleanup(outside);
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

    // ── symlink / set_permissions / statfs ───────────────────────────────

    #[tokio::test]
    async fn caps_claim_links_permissions_and_space() {
        let caps = backend().caps();
        assert!(caps.contains(Caps::SYMLINK));
        assert!(caps.contains(Caps::SET_PERMISSIONS));
        assert!(caps.contains(Caps::STATFS));
    }

    #[tokio::test]
    async fn symlink_target_bytes_round_trip_even_when_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir();
        let link = dir.join("link");
        let target = std::path::Path::new(std::ffi::OsStr::from_bytes(b"gone-\xff\xfe.txt"));
        // The target need not exist: a dangling link is still a link.
        backend()
            .symlink(target, &Location::local(&link))
            .await
            .unwrap();
        let stored = std::fs::read_link(&link).unwrap();
        assert_eq!(stored.as_os_str().as_bytes(), target.as_os_str().as_bytes());
        cleanup(dir);
    }

    #[tokio::test]
    async fn relative_symlink_target_is_stored_verbatim() {
        let dir = tempdir();
        let link = dir.join("rel");
        backend()
            .symlink(Path::new("../sibling/x"), &Location::local(&link))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../sibling/x")
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn set_permissions_on_a_symlink_changes_the_target() {
        let dir = tempdir();
        let file = dir.join("real");
        let link = dir.join("link");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        backend()
            .set_permissions(&Location::local(&link), 0o600)
            .await
            .unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o600);
        cleanup(dir);
    }

    #[tokio::test]
    async fn symlink_over_an_existing_name_is_already_exists() {
        let dir = tempdir();
        let link = dir.join("taken");
        std::fs::write(&link, b"x").unwrap();
        let err = backend()
            .symlink(std::path::Path::new("whatever"), &Location::local(&link))
            .await
            .unwrap_err();
        assert!(matches!(err, VfsError::AlreadyExists { .. }), "{err:?}");
        cleanup(dir);
    }

    #[tokio::test]
    async fn set_permissions_applies_the_mode() {
        let dir = tempdir();
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        backend()
            .set_permissions(&Location::local(&file), 0o640)
            .await
            .unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o640);
        // File-type bits in the argument (as copied from `st_mode`) are
        // ignored rather than rejected.
        backend()
            .set_permissions(&Location::local(&file), 0o100_600)
            .await
            .unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o600);
        cleanup(dir);
    }

    #[tokio::test]
    async fn set_permissions_on_a_missing_file_is_not_found() {
        let dir = tempdir();
        let err = backend()
            .set_permissions(&Location::local(dir.join("nope")), 0o600)
            .await
            .unwrap_err();
        assert!(matches!(err, VfsError::NotFound { .. }), "{err:?}");
        cleanup(dir);
    }

    #[tokio::test]
    async fn statfs_reports_plausible_numbers_for_a_temp_dir() {
        let dir = tempdir();
        let stats = backend().statfs(&Location::local(&dir)).await.unwrap();
        assert!(stats.total > 0);
        assert!(stats.free <= stats.total);
        assert!(stats.available <= stats.free);
        cleanup(dir);
    }

    #[tokio::test]
    async fn statfs_on_a_missing_path_is_not_found() {
        let dir = tempdir();
        let err = backend()
            .statfs(&Location::local(dir.join("nope")))
            .await
            .unwrap_err();
        assert!(matches!(err, VfsError::NotFound { .. }), "{err:?}");
        cleanup(dir);
    }

    // ── tiny temp-dir helpers (no `tempfile` dependency for this alone) ──

    /// A fresh, empty directory for one test. The per-process counter keeps
    /// two parallel tests that read the same nanosecond from sharing a
    /// directory (the cause of issue #106; see `core::fs::ops`'s helper).
    fn tempdir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-test-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir(&dir).unwrap();
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

    // ── a failed write must surface on close (durable-on-close) ─────────

    /// A writer that accepts `ok` writes, then fails like a full disk.
    struct FailsAfter {
        ok: usize,
    }

    impl std::io::Write for FailsAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.ok == 0 {
                return Err(io::Error::from_raw_os_error(28)); // ENOSPC
            }
            self.ok -= 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SyncWrite for FailsAfter {
        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SyncWrite for &mut Vec<u8> {
        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Accepts every write; the flush to stable storage fails (EIO), like a
    /// disk that drops the data on its way down.
    struct SyncFails {
        written: Vec<u8>,
        synced: bool,
    }

    impl std::io::Write for SyncFails {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SyncWrite for SyncFails {
        fn sync(&mut self) -> io::Result<()> {
            self.synced = true;
            Err(io::Error::from_raw_os_error(5)) // EIO
        }
    }

    #[test]
    fn a_failed_sync_is_a_failed_write_when_sync_is_asked_for() {
        let (mut tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        tx.try_send(vec![1, 2]).expect("send");
        drop(tx);
        let mut writer = SyncFails {
            written: Vec::new(),
            synced: false,
        };
        let err = write_chunks(&mut writer, &mut rx, true).expect_err("sync failure must surface");
        assert_eq!(err.raw_os_error(), Some(5));
        assert_eq!(writer.written, vec![1, 2]);
    }

    #[test]
    fn sync_is_skipped_when_not_asked_for() {
        let (mut tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        tx.try_send(vec![9]).expect("send");
        drop(tx);
        let mut writer = SyncFails {
            written: Vec::new(),
            synced: false,
        };
        write_chunks(&mut writer, &mut rx, false).expect("no sync, no failure");
        assert!(!writer.synced);
    }

    impl SyncWrite for &mut SyncFails {
        fn sync(&mut self) -> io::Result<()> {
            (**self).sync()
        }
    }

    #[tokio::test]
    async fn create_new_refuses_an_existing_file_and_leaves_it_alone() {
        let dir = tempdir();
        let path = dir.join("taken");
        std::fs::write(&path, b"precious").unwrap();
        let options = WriteOptions {
            create_new: true,
            sync: false,
            owner_only: false,
        };
        let result = backend().write_with(&Location::local(&path), options).await;
        assert!(matches!(result, Err(VfsError::AlreadyExists { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");
        cleanup(dir);
    }

    #[tokio::test]
    async fn create_new_never_writes_through_a_symlink() {
        let dir = tempdir();
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::write(&target, b"precious").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let options = WriteOptions {
            create_new: true,
            sync: true,
            owner_only: false,
        };
        let result = backend().write_with(&Location::local(&link), options).await;
        assert!(matches!(result, Err(VfsError::AlreadyExists { .. })));
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        // A dangling link is refused too, and its target is not created.
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
        assert!(
            backend()
                .write_with(&Location::local(&dangling), options)
                .await
                .is_err()
        );
        assert!(!dir.join("nowhere").exists());
        cleanup(dir);
    }

    #[tokio::test]
    async fn create_new_with_sync_writes_the_whole_file_by_close() {
        let dir = tempdir();
        let path = dir.join("fresh");
        let options = WriteOptions {
            create_new: true,
            sync: true,
            owner_only: false,
        };
        let mut sink = backend()
            .write_with(&Location::local(&path), options)
            .await
            .unwrap();
        sink.send(vec![1, 2, 3]).await.unwrap();
        sink.send(vec![4]).await.unwrap();
        sink.close().await.unwrap();
        // No polling: `close` resolved, so the bytes are there.
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3, 4]);
        cleanup(dir);
    }

    #[tokio::test]
    async fn same_file_sees_hard_links_symlinks_and_symlinked_folders() {
        let dir = tempdir();
        let file = dir.join("file");
        let other = dir.join("other");
        std::fs::write(&file, b"a").unwrap();
        std::fs::write(&other, b"a").unwrap();
        std::fs::hard_link(&file, dir.join("hard")).unwrap();
        std::os::unix::fs::symlink(&file, dir.join("soft")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("via")).unwrap();
        let at = |name: &str| Location::local(dir.join(name));
        let b = backend();
        assert_eq!(b.same_file(&at("file"), &at("file")).await, Ok(true));
        assert_eq!(b.same_file(&at("file"), &at("hard")).await, Ok(true));
        assert_eq!(b.same_file(&at("file"), &at("soft")).await, Ok(true));
        assert_eq!(b.same_file(&at("file"), &at("via/file")).await, Ok(true));
        assert_eq!(b.same_file(&at("file"), &at("other")).await, Ok(false));
        // Missing on one side: not the same, and no error.
        assert_eq!(b.same_file(&at("file"), &at("missing")).await, Ok(false));
        // A dangling symlink can't be the file either.
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        assert_eq!(b.same_file(&at("file"), &at("dangling")).await, Ok(false));
        // A symlink loop can't be examined: an error, not a guess.
        std::os::unix::fs::symlink(dir.join("loop"), dir.join("loop")).unwrap();
        assert!(b.same_file(&at("file"), &at("loop")).await.is_err());
        cleanup(dir);
    }

    #[tokio::test]
    async fn a_write_that_fails_part_way_is_reported_by_close() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let join =
            tokio::task::spawn_blocking(move || write_chunks(FailsAfter { ok: 1 }, &mut rx, false));
        let mut sink = WriterSink {
            tx,
            join: Some(join),
            location: Location::local("/full"),
        };
        // The first chunk is written; the second fails on the thread. Later
        // sends may or may not notice; `close` must.
        let _ = sink.send(vec![1]).await;
        let _ = sink.send(vec![2]).await;
        let _ = sink.send(vec![3]).await;
        let err = sink
            .close()
            .await
            .expect_err("close must report the failure");
        assert!(format!("{err:?}").contains("28"), "{err:?}");
    }

    #[tokio::test]
    async fn writing_to_a_full_device_does_not_close_successfully() {
        // /dev/full accepts open() and fails every write with ENOSPC.
        let mut sink = backend()
            .write(&Location::local("/dev/full"))
            .await
            .expect("open /dev/full");
        let _ = sink.send(vec![0u8; 4096]).await;
        assert!(sink.close().await.is_err());
    }

    #[test]
    fn chunks_are_written_in_order_and_flushed_on_success() {
        let (mut tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        tx.try_send(vec![1, 2]).expect("send");
        tx.try_send(vec![3]).expect("send");
        drop(tx);
        let mut out = Vec::new();
        write_chunks(&mut out, &mut rx, true).expect("ok");
        assert_eq!(out, vec![1, 2, 3]);
    }
}
