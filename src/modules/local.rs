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
use crate::core::vfs::{Backend, Caps, DirEvent, Location, ReadStream, VfsError, WriteSink};

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
    join: Option<JoinHandle<()>>,
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
                if let Err(err) = result {
                    // The blocking task panicked — the no-panic rule
                    // extends to background work (see `run_blocking`'s
                    // identical posture for the read/list/mkdir side).
                    return Poll::Ready(Err(VfsError::Other {
                        message: format!("internal error writing {}: {err}", self.location),
                    }));
                }
                Poll::Ready(Ok(()))
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
        // `WATCH` as of Stage 5 (`watch()` below is real inotify). Still no
        // `TRASH` (`remove()` is a real permanent delete — claiming trash
        // here would be a capability lie the UI would word wrong).
        // `SET_PERMISSIONS` has no backing trait method at all yet.
        Caps::WATCH | Caps::RENAME_IN_PLACE | Caps::LOCAL_PATH | Caps::THUMBNAILS
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
        let join = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = file;
            while let Some(chunk) = futures::executor::block_on(rx.next()) {
                if let Err(err) = file.write_all(&chunk) {
                    eprintln!("saola-files: write to {loc} failed: {err}");
                    break;
                }
            }
        });

        Ok(Box::pin(WriterSink {
            tx,
            join: Some(join),
            location: location.clone(),
        }))
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
    fn caps_claim_watch_now_but_still_not_trash() {
        // Capability-honest: `remove()` really does permanently delete (no
        // trash dir involved) — claiming that bit would be a lie the UI
        // would word wrong. `watch()` genuinely can signal changes now
        // (see the temp-dir tests below), so claiming `WATCH` is not.
        let caps = backend().caps();
        assert!(caps.contains(Caps::WATCH));
        assert!(!caps.contains(Caps::TRASH));
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
