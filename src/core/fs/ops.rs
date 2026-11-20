//! The async copy/move op engine (Stage 8) and the in-app clipboard it
//! serves — both live in one file because a paste is exactly "read the
//! clipboard, submit an op", and the two have no other consumers to
//! justify a second module boundary yet.
//!
//! **Engine shape.** [`run`] is the whole entry point: synchronous, like
//! [`crate::core::vfs::Backend::watch`] — it spawns a detached tokio task
//! (`execute`) and returns the event side of a bounded
//! `futures::channel::mpsc` immediately. `core/` stays iced-free (CLAUDE.md),
//! so this returns a plain `BoxStream<'static, OpEvent>`, the same shape
//! `Backend::watch`/`core::udisks::MountsSource::watch` already established;
//! wrapping that into an `iced::Subscription` is `ui::dialogs::progress`'s
//! job, one layer up (mirrors `ui::dirview::watch`/`ui::sidebar`'s
//! `mounts_stream`).
//!
//! **Cancel.** [`OpRequest`] carries an `Arc<AtomicBool>` the caller flips
//! via [`OpRequest::cancel`]; `execute`'s copy loop checks it on every raw
//! chunk read from the source (not just every re-chunked write), so a
//! cancel lands within one source-backend chunk of being requested, not
//! one whole file. `OpRequest` implements `Hash` by `id` alone (see its
//! doc comment) so it can serve as a `Subscription::run_with` identity —
//! the `Arc<AtomicBool>` has no meaningful hash and the `id` alone is
//! already a stronger identity than the rest of the fields combined.
//!
//! **Conflict prompts** use the capacity-1 reply-channel pattern CLAUDE.md
//! names (capture's `DaemonEvent::BeginRegion`/`RegionOutcome` — see that
//! file's doc comment): `execute` builds a fresh one-shot-shaped
//! `mpsc::channel(1)` per conflict, sends the receiving half's `Sender` out
//! on the *same* bounded event channel as an `OpEvent::Conflict { reply,
//! .. }`, then `.await`s the reply. The UI (`ui::dialogs::conflict`) stores
//! that `Sender` in app state the moment the event arrives and
//! `try_send`s the human's choice back down it once they click a button —
//! exactly the shape `main.rs::App::handle_directory_event`'s sibling
//! `overlay_reply` field takes in capture, just renamed for this domain.
//!
//! **Same-backend rename fast path.** A `Move` whose source and
//! destination share a backend (`scheme` + `authority`) and whose backend
//! claims `Caps::RENAME_IN_PLACE` tries `Backend::rename` directly, no
//! streaming at all. Failure (which includes the classic cross-filesystem
//! `EXDEV`, surfaced by the backend as an ordinary [`VfsError`] like any
//! other I/O failure) falls through to the generic copy-then-delete path
//! uniformly — this *is* "replaces EXDEV special-casing" (PLAN.md's own
//! wording): nothing here inspects an `io::ErrorKind` to decide, the
//! engine just tries the fast path and accepts whatever the copy path
//! would have done anyway if it doesn't work out.
//!
//! **Deletes and trashing** ([`OpKind::Trash`], [`OpKind::Delete`]) run
//! through the same engine, so they get the same progress strip, Cancel and
//! failure reporting as copy and move. They have no destination: the
//! request is built with [`OpRequest::removal`]. See `execute_removal`.
//!
//! - *Trash* moves each top-level source into the freedesktop trash with one
//!   `rename`, announces it with [`OpEvent::Trashed`] (the app turns that
//!   into the same undo entry the old fire-and-forget path pushed), and
//!   counts one item per source.
//! - *Delete* walks the tree depth-first, removing children before their
//!   directory, one `Backend::remove` per item. That is what makes a
//!   non-empty remote folder deletable (SFTP `rmdir` refuses a directory
//!   with contents), and it is why progress can count items. A symlink is
//!   an item: it is unlinked, never followed, so a link to a directory
//!   deletes the link and leaves the directory alone.
//! - A failing item is recorded and the walk carries on; a directory whose
//!   contents did not all go is left alone (its error is already recorded).
//!   Cancel stops before the next item and reports nothing as failed.
//!
//! **What this stage does not build.** No queue of multiple concurrent
//! ops — `ui::dialogs::progress`'s "ops strip" shows at most one running
//! op, and `App` tracks `active_op: Option<OpRequest>`. No undo — Stage 10
//! (see the Stage 8 handoff's notes for what it needs to hook here). No
//! foreign-clipboard interop (reading/writing
//! `x-special/gnome-copied-files`-style data another app put on the
//! desktop clipboard) — also Stage 10, per PLAN.md's own wording; `Clipboard`
//! here is the internal `{op, locations}` model CLAUDE.md calls
//! authoritative.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::sink::SinkExt;
use futures::stream::{BoxStream, StreamExt};

use crate::core::fs::entry::EntryKind;
use crate::core::fs::trash::{self, TrashId};
use crate::core::vfs::{Backend, Caps, Location, VfsError};

/// Chunk size the engine re-batches a backend's own read-stream chunks
/// into before handing them to the destination's write sink — PLAN.md's
/// explicit "1 MiB chunks", independent of whatever chunk size the source
/// backend's `read()` happens to produce (`modules::local`'s is 64 KiB).
const COPY_CHUNK_BYTES: usize = 1024 * 1024;

/// Bound on the engine's own progress/conflict event bridge — CLAUDE.md's
/// bounded-channel rule, same posture as `modules::local`'s
/// `WATCH_CHANNEL_CAPACITY`.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Identifies one submitted op. Newtype over `u64` rather than a bare
/// integer purely so `OpRequest`'s `Hash` impl (and any future map keyed by
/// this) reads as "an op id", not "some number".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(u64);

/// Monotonic [`OpId`] allocator — `App` owns exactly one, the same "one
/// shared counter, never per-view" posture as `mime_db`/`apps_db`.
#[derive(Debug, Default)]
pub struct OpIdSource(u64);

impl OpIdSource {
    /// Named `alloc`, not `next` — clippy (rightly) flags `next` on a type
    /// that isn't an `Iterator` as easy to confuse with one.
    pub fn alloc(&mut self) -> OpId {
        self.0 += 1;
        OpId(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Move,
    /// Move each source into the trash. Local items only (`Caps::TRASH`).
    Trash,
    /// Remove each source and everything under it, for good.
    Delete,
}

/// One submitted copy/move job: `sources` (top-level items only — trees are
/// walked internally) into `dest_dir`. `Clone` so the UI can hand a
/// `&OpRequest` to both `ui::dialogs::progress::subscription` (which needs
/// an owned copy for `Subscription::run_with`'s `Data`) and keep its own
/// copy in `App` state to expose `Self::cancel` from a Cancel button.
#[derive(Clone)]
pub struct OpRequest {
    pub id: OpId,
    pub kind: OpKind,
    pub sources: Vec<Location>,
    /// Where copies and moves land. For [`OpKind::Trash`] and
    /// [`OpKind::Delete`] there is no destination; [`OpRequest::removal`]
    /// puts the folder the first source lives in here, and nothing reads it.
    pub dest_dir: Location,
    cancel: Arc<AtomicBool>,
    /// Which trash [`OpKind::Trash`] moves into. `None` means the user's
    /// real trash, resolved from the environment by `core::fs::trash`;
    /// tests pass a temp directory so they never touch it.
    trash_root: Option<PathBuf>,
}

impl OpRequest {
    pub fn new(id: OpId, kind: OpKind, sources: Vec<Location>, dest_dir: Location) -> Self {
        OpRequest {
            id,
            kind,
            sources,
            dest_dir,
            cancel: Arc::new(AtomicBool::new(false)),
            trash_root: None,
        }
    }

    /// A [`OpKind::Trash`] or [`OpKind::Delete`] request: no destination.
    pub fn removal(id: OpId, kind: OpKind, sources: Vec<Location>) -> Self {
        let dest_dir = sources
            .first()
            .and_then(Location::parent)
            .or_else(|| sources.first().cloned())
            .unwrap_or_else(|| Location::local("/"));
        OpRequest::new(id, kind, sources, dest_dir)
    }

    /// Trash into `root` (a freedesktop trash directory) instead of the
    /// user's own. Argument-taking so tests never read the environment.
    pub fn with_trash_root(mut self, root: PathBuf) -> Self {
        self.trash_root = Some(root);
        self
    }

    /// True for the kinds that remove their sources rather than write a
    /// destination.
    pub fn is_removal(&self) -> bool {
        matches!(self.kind, OpKind::Trash | OpKind::Delete)
    }

    /// Requests cancellation. Fire-and-forget: the engine notices on its
    /// next chunk-boundary check and reports back via `OpEvent::Cancelled`
    /// on its own event stream — there is nothing to await here.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Manual `Hash`, by `id` alone. `Subscription::run_with` requires `Data:
/// Hash` to tell subscriptions apart across re-renders (see
/// `ui::dialogs::progress::subscription`'s doc comment) — `id` is already a
/// stronger identity than `sources`/`dest_dir`/`kind` combined (two
/// identical-looking copies started back to back must still count as two
/// different subscriptions), and `Arc<AtomicBool>` has no meaningful hash
/// to contribute anyway.
impl std::hash::Hash for OpRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// A file/dir a copy or move is about to overwrite. `dest` already exists;
/// `source` is what's trying to land there.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub source: Location,
    pub dest: Location,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    /// Keep both: copy to a uniquified name instead of `dest`
    /// (`unique_rename_dest`).
    RenameCopy,
}

/// What comes back down a [`OpEvent::Conflict`]'s `reply` channel.
#[derive(Debug, Clone, Copy)]
pub struct ConflictDecision {
    pub choice: ConflictChoice,
    /// Apply `choice` to every remaining conflict in this op without
    /// prompting again.
    pub apply_to_all: bool,
}

/// One thing the engine tells the UI about a running op. `Clone` so
/// [`send_event`]'s full-channel retry loop can re-offer the same value.
#[derive(Debug, Clone)]
pub enum OpEvent {
    /// The totals a progress bar needs — from a best-effort pre-scan (see
    /// `count_totals`'s doc comment on why it's allowed to undercount).
    Started {
        files_total: usize,
        bytes_total: u64,
    },
    /// A file's stream copy is about to start.
    FileStarted { name: OsString },
    /// Cumulative totals so far — the UI replaces its whole progress
    /// readout with this, never accumulates deltas itself.
    Progress { files_done: usize, bytes_done: u64 },
    /// A trash op moved `original` into the trash; `id` is what restores it.
    /// The app turns this into an undo entry.
    Trashed { original: Location, id: TrashId },
    /// `dest` already exists; the UI must answer via `reply` before the
    /// engine proceeds past this item. `reply` is a fresh capacity-1
    /// channel per conflict (CLAUDE.md's blocking-prompt pattern) — see the
    /// module doc comment.
    Conflict {
        conflict: Conflict,
        reply: mpsc::Sender<ConflictDecision>,
    },
    /// The op ran to completion (possibly with per-item errors, e.g. one
    /// unreadable file in a large tree) rather than being cancelled.
    Finished {
        errors: Vec<(Location, VfsError)>,
        /// Failures beyond `errors` (a removal keeps only the first few
        /// in full); counted so the notice headline stays right.
        omitted: usize,
    },
    /// The op stopped early because `OpRequest::request_cancel` was called,
    /// or because nobody was listening on a *must-deliver* event anymore
    /// (the progress UI was torn down mid-conflict-prompt — see
    /// `send_event`'s doc comment) and continuing unattended would be
    /// worse than stopping.
    Cancelled,
}

/// Runs `request` and returns its event stream. Spawns a detached tokio
/// task immediately (like `Backend::watch`, this function itself never
/// `.await`s) — the returned stream is what governs how long the caller
/// cares to keep listening; dropping it doesn't stop the underlying copy
/// (there is no way to un-write bytes already in flight), only the
/// `request.cancel` flag does that, checked at the next chunk boundary.
pub fn run(request: &OpRequest) -> BoxStream<'static, OpEvent> {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    tokio::spawn(execute(request.clone(), tx));
    rx.boxed()
}

// ── The in-app clipboard ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    Copy,
    Cut,
}

/// The file manager's own internal clipboard — CLAUDE.md: "internal `{op:
/// Copy|Cut, locations}` is authoritative". This is *not* the desktop
/// clipboard (no `wl-clipboard`/MIME-type interop): reading/writing a
/// foreign app's `x-special/gnome-copied-files`-shaped clipboard data is
/// Stage 10's job, deliberately deferred — see the Stage 8 handoff.
#[derive(Debug, Clone, Default)]
pub struct Clipboard {
    op: Option<ClipboardOp>,
    locations: Vec<Location>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_copy(&mut self, locations: Vec<Location>) {
        self.op = Some(ClipboardOp::Copy);
        self.locations = locations;
    }

    pub fn set_cut(&mut self, locations: Vec<Location>) {
        self.op = Some(ClipboardOp::Cut);
        self.locations = locations;
    }

    pub fn op(&self) -> Option<ClipboardOp> {
        self.op
    }

    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    pub fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }

    /// After a Cut's contents are handed off to a submitted move op — a
    /// second, later paste of the same "cut" selection would be pasting
    /// files that (once the move finishes) no longer exist at their old
    /// location, which is confusing, not useful. A Copy's clipboard is
    /// deliberately *not* cleared anywhere (there is no call to this after
    /// a copy paste): pasting the same copied selection repeatedly is
    /// ordinary, expected behavior every mainstream file manager supports.
    pub fn clear(&mut self) {
        self.op = None;
        self.locations.clear();
    }
}

// ── Engine internals ────────────────────────────────────────────────────

fn same_backend(a: &Location, b: &Location) -> bool {
    a.scheme == b.scheme && a.authority == b.authority
}

async fn execute(request: OpRequest, mut tx: mpsc::Sender<OpEvent>) {
    if request.is_removal() {
        execute_removal(request, tx, &crate::modules::resolve).await;
        return;
    }
    let is_move = request.kind == OpKind::Move;

    // ── Fast path: same-backend rename, tried per top-level source ──────
    let mut remaining: Vec<Location> = Vec::with_capacity(request.sources.len());
    if is_move {
        for source in request.sources {
            let fast = same_backend(&source, &request.dest_dir)
                .then(|| crate::modules::resolve(&request.dest_dir))
                .flatten()
                .filter(|backend| backend.caps().contains(Caps::RENAME_IN_PLACE));
            let Some(backend) = fast else {
                remaining.push(source);
                continue;
            };
            let Some(name) = source.path.file_name().map(OsString::from) else {
                continue;
            };
            let dest = request.dest_dir.join(&name);
            // Any failure — cross-device EXDEV included — falls through to
            // the generic streaming path below rather than being
            // specially detected; see the module doc comment.
            if backend.rename(&source, &dest).await.is_err() {
                remaining.push(source);
            }
        }
    } else {
        remaining = request.sources;
    }

    if remaining.is_empty() {
        send_event(
            &mut tx,
            OpEvent::Finished {
                errors: Vec::new(),
                omitted: 0,
            },
        )
        .await;
        return;
    }

    let Some(dest_backend) = crate::modules::resolve(&request.dest_dir) else {
        let err = VfsError::Other {
            message: format!("no backend for scheme \"{}\"", request.dest_dir.scheme),
        };
        send_event(
            &mut tx,
            OpEvent::Finished {
                errors: vec![(request.dest_dir.clone(), err)],
                omitted: 0,
            },
        )
        .await;
        return;
    };

    let (files_total, bytes_total) = count_totals(&remaining).await;
    if !send_event(
        &mut tx,
        OpEvent::Started {
            files_total,
            bytes_total,
        },
    )
    .await
    {
        return; // nobody's listening — see send_event's doc comment
    }

    let mut ctx = ExecContext {
        cancel: request.cancel,
        tx,
        dest_backend,
        files_done: 0,
        bytes_done: 0,
        remembered: None,
        is_move,
        errors: Vec::new(),
        cancelled: false,
    };

    for source in remaining {
        if ctx.cancelled {
            break;
        }
        let Some(name) = source.path.file_name().map(OsString::from) else {
            continue;
        };
        let dest = request.dest_dir.join(&name);
        ctx.copy_item(source, dest).await;
    }

    if ctx.cancelled {
        send_event(&mut ctx.tx, OpEvent::Cancelled).await;
    } else {
        send_event(
            &mut ctx.tx,
            OpEvent::Finished {
                errors: ctx.errors,
                omitted: 0,
            },
        )
        .await;
    }
}

// ── Trash and delete ────────────────────────────────────────────────────

/// How many failures one removal keeps in full. The notice shows fewer
/// (`ui::dialogs::error_notice::MAX_DETAIL_LINES`); the surplus over that
/// is for stderr, which logs each kept one. Failures past this are only
/// counted ([`OpEvent::Finished`]'s `omitted`), so a directory of a million
/// undeletable files cannot grow memory, event clones or log lines without
/// bound.
const MAX_KEPT_FAILURES: usize = 32;

/// How deep the walk and the pre-scan will go. Both recurse through boxed
/// futures, and polling a chain of nested futures uses real stack: roughly
/// a few hundred bytes to a kilobyte per level in a debug build, on a tokio
/// worker thread whose stack is 2 MiB. 512 levels stays well inside that,
/// and is far deeper than real trees (a path is limited to 4096 bytes, and
/// even `node_modules` rarely passes 50 levels). A hostile server serving
/// an endless `a/a/a/...` chain must hit a worded failure, not a stack
/// overflow that aborts the process.
const MAX_WALK_DEPTH: usize = 512;

/// True for a directory-entry name that must never be joined onto a path:
/// empty (re-enters the same directory forever), `.` / `..` (escape or
/// loop), or containing a `/` (an absolute name makes `Path::join` replace
/// the whole path; `a/../..` climbs out of the tree). A well-behaved
/// backend never produces these; a hostile or buggy server can. Compared as
/// bytes, never as lossy text.
pub(crate) fn is_unsafe_entry_name(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/')
}

/// Mutable state of one trash/delete op. Separate from [`ExecContext`]
/// because a removal has no destination backend or conflicts.
struct RemovalState {
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<OpEvent>,
    items_done: usize,
    /// The first [`MAX_KEPT_FAILURES`] failures.
    errors: Vec<(Location, VfsError)>,
    /// Failures beyond those kept.
    omitted: usize,
    /// Set the moment cancel is seen (or nobody listens any more).
    cancelled: bool,
}

impl RemovalState {
    fn is_cancelled(&mut self) -> bool {
        if !self.cancelled && self.cancel.load(Ordering::Relaxed) {
            self.cancelled = true;
        }
        self.cancelled
    }

    /// Records one failed item, keeping only the first few in full.
    fn fail(&mut self, location: Location, err: VfsError) {
        if self.errors.len() < MAX_KEPT_FAILURES {
            self.errors.push((location, err));
        } else {
            self.omitted += 1;
        }
    }

    /// How many failures so far, kept or not. A directory compares this
    /// before and after its children to learn whether any of them survived.
    fn failure_count(&self) -> usize {
        self.errors.len() + self.omitted
    }

    /// Records an entry name [`is_unsafe_entry_name`] refused, against the
    /// directory that listed it (the name itself is not trustworthy as a
    /// path).
    fn fail_unsafe_name(&mut self, directory: &Location) {
        self.fail(
            directory.clone(),
            VfsError::Other {
                message: crate::tr!("error-unsafe-entry-name"),
            },
        );
    }

    /// One more item is gone: report cumulative progress. Items, not bytes:
    /// removing is not proportional to size.
    fn item_done(&mut self) {
        self.items_done += 1;
        try_send_event(
            &mut self.tx,
            OpEvent::Progress {
                files_done: self.items_done,
                bytes_done: 0,
            },
        );
    }

    fn started(&mut self, location: &Location) {
        try_send_event(
            &mut self.tx,
            OpEvent::FileStarted {
                name: location
                    .path
                    .file_name()
                    .map(OsString::from)
                    .unwrap_or_default(),
            },
        );
    }
}

/// Runs a trash or delete request. `resolve` finds the backend for a
/// location: production passes `modules::resolve`; tests pass a closure
/// that hands back an in-memory backend (the seam that lets the recursion
/// be tested without a real remote).
///
/// Teaching note: `R: Fn(..) + Sync` lets us borrow the closure across
/// `.await` points inside a task tokio may move between threads; sharing a
/// borrow across threads needs `Sync`.
///
/// Event order: `Started` is sent first with a total of 0 ("not known
/// yet", so the strip appears at once); for a delete, a second `Started`
/// follows when the pre-scan finishes (the app just replaces its progress
/// snapshot, and nothing has been removed yet). The scan honours cancel.
/// Cancelling with failures already recorded ends in `Finished` carrying
/// them (so they are not lost); cancelling with none ends in `Cancelled`.
async fn execute_removal<R>(request: OpRequest, tx: mpsc::Sender<OpEvent>, resolve: &R)
where
    R: Fn(&Location) -> Option<Box<dyn Backend>> + Sync,
{
    let mut state = RemovalState {
        cancel: request.cancel.clone(),
        tx,
        items_done: 0,
        errors: Vec::new(),
        omitted: 0,
        cancelled: false,
    };

    let first_total = if request.kind == OpKind::Trash {
        request.sources.len()
    } else {
        0
    };
    if !send_event(
        &mut state.tx,
        OpEvent::Started {
            files_total: first_total,
            bytes_total: 0,
        },
    )
    .await
    {
        return; // nobody is listening
    }

    // Delete pre-scans so the strip can say "N of M"; like `count_totals`
    // the scan may undercount, which only makes the bar read optimistic.
    if request.kind == OpKind::Delete {
        let mut total = 0usize;
        for source in &request.sources {
            if let Some(backend) = resolve(source) {
                total +=
                    count_removal_items(backend.as_ref(), source.clone(), None, &request.cancel, 0)
                        .await;
            }
        }
        if !state.is_cancelled()
            && !send_event(
                &mut state.tx,
                OpEvent::Started {
                    files_total: total,
                    bytes_total: 0,
                },
            )
            .await
        {
            return;
        }
    }

    for source in request.sources {
        if state.is_cancelled() {
            break;
        }
        let Some(backend) = resolve(&source) else {
            let message = crate::tr!("error-no-backend", scheme = source.scheme.as_str());
            state.fail(source, VfsError::Other { message });
            continue;
        };
        match request.kind {
            OpKind::Trash => {
                trash_source(
                    &mut state,
                    backend.as_ref(),
                    source,
                    request.trash_root.as_deref(),
                )
                .await
            }
            _ => remove_tree(&mut state, backend.as_ref(), source, None, 0).await,
        }
    }

    if state.cancelled && state.failure_count() == 0 {
        send_event(&mut state.tx, OpEvent::Cancelled).await;
    } else {
        let (errors, omitted) = (state.errors, state.omitted);
        send_event(&mut state.tx, OpEvent::Finished { errors, omitted }).await;
    }
}

/// Moves one source into the trash. Honest about capabilities: a backend
/// that cannot trash gets an error rather than a silent permanent delete,
/// and a trash that fails stays a failure: nothing here ever falls back to
/// deleting.
async fn trash_source(
    state: &mut RemovalState,
    backend: &dyn Backend,
    source: Location,
    trash_root: Option<&Path>,
) {
    state.started(&source);
    if !(source.is_local() && backend.caps().contains(Caps::TRASH)) {
        state.fail(
            source,
            VfsError::Unsupported {
                operation: crate::tr!("error-unsupported-trash"),
            },
        );
        return;
    }
    // The trash code is plain blocking filesystem work, so it runs on
    // tokio's blocking pool instead of stalling the async executor. The
    // closure must own its data (`'static`), hence the clones.
    let (path, root) = (source.path.clone(), trash_root.map(Path::to_path_buf));
    let outcome = tokio::task::spawn_blocking(move || match root {
        Some(root) => trash::trash_into(&path, &root),
        None => trash::trash(&path),
    })
    .await;
    match outcome {
        Ok(Ok(id)) => {
            state.item_done();
            let event = OpEvent::Trashed {
                original: source,
                id,
            };
            // Must-deliver: a missed event is a trashed item with no undo.
            if !send_event(&mut state.tx, event).await {
                state.cancelled = true;
            }
        }
        Ok(Err(err)) => state.fail(source, err),
        Err(join_err) => state.fail(
            source,
            VfsError::Other {
                message: crate::tr!("error-trash-internal", detail = join_err.to_string()),
            },
        ),
    }
}

/// True only for a real directory: `Backend::metadata` is `lstat`-like on
/// both backends (`modules::local` uses `symlink_metadata`, `modules::sftp`
/// the protocol's lstat), so a symlink reports `is_symlink` and is never
/// mistaken for the directory it points at.
fn is_real_directory(entry: &crate::core::fs::entry::FileEntry) -> bool {
    entry.kind == EntryKind::Directory && !entry.is_symlink
}

/// Removes `location` and everything under it, depth first. `listed_as_dir`
/// is what the parent's listing said (`None` for a top-level source): only a
/// hint. It is never trusted to descend, because a listing can be stale (the
/// entry was swapped for a symlink since) or come from a server that reports
/// followed attributes. Immediately before listing a directory, the walk
/// asks `backend.metadata` (lstat) and descends **only if that says real
/// directory, not symlink**; otherwise the entry is removed as a single
/// item (unlinked, if it is a link). That check is the whole defence against
/// following a link out of the tree.
///
/// Residual risk, stated plainly: every location is a full path, so the
/// check above protects only the instant it runs. If another process swaps
/// *any* directory inside the tree (not just the one being examined) for a
/// symlink while the walk is still working below it, every later lstat and
/// unlink under that path resolves through the link, and files outside the
/// tree can be removed. `main` had the same exposure before this engine
/// (`trash::delete_permanently` walked paths the same way). Closing it needs
/// fd-relative descent (`openat` / `unlinkat`), which the `Backend` trait
/// does not offer.
fn remove_tree<'a>(
    state: &'a mut RemovalState,
    backend: &'a dyn Backend,
    location: Location,
    listed_as_dir: Option<bool>,
    depth: usize,
) -> BoxFuture<'a, ()> {
    // Async fns cannot call themselves directly; boxing the future is the
    // standard workaround (same as `copy_item`).
    Box::pin(async move {
        if state.is_cancelled() {
            return;
        }
        // A parent that listed this as a non-directory needs no lookup: it
        // is removed as one item, and a backend's `remove` itself refuses a
        // non-empty directory rather than recursing.
        let descend = if listed_as_dir == Some(false) {
            false
        } else {
            match backend.metadata(&location).await {
                Ok(entry) => is_real_directory(&entry),
                Err(err) => {
                    state.fail(location, err);
                    return;
                }
            }
        };

        if descend {
            if depth >= MAX_WALK_DEPTH {
                // Leave this directory (and so every ancestor, which now
                // has a surviving child) alone, and say why.
                state.fail(
                    location,
                    VfsError::Other {
                        message: crate::tr!("error-too-deep"),
                    },
                );
                return;
            }
            let failures_before = state.failure_count();
            let children = match backend.list(&location).await {
                Ok(children) => children,
                Err(err) => {
                    state.fail(location, err);
                    return;
                }
            };
            for child in children {
                if state.is_cancelled() {
                    return;
                }
                if is_unsafe_entry_name(&child.name) {
                    state.fail_unsafe_name(&location);
                    continue;
                }
                let child_location = location.join(&child.name);
                let as_dir = child.kind == EntryKind::Directory;
                remove_tree(state, backend, child_location, Some(as_dir), depth + 1).await;
            }
            // If something inside could not go, removing the directory
            // would only fail again with a misleading "not empty".
            if state.is_cancelled() || state.failure_count() != failures_before {
                return;
            }
        }

        state.started(&location);
        match backend.remove(&location).await {
            Ok(()) => state.item_done(),
            Err(err) => state.fail(location, err),
        }
    })
}

/// Counts what [`remove_tree`] would remove (directories included), for the
/// progress total. Follows the same rules: descends only after an lstat
/// says real directory, skips unsafe names (the walk reports them), stops
/// at cancel, and ignores errors like [`count_one`].
fn count_removal_items<'a>(
    backend: &'a dyn Backend,
    location: Location,
    listed_as_dir: Option<bool>,
    cancel: &'a AtomicBool,
    depth: usize,
) -> BoxFuture<'a, usize> {
    Box::pin(async move {
        if cancel.load(Ordering::Relaxed) {
            return 0;
        }
        let descend = if listed_as_dir == Some(false) {
            false
        } else {
            match backend.metadata(&location).await {
                Ok(entry) => is_real_directory(&entry),
                Err(_) => return 0,
            }
        };
        let mut total = 1;
        // Past the limit the walk reports the directory as a failure, so
        // there is nothing more to count.
        if descend && depth < MAX_WALK_DEPTH {
            let Ok(children) = backend.list(&location).await else {
                return total;
            };
            for child in children {
                if is_unsafe_entry_name(&child.name) {
                    continue;
                }
                total += count_removal_items(
                    backend,
                    location.join(&child.name),
                    Some(child.kind == EntryKind::Directory),
                    cancel,
                    depth + 1,
                )
                .await;
            }
        }
        total
    })
}

/// Mutable state threaded through one op's recursive walk —
/// `ExecContext::copy_item` bundles what would otherwise be seven-plus
/// parameters into one `&mut self`.
struct ExecContext {
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<OpEvent>,
    dest_backend: Box<dyn Backend>,
    files_done: usize,
    bytes_done: u64,
    /// Set once a conflict's `apply_to_all` is chosen — every later
    /// conflict in this op reuses it without prompting again.
    remembered: Option<ConflictChoice>,
    is_move: bool,
    errors: Vec<(Location, VfsError)>,
    /// Set the moment `cancel` is observed (or a must-deliver event finds
    /// nobody listening) — every remaining recursive call becomes a no-op
    /// check-and-return rather than a second cancellation source.
    cancelled: bool,
}

impl ExecContext {
    fn is_cancelled(&mut self) -> bool {
        if self.cancelled {
            return true;
        }
        if self.cancel.load(Ordering::Relaxed) {
            self.cancelled = true;
        }
        self.cancelled
    }

    /// Copies (or, for a directory, recursively merges) `source` into
    /// `dest`. Boxed because async fns can't recurse directly — the
    /// standard "self-referential future" workaround, same shape
    /// `modules::local`'s own doc comments point at for similar cases.
    fn copy_item(&mut self, source: Location, dest: Location) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.is_cancelled() {
                return;
            }

            let Some(source_backend) = crate::modules::resolve(&source) else {
                self.errors.push((
                    source.clone(),
                    VfsError::Other {
                        message: format!("no backend for scheme \"{}\"", source.scheme),
                    },
                ));
                return;
            };

            let meta = match source_backend.metadata(&source).await {
                Ok(meta) => meta,
                Err(err) => {
                    self.errors.push((source, err));
                    return;
                }
            };

            if meta.kind == EntryKind::Directory {
                self.copy_dir(source, dest, source_backend.as_ref()).await;
            } else {
                let size = meta.size;
                self.copy_file(source_backend.as_ref(), source, dest, size)
                    .await;
            }
        })
    }

    async fn copy_dir(&mut self, source: Location, dest: Location, source_backend: &dyn Backend) {
        // An existing directory of the same name is a merge, never a
        // conflict — pasting a folder into a destination that already has
        // one by that name is ordinary, expected file-manager behavior.
        // Anything else already there (a file blocking a directory name)
        // is a genuine conflict.
        let existing_kind = self
            .dest_backend
            .metadata(&dest)
            .await
            .ok()
            .map(|entry| entry.kind);
        let final_dest = match existing_kind {
            Some(EntryKind::Directory) => dest,
            Some(_) => match self.resolve_conflict(&source, &dest).await {
                Some(resolved) => resolved,
                None => return, // skipped, or the reply channel was dropped (cancelled)
            },
            None => dest,
        };

        if self.dest_backend.metadata(&final_dest).await.is_err()
            && let Err(err) = self.dest_backend.mkdir(&final_dest).await
        {
            self.errors.push((final_dest, err));
            return;
        }

        let children = match source_backend.list(&source).await {
            Ok(children) => children,
            Err(err) => {
                self.errors.push((source, err));
                return;
            }
        };
        for child in children {
            if self.is_cancelled() {
                return;
            }
            let child_source = source.join(&child.name);
            let child_dest = final_dest.join(&child.name);
            self.copy_item(child_source, child_dest).await;
        }

        // Best-effort: `Backend::remove` on a non-empty directory (some
        // children were skipped by a conflict decision, so the source
        // tree isn't fully vacated) fails, which is recorded as a soft,
        // non-fatal error rather than aborting the op — the destination
        // side is already correct either way.
        if self.is_move
            && !self.cancelled
            && let Err(err) = source_backend.remove(&source).await
        {
            self.errors.push((source, err));
        }
    }

    async fn copy_file(
        &mut self,
        source_backend: &dyn Backend,
        source: Location,
        dest: Location,
        size: u64,
    ) {
        try_send_event(
            &mut self.tx,
            OpEvent::FileStarted {
                name: source
                    .path
                    .file_name()
                    .map(OsString::from)
                    .unwrap_or_default(),
            },
        );

        let dest_exists = self.dest_backend.metadata(&dest).await.is_ok();
        let final_dest = if dest_exists {
            match self.resolve_conflict(&source, &dest).await {
                Some(resolved) => resolved,
                None => {
                    // Skipped, or the reply channel was dropped (treated
                    // as a cancel by `resolve_conflict`, which already set
                    // `self.cancelled` in that case) — either way this
                    // file counts as "done" for progress purposes so a
                    // skip-heavy op still reaches 100%.
                    if !self.cancelled {
                        self.files_done += 1;
                        self.bytes_done += size;
                        self.emit_progress();
                    }
                    return;
                }
            }
        } else {
            dest
        };

        match copy_bytes(
            source_backend,
            self.dest_backend.as_ref(),
            &source,
            &final_dest,
            &self.cancel,
            &mut self.bytes_done,
            &mut self.tx,
        )
        .await
        {
            Ok(CopyOutcome::Done) => {
                self.files_done += 1;
                self.emit_progress();
                if self.is_move
                    && let Err(err) = source_backend.remove(&source).await
                {
                    self.errors.push((source, err));
                }
            }
            Ok(CopyOutcome::Cancelled) => {
                self.cancelled = true;
            }
            Err(err) => {
                self.errors.push((source, err));
            }
        }
    }

    /// Resolves one conflict: reuses a remembered `apply_to_all` choice if
    /// there is one, otherwise prompts (must-deliver — see `send_event`)
    /// and awaits the answer. Returns the destination to actually write to
    /// (`Some`), or `None` for Skip. A dropped reply channel (the progress
    /// UI was torn down mid-prompt) is treated as a cancel: there is no
    /// sane default to fall back to for "should this overwrite", so this
    /// sets `self.cancelled` and returns `None` rather than guessing.
    async fn resolve_conflict(&mut self, source: &Location, dest: &Location) -> Option<Location> {
        let choice = match self.remembered {
            Some(choice) => choice,
            None => {
                let (reply_tx, mut reply_rx) = mpsc::channel(1);
                let delivered = send_event(
                    &mut self.tx,
                    OpEvent::Conflict {
                        conflict: Conflict {
                            source: source.clone(),
                            dest: dest.clone(),
                        },
                        reply: reply_tx,
                    },
                )
                .await;
                if !delivered {
                    self.cancelled = true;
                    return None;
                }
                match reply_rx.next().await {
                    Some(decision) => {
                        if decision.apply_to_all {
                            self.remembered = Some(decision.choice);
                        }
                        decision.choice
                    }
                    None => {
                        self.cancelled = true;
                        return None;
                    }
                }
            }
        };

        match choice {
            ConflictChoice::Overwrite => Some(dest.clone()),
            ConflictChoice::Skip => None,
            ConflictChoice::RenameCopy => {
                Some(unique_rename_dest(self.dest_backend.as_ref(), dest).await)
            }
        }
    }

    fn emit_progress(&mut self) {
        try_send_event(
            &mut self.tx,
            OpEvent::Progress {
                files_done: self.files_done,
                bytes_done: self.bytes_done,
            },
        );
    }
}

enum CopyOutcome {
    Done,
    Cancelled,
}

/// Streams `source`'s bytes to `dest`, re-chunking whatever size the
/// source backend's `read()` produces into [`COPY_CHUNK_BYTES`]-sized
/// writes. Checked for cancellation on every chunk the *source* stream
/// yields (finer-grained than the output re-chunking), so a cancel lands
/// well within one output chunk of being requested rather than one whole
/// re-chunked write.
async fn copy_bytes(
    source_backend: &dyn Backend,
    dest_backend: &dyn Backend,
    source: &Location,
    dest: &Location,
    cancel: &AtomicBool,
    bytes_done: &mut u64,
    tx: &mut mpsc::Sender<OpEvent>,
) -> Result<CopyOutcome, VfsError> {
    let mut read = source_backend.read(source).await?;
    let mut write = dest_backend.write(dest).await?;

    let write_err = |dest: &Location| VfsError::Other {
        message: format!("writing {dest} failed"),
    };

    let mut buffer: Vec<u8> = Vec::with_capacity(COPY_CHUNK_BYTES);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = write.close().await;
            let _ = dest_backend.remove(dest).await;
            return Ok(CopyOutcome::Cancelled);
        }
        match read.next().await {
            Some(Ok(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() >= COPY_CHUNK_BYTES {
                    let flushed =
                        std::mem::replace(&mut buffer, Vec::with_capacity(COPY_CHUNK_BYTES));
                    let len = flushed.len() as u64;
                    if write.send(flushed).await.is_err() {
                        return Err(write_err(dest));
                    }
                    *bytes_done += len;
                    try_send_event(
                        tx,
                        OpEvent::Progress {
                            files_done: 0, // the caller (`copy_file`) overwrites this with the real count
                            bytes_done: *bytes_done,
                        },
                    );
                }
            }
            Some(Err(err)) => {
                let _ = write.close().await;
                let _ = dest_backend.remove(dest).await;
                return Err(err);
            }
            None => break,
        }
    }
    if !buffer.is_empty() {
        let len = buffer.len() as u64;
        if write.send(buffer).await.is_err() {
            return Err(write_err(dest));
        }
        *bytes_done += len;
    }
    if write.close().await.is_err() {
        return Err(write_err(dest));
    }
    Ok(CopyOutcome::Done)
}

/// Computes a "keep both" destination for [`ConflictChoice::RenameCopy`]:
/// `name (copy).ext`, then `name (copy 2).ext`, `name (copy 3).ext`, … —
/// the first that doesn't already exist at the destination. Bounded at
/// 1000 attempts purely so a pathological destination (something keeps
/// recreating every candidate name concurrently) can't spin forever; that
/// ceiling is never reached in practice and isn't worth surfacing as an
/// error if it somehow were — the 1000th candidate is returned regardless
/// and just overwrites, same as any other name collision would.
async fn unique_rename_dest(dest_backend: &dyn Backend, dest: &Location) -> Location {
    let (stem, extension) = split_extension(&dest.path);
    for n in 1..1000u32 {
        let candidate_name = if n == 1 {
            format!("{stem} (copy){extension}")
        } else {
            format!("{stem} (copy {n}){extension}")
        };
        let Some(parent) = dest.parent() else {
            break;
        };
        let candidate = parent.join(candidate_name);
        if dest_backend.metadata(&candidate).await.is_err() {
            return candidate;
        }
    }
    dest.clone()
}

/// Splits a file name's stem from its extension (including the leading
/// `.`), the way a human expects "report.tar.gz" ->
/// ("report.tar", ".gz") — only the *last* dot counts, so a dotfile with
/// no other dot (".bashrc") is treated as having no extension (stem
/// ".bashrc", extension "") rather than an empty stem, matching every
/// mainstream file manager's "keep both" naming.
fn split_extension(path: &std::path::Path) -> (String, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(0) | None => (name, String::new()),
        Some(index) => (name[..index].to_owned(), name[index..].to_owned()),
    }
}

/// Best-effort recursive pre-scan purely for [`OpEvent::Started`]'s
/// progress-bar denominator. Deliberately tolerant of errors mid-walk (a
/// permission-denied subdirectory just stops counting *that* branch,
/// rather than failing the whole op before it even starts) — an
/// undercounted total makes the progress bar read slightly optimistic
/// (100% arrives a hair before the last item finishes) rather than blocking
/// the copy on a scan that isn't itself doing any of the real work.
async fn count_totals(sources: &[Location]) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for source in sources {
        count_one(source, &mut files, &mut bytes).await;
    }
    (files, bytes)
}

fn count_one<'a>(
    source: &'a Location,
    files: &'a mut usize,
    bytes: &'a mut u64,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let Some(backend) = crate::modules::resolve(source) else {
            return;
        };
        let Ok(meta) = backend.metadata(source).await else {
            return;
        };
        if meta.kind == EntryKind::Directory {
            let Ok(children) = backend.list(source).await else {
                return;
            };
            for child in children {
                let child_source = source.join(&child.name);
                count_one(&child_source, files, bytes).await;
            }
        } else {
            *files += 1;
            *bytes += meta.size;
        }
    })
}

/// Delivers `event`, retrying through a full channel rather than dropping
/// it — for events a consumer must never miss (`Started`, `Conflict`,
/// `Finished`, `Cancelled`). Unlike `DirEvent`'s progress-style events
/// (`modules::local`'s `send_or_mark_overflow`, where a later event
/// supersedes an earlier dropped one), missing a `Conflict` would strand
/// the op waiting on an answer nobody was ever asked for, and missing
/// `Finished`/`Cancelled` would leave the UI thinking an op is still
/// running forever. Still never a hard block on the worker thread
/// (CLAUDE.md's bounded/`try_send` bridging rule): a full channel yields
/// via `tokio::task::yield_now` rather than blocking, and a *closed*
/// channel (the UI tore down the progress subscription — e.g. the app is
/// shutting down) ends the retry loop and returns `false` so the caller
/// can stop the op instead of retrying forever into the void.
async fn send_event(tx: &mut mpsc::Sender<OpEvent>, event: OpEvent) -> bool {
    loop {
        match tx.try_send(event.clone()) {
            Ok(()) => return true,
            Err(err) if err.is_full() => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(_) => return false,
        }
    }
}

/// Best-effort delivery for high-frequency events (`FileStarted`,
/// `Progress`) where a full channel just drops this one — the next such
/// event supersedes it, same reasoning as `modules::local`'s watch bridge.
fn try_send_event(tx: &mut mpsc::Sender<OpEvent>, event: OpEvent) {
    let _ = tx.try_send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vfs::Location;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    // ── Clipboard: pure logic, no I/O ───────────────────────────────────

    #[test]
    fn clipboard_starts_empty() {
        let clipboard = Clipboard::new();
        assert!(clipboard.is_empty());
        assert_eq!(clipboard.op(), None);
    }

    #[test]
    fn set_copy_and_set_cut_replace_op_and_contents() {
        let mut clipboard = Clipboard::new();
        clipboard.set_copy(vec![Location::local("/a")]);
        assert_eq!(clipboard.op(), Some(ClipboardOp::Copy));
        assert_eq!(clipboard.locations(), &[Location::local("/a")]);

        clipboard.set_cut(vec![Location::local("/b"), Location::local("/c")]);
        assert_eq!(clipboard.op(), Some(ClipboardOp::Cut));
        assert_eq!(clipboard.locations().len(), 2);
    }

    #[test]
    fn clear_empties_the_clipboard() {
        let mut clipboard = Clipboard::new();
        clipboard.set_copy(vec![Location::local("/a")]);
        clipboard.clear();
        assert!(clipboard.is_empty());
        assert_eq!(clipboard.op(), None);
    }

    // ── OpId / OpIdSource ────────────────────────────────────────────────

    #[test]
    fn op_id_source_hands_out_increasing_distinct_ids() {
        let mut ids = OpIdSource::default();
        let a = ids.alloc();
        let b = ids.alloc();
        assert_ne!(a, b);
    }

    #[test]
    fn op_request_hash_depends_only_on_id() {
        use std::hash::{Hash, Hasher};
        let mut ids = OpIdSource::default();
        let id = ids.alloc();
        let a = OpRequest::new(
            id,
            OpKind::Copy,
            vec![Location::local("/a")],
            Location::local("/dest"),
        );
        // A different `sources`/`dest_dir` with the *same* id still hashes
        // identically — id alone is the identity `Subscription::run_with`
        // relies on (see the type's doc comment).
        let b = OpRequest::new(id, OpKind::Move, vec![], Location::local("/other"));
        let hash_of = |r: &OpRequest| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            r.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── split_extension / unique_rename_dest ────────────────────────────

    #[test]
    fn split_extension_finds_the_last_dot() {
        assert_eq!(
            split_extension(std::path::Path::new("report.tar.gz")),
            ("report.tar".to_owned(), ".gz".to_owned())
        );
        assert_eq!(
            split_extension(std::path::Path::new("readme")),
            ("readme".to_owned(), String::new())
        );
    }

    #[test]
    fn split_extension_treats_a_leading_dot_as_no_extension() {
        assert_eq!(
            split_extension(std::path::Path::new(".bashrc")),
            (".bashrc".to_owned(), String::new())
        );
    }

    // ── Integration tests: real LocalBackend I/O over temp dirs ─────────
    //
    // Every test below drives `run()` end to end: a real spawned tokio
    // task, a real bounded event channel, real disk I/O through
    // `LocalBackend`. `#[tokio::test]` provides the runtime `tokio::spawn`/
    // `tokio::task::spawn_blocking` need, the same posture
    // `modules::local`'s own integration tests already take.

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-ops-test-{}-{}",
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

    /// Drains `stream` to completion, auto-answering every `Conflict` with
    /// `answer` — the shared driver every "conflict resolves one way" test
    /// below reuses. Returns every event seen, in order, so a test can
    /// assert on the whole shape (e.g. "no `Conflict` before the second
    /// file" or "exactly one `Conflict`").
    async fn drain_answering_conflicts_with(
        mut stream: BoxStream<'static, OpEvent>,
        answer: ConflictDecision,
    ) -> Vec<OpEvent> {
        let mut seen = Vec::new();
        while let Some(event) = stream.next().await {
            if let OpEvent::Conflict { ref reply, .. } = event {
                let mut reply = reply.clone();
                reply.try_send(answer).unwrap();
            }
            let is_terminal = matches!(event, OpEvent::Finished { .. } | OpEvent::Cancelled);
            seen.push(event);
            if is_terminal {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn copy_streams_a_file_tree_between_two_local_roots() {
        // "Cross-'backend' copy via two local roots" (PLAN.md): there is
        // only one real backend to test against until SFTP lands, so a
        // plain `OpKind::Copy` between two independent temp roots is what
        // exercises the exact same read-stream/write-sink code path a
        // genuine cross-backend copy would (Copy never takes the
        // same-backend rename fast path Move does, regardless of how
        // similar the two roots are).
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(src_root.join("sub")).unwrap();
        std::fs::write(src_root.join("sub/b.txt"), b"world").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(&src_root)],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Skip,
                apply_to_all: false,
            },
        )
        .await;

        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );

        let dst_name = src_root.file_name().unwrap();
        let copied_root = dst_root.join(dst_name);
        assert_eq!(std::fs::read(copied_root.join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(copied_root.join("sub/b.txt")).unwrap(),
            b"world"
        );
        // The source tree is untouched — this was a copy, not a move.
        assert!(src_root.join("a.txt").exists());

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn copy_preserves_non_utf8_names() {
        let src_root = tempdir();
        let dst_root = tempdir();
        let raw_name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        std::fs::write(src_root.join(raw_name), b"x").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(src_root.join(raw_name))],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Skip,
                apply_to_all: false,
            },
        )
        .await;
        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );

        let copied = dst_root.join(raw_name);
        assert_eq!(std::fs::read(&copied).unwrap(), b"x");

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn move_within_one_root_takes_the_rename_fast_path_and_deletes_the_source() {
        let root = tempdir();
        std::fs::create_dir(root.join("from")).unwrap();
        std::fs::create_dir(root.join("to")).unwrap();
        std::fs::write(root.join("from/f.txt"), b"data").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Move,
            vec![Location::local(root.join("from/f.txt"))],
            Location::local(root.join("to")),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Skip,
                apply_to_all: false,
            },
        )
        .await;
        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );

        assert!(!root.join("from/f.txt").exists());
        assert_eq!(std::fs::read(root.join("to/f.txt")).unwrap(), b"data");

        cleanup(root);
    }

    #[tokio::test]
    async fn move_deletes_the_source_tree_after_a_streamed_copy() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::create_dir(src_root.join("dir")).unwrap();
        std::fs::write(src_root.join("dir/f.txt"), b"data").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Move,
            vec![Location::local(src_root.join("dir"))],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Skip,
                apply_to_all: false,
            },
        )
        .await;
        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );

        assert!(!src_root.join("dir").exists());
        assert_eq!(std::fs::read(dst_root.join("dir/f.txt")).unwrap(), b"data");

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn conflict_overwrite_replaces_the_existing_destination() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("f.txt"), b"new").unwrap();
        std::fs::write(dst_root.join("f.txt"), b"old-longer-content").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(src_root.join("f.txt"))],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Overwrite,
                apply_to_all: false,
            },
        )
        .await;

        assert!(events.iter().any(|e| matches!(e, OpEvent::Conflict { .. })));
        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );
        assert_eq!(std::fs::read(dst_root.join("f.txt")).unwrap(), b"new");

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn conflict_skip_leaves_the_existing_destination_untouched() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("f.txt"), b"new").unwrap();
        std::fs::write(dst_root.join("f.txt"), b"old").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(src_root.join("f.txt"))],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Skip,
                apply_to_all: false,
            },
        )
        .await;

        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );
        assert_eq!(std::fs::read(dst_root.join("f.txt")).unwrap(), b"old");

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn conflict_rename_copy_keeps_both_files() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("f.txt"), b"new").unwrap();
        std::fs::write(dst_root.join("f.txt"), b"old").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(src_root.join("f.txt"))],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::RenameCopy,
                apply_to_all: false,
            },
        )
        .await;

        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );
        assert_eq!(std::fs::read(dst_root.join("f.txt")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(dst_root.join("f (copy).txt")).unwrap(),
            b"new"
        );

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn conflict_apply_to_all_answers_every_later_conflict_without_prompting() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("a.txt"), b"a-new").unwrap();
        std::fs::write(src_root.join("b.txt"), b"b-new").unwrap();
        std::fs::write(dst_root.join("a.txt"), b"a-old").unwrap();
        std::fs::write(dst_root.join("b.txt"), b"b-old").unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![
                Location::local(src_root.join("a.txt")),
                Location::local(src_root.join("b.txt")),
            ],
            Location::local(&dst_root),
        );
        let events = drain_answering_conflicts_with(
            run(&request),
            ConflictDecision {
                choice: ConflictChoice::Overwrite,
                apply_to_all: true,
            },
        )
        .await;

        let conflict_count = events
            .iter()
            .filter(|e| matches!(e, OpEvent::Conflict { .. }))
            .count();
        assert_eq!(conflict_count, 1, "only the first conflict should prompt");
        assert!(
            matches!(events.last(), Some(OpEvent::Finished { errors, .. }) if errors.is_empty())
        );
        assert_eq!(std::fs::read(dst_root.join("a.txt")).unwrap(), b"a-new");
        assert_eq!(std::fs::read(dst_root.join("b.txt")).unwrap(), b"b-new");

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn cancel_mid_copy_stops_early_and_removes_the_partial_destination() {
        let src_root = tempdir();
        let dst_root = tempdir();
        // Big enough that the bounded write-sink channel (4 x 64 KiB =
        // 256 KiB of slack, `modules::local::CHANNEL_CAPACITY`/
        // `CHUNK_SIZE`) forces this copy through many `.await` points —
        // plenty of room for a cancel issued after the first progress
        // event to land well before the file finishes.
        let payload = vec![7u8; 32 * 1024 * 1024];
        std::fs::write(src_root.join("big.bin"), &payload).unwrap();

        let mut ids = OpIdSource::default();
        let request = OpRequest::new(
            ids.alloc(),
            OpKind::Copy,
            vec![Location::local(src_root.join("big.bin"))],
            Location::local(&dst_root),
        );
        let mut stream = run(&request);

        // Cancel as soon as the first progress-bearing event arrives.
        loop {
            match stream.next().await {
                Some(OpEvent::Started { .. }) => continue,
                Some(OpEvent::Progress { .. }) | Some(OpEvent::FileStarted { .. }) => {
                    request.request_cancel();
                    break;
                }
                other => panic!("expected Started/Progress before Finished, got {other:?}"),
            }
        }

        let mut saw_cancelled = false;
        while let Some(event) = stream.next().await {
            if matches!(event, OpEvent::Cancelled) {
                saw_cancelled = true;
            }
            if matches!(event, OpEvent::Cancelled | OpEvent::Finished { .. }) {
                break;
            }
        }
        assert!(saw_cancelled, "expected the op to report Cancelled");
        assert!(
            !dst_root.join("big.bin").exists(),
            "a cancelled copy must not leave a partial file behind"
        );
        // The source is untouched — this was a Copy, and cancel must not
        // ever touch the source side regardless of op kind.
        assert_eq!(
            std::fs::read(src_root.join("big.bin")).unwrap().len(),
            payload.len()
        );

        cleanup(src_root);
        cleanup(dst_root);
    }

    // ── Trash and delete ────────────────────────────────────────────────
    //
    // None of these depend on timing. Cancel is triggered by the backend
    // itself after a fixed number of removals (not by a clock or by racing
    // the event stream), and progress events are never asserted on because
    // they are best-effort by design (`try_send_event` may drop them).

    use crate::core::fs::entry::FileEntry;
    use crate::core::vfs::{DirEvent, ReadStream, WriteSink};
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;

    /// One node of the in-memory tree.
    struct Node {
        kind: EntryKind,
        is_symlink: bool,
    }

    #[derive(Default)]
    struct TreeState {
        nodes: BTreeMap<PathBuf, Node>,
        /// Paths whose `remove` fails with permission denied.
        refuse: HashSet<PathBuf>,
        removed: Vec<PathBuf>,
        /// Every `remove` call, successful or not.
        attempted: Vec<PathBuf>,
        listed: Vec<PathBuf>,
        /// What `list` claims about a child, when different from the node
        /// (a server that reports followed attributes).
        claimed: BTreeMap<PathBuf, (EntryKind, bool)>,
        /// Extra names `list` returns for a directory (hostile server).
        phantoms: BTreeMap<PathBuf, Vec<OsString>>,
        /// `(parent, child)`: after `list(parent)` answers, `child` becomes
        /// a symlink (a swap racing the walk).
        swap_after_list: Vec<(PathBuf, PathBuf, usize)>,
        /// Every directory has one child directory `a`, forever.
        endless: bool,
        /// After this many successful removals, flip the flag.
        cancel_after: Option<(usize, Arc<AtomicBool>)>,
    }

    /// An in-memory remote-like backend. Like SFTP, `remove` on a directory
    /// that still has children fails; the engine has to go depth first.
    #[derive(Clone, Default)]
    struct TreeBackend {
        state: Arc<Mutex<TreeState>>,
    }

    fn fake(path: &str) -> Location {
        Location {
            scheme: "fake".to_owned(),
            authority: None,
            path: PathBuf::from(path),
        }
    }

    impl TreeBackend {
        fn with(self, path: &str, kind: EntryKind, is_symlink: bool) -> Self {
            self.state
                .lock()
                .unwrap()
                .nodes
                .insert(PathBuf::from(path), Node { kind, is_symlink });
            self
        }
        fn dir(self, path: &str) -> Self {
            self.with(path, EntryKind::Directory, false)
        }
        fn file(self, path: &str) -> Self {
            self.with(path, EntryKind::File, false)
        }
        fn claiming(self, path: &str, kind: EntryKind, is_symlink: bool) -> Self {
            self.state
                .lock()
                .unwrap()
                .claimed
                .insert(PathBuf::from(path), (kind, is_symlink));
            self
        }
        fn phantom(self, dir: &str, name: &[u8]) -> Self {
            use std::os::unix::ffi::OsStrExt;
            self.state
                .lock()
                .unwrap()
                .phantoms
                .entry(PathBuf::from(dir))
                .or_default()
                .push(OsString::from(std::ffi::OsStr::from_bytes(name)));
            self
        }
        /// After the `nth` (1-based) `list` of `parent` answers, `child`
        /// becomes a symlink.
        fn swapping(self, parent: &str, child: &str, nth: usize) -> Self {
            self.state.lock().unwrap().swap_after_list.push((
                PathBuf::from(parent),
                PathBuf::from(child),
                nth,
            ));
            self
        }
        fn listed(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().listed.clone()
        }
        fn attempted(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().attempted.clone()
        }
        fn refusing(self, path: &str) -> Self {
            self.state
                .lock()
                .unwrap()
                .refuse
                .insert(PathBuf::from(path));
            self
        }
        fn exists(&self, path: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .nodes
                .contains_key(Path::new(path))
        }
        fn removed(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().removed.clone()
        }
    }

    #[async_trait::async_trait]
    impl Backend for TreeBackend {
        fn scheme(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> Caps {
            Caps::empty()
        }
        async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError> {
            let mut state = self.state.lock().unwrap();
            state.listed.push(location.path.clone());
            if state.endless {
                return Ok(vec![FileEntry {
                    name: OsString::from("a"),
                    kind: EntryKind::Directory,
                    size: 0,
                    modified: None,
                    is_symlink: false,
                    mode: None,
                }]);
            }
            let mut entries: Vec<FileEntry> = state
                .nodes
                .iter()
                .filter(|(path, _)| path.parent() == Some(location.path.as_path()))
                .map(|(path, node)| {
                    let (kind, is_symlink) = state
                        .claimed
                        .get(path)
                        .copied()
                        .unwrap_or((node.kind, node.is_symlink));
                    FileEntry {
                        name: path.file_name().map(OsString::from).unwrap_or_default(),
                        kind,
                        size: 0,
                        modified: None,
                        is_symlink,
                        mode: None,
                    }
                })
                .collect();
            for name in state
                .phantoms
                .get(&location.path)
                .cloned()
                .unwrap_or_default()
            {
                entries.push(FileEntry {
                    name,
                    kind: EntryKind::File,
                    size: 0,
                    modified: None,
                    is_symlink: false,
                    mode: None,
                });
            }
            let times_listed = state
                .listed
                .iter()
                .filter(|path| **path == location.path)
                .count();
            let swaps: Vec<PathBuf> = state
                .swap_after_list
                .iter()
                .filter(|(parent, _, nth)| *parent == location.path && *nth == times_listed)
                .map(|(_, child, _)| child.clone())
                .collect();
            for child in swaps {
                if let Some(node) = state.nodes.get_mut(&child) {
                    node.is_symlink = true;
                }
            }
            Ok(entries)
        }
        async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
            let state = self.state.lock().unwrap();
            if state.endless {
                return Ok(FileEntry {
                    name: OsString::from("a"),
                    kind: EntryKind::Directory,
                    size: 0,
                    modified: None,
                    is_symlink: false,
                    mode: None,
                });
            }
            state
                .nodes
                .get(&location.path)
                .map(|node| FileEntry {
                    name: location
                        .path
                        .file_name()
                        .map(OsString::from)
                        .unwrap_or_default(),
                    kind: node.kind,
                    size: 0,
                    modified: None,
                    is_symlink: node.is_symlink,
                    mode: None,
                })
                .ok_or_else(|| VfsError::NotFound {
                    location: location.to_string(),
                })
        }
        async fn read(&self, _location: &Location) -> Result<ReadStream, VfsError> {
            Err(VfsError::Other {
                message: "unused".to_owned(),
            })
        }
        async fn write(&self, _location: &Location) -> Result<WriteSink, VfsError> {
            Err(VfsError::Other {
                message: "unused".to_owned(),
            })
        }
        async fn mkdir(&self, _location: &Location) -> Result<(), VfsError> {
            Err(VfsError::Other {
                message: "unused".to_owned(),
            })
        }
        async fn rename(&self, _from: &Location, _to: &Location) -> Result<(), VfsError> {
            Err(VfsError::Other {
                message: "unused".to_owned(),
            })
        }
        async fn remove(&self, location: &Location) -> Result<(), VfsError> {
            let mut state = self.state.lock().unwrap();
            state.attempted.push(location.path.clone());
            if state.endless {
                return Ok(());
            }
            if state.refuse.contains(&location.path) {
                return Err(VfsError::PermissionDenied {
                    location: location.to_string(),
                });
            }
            let has_children = state
                .nodes
                .keys()
                .any(|path| path.parent() == Some(location.path.as_path()));
            let is_real_dir = state
                .nodes
                .get(&location.path)
                .is_some_and(|n| n.kind == EntryKind::Directory && !n.is_symlink);
            if is_real_dir && has_children {
                return Err(VfsError::Other {
                    message: "directory not empty".to_owned(),
                });
            }
            if state.nodes.remove(&location.path).is_none() {
                return Err(VfsError::NotFound {
                    location: location.to_string(),
                });
            }
            state.removed.push(location.path.clone());
            if let Some((limit, flag)) = &state.cancel_after
                && state.removed.len() >= *limit
            {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
        async fn set_times(
            &self,
            _location: &Location,
            _accessed: Option<std::time::SystemTime>,
            _modified: Option<std::time::SystemTime>,
        ) -> Result<(), VfsError> {
            Ok(())
        }
        fn watch(&self, _location: &Location) -> Option<BoxStream<'static, DirEvent>> {
            None
        }
    }

    /// `run`, but resolving every location to `tree` instead of the global
    /// backend registry.
    fn run_on_tree(request: &OpRequest, tree: &TreeBackend) -> BoxStream<'static, OpEvent> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (request, tree) = (request.clone(), tree.clone());
        tokio::spawn(async move {
            let resolve = move |_: &Location| Some(Box::new(tree.clone()) as Box<dyn Backend>);
            execute_removal(request, tx, &resolve).await;
        });
        rx.boxed()
    }

    async fn collect(mut stream: BoxStream<'static, OpEvent>) -> Vec<OpEvent> {
        let mut seen = Vec::new();
        while let Some(event) = stream.next().await {
            let terminal = matches!(event, OpEvent::Finished { .. } | OpEvent::Cancelled);
            seen.push(event);
            if terminal {
                break;
            }
        }
        seen
    }

    fn errors_of(events: &[OpEvent]) -> &[(Location, VfsError)] {
        match events.last() {
            Some(OpEvent::Finished { errors, .. }) => errors,
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    fn delete_request(sources: Vec<Location>) -> OpRequest {
        OpRequest::removal(OpIdSource::default().alloc(), OpKind::Delete, sources)
    }

    /// Written first: a symlink to a directory must be deleted as a link.
    /// Real filesystem, real `LocalBackend`.
    #[tokio::test]
    async fn deleting_a_tree_unlinks_a_symlink_to_a_directory_without_following_it() {
        let root = tempdir();
        let precious = root.join("precious");
        std::fs::create_dir(&precious).unwrap();
        std::fs::write(precious.join("keep.txt"), b"keep").unwrap();
        let tree = root.join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(&precious, tree.join("link")).unwrap();

        let request = delete_request(vec![Location::local(&tree)]);
        let events = collect(run(&request)).await;

        assert!(errors_of(&events).is_empty());
        assert!(!tree.exists(), "the tree is gone, link included");
        assert_eq!(
            std::fs::read(precious.join("keep.txt")).unwrap(),
            b"keep",
            "the link's target must be untouched"
        );
        cleanup(root);
    }

    #[tokio::test]
    async fn a_symlink_to_a_directory_is_never_listed_on_any_backend() {
        // The link is *reported* as a directory (as a sloppy backend might)
        // with children beneath its path: only `is_symlink` keeps the
        // engine from walking into it.
        let tree = TreeBackend::default()
            .dir("/t")
            .with("/t/link", EntryKind::Directory, true)
            .file("/t/link/inside.txt");
        let request = delete_request(vec![fake("/t")]);
        let events = collect(run_on_tree(&request, &tree)).await;

        // `/t/link/inside.txt` still exists, so removing `/t` fails: that
        // is the proof the link was not entered.
        let listed = tree.state.lock().unwrap().listed.clone();
        assert!(!listed.contains(&PathBuf::from("/t/link")));
        assert!(tree.removed().contains(&PathBuf::from("/t/link")));
        assert!(tree.exists("/t/link/inside.txt"));
        assert!(matches!(events.last(), Some(OpEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn deleting_a_non_empty_folder_removes_children_before_their_directory() {
        let tree = TreeBackend::default()
            .dir("/srv")
            .dir("/srv/d")
            .file("/srv/d/a")
            .dir("/srv/d/sub")
            .file("/srv/d/sub/b");
        let request = delete_request(vec![fake("/srv/d")]);
        let events = collect(run_on_tree(&request, &tree)).await;

        assert!(errors_of(&events).is_empty());
        assert!(!tree.exists("/srv/d"));
        assert!(tree.exists("/srv"), "the parent is not touched");
        let removed = tree.removed();
        let at = |p: &str| removed.iter().position(|r| r == Path::new(p)).unwrap();
        assert!(at("/srv/d/sub/b") < at("/srv/d/sub"));
        assert!(at("/srv/d/sub") < at("/srv/d"));
        assert!(at("/srv/d/a") < at("/srv/d"));
        // The progress total is every item: d, a, sub, b.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OpEvent::Started { files_total: 4, .. })),
            "the scan's total is announced"
        );
    }

    #[tokio::test]
    async fn one_failing_child_is_one_error_and_the_rest_are_still_removed() {
        let tree = TreeBackend::default()
            .dir("/d")
            .file("/d/a")
            .file("/d/b")
            .file("/d/c")
            .file("/other")
            .refusing("/d/b");
        let request = delete_request(vec![fake("/d"), fake("/other")]);
        let events = collect(run_on_tree(&request, &tree)).await;

        let errors = errors_of(&events);
        assert_eq!(errors.len(), 1, "only the refused item is reported");
        assert_eq!(errors[0].0, fake("/d/b"));
        assert!(!tree.exists("/d/a") && !tree.exists("/d/c"));
        assert!(!tree.exists("/other"), "later sources still go");
        assert!(tree.exists("/d/b"));
        assert!(tree.exists("/d"), "a folder with a survivor stays");
    }

    #[tokio::test]
    async fn cancel_stops_cleanly_and_reports_nothing_as_failed() {
        let request = delete_request(vec![fake("/d")]);
        let tree = TreeBackend::default()
            .dir("/d")
            .file("/d/1")
            .file("/d/2")
            .file("/d/3")
            .file("/d/4")
            .file("/d/5");
        tree.state.lock().unwrap().cancel_after = Some((2, request.cancel.clone()));
        let events = collect(run_on_tree(&request, &tree)).await;

        assert!(matches!(events.last(), Some(OpEvent::Cancelled)));
        assert!(
            !events.iter().any(|e| matches!(e, OpEvent::Finished { .. })),
            "a cancelled op reports no failures"
        );
        assert_eq!(
            tree.removed().len(),
            2,
            "already-removed items stay removed"
        );
        assert!(tree.exists("/d"), "the rest remain");
    }

    #[tokio::test]
    async fn deleting_a_real_local_tree_removes_everything() {
        let root = tempdir();
        std::fs::create_dir_all(root.join("t/sub/deeper")).unwrap();
        std::fs::write(root.join("t/a"), b"a").unwrap();
        std::fs::write(root.join("t/sub/deeper/b"), b"b").unwrap();
        let request = delete_request(vec![Location::local(root.join("t"))]);
        let events = collect(run(&request)).await;
        assert!(errors_of(&events).is_empty());
        assert!(!root.join("t").exists());
        cleanup(root);
    }

    #[tokio::test]
    async fn trash_moves_each_source_and_announces_it_for_undo() {
        let root = tempdir();
        let trash_root = tempdir().join("Trash");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/inner"), b"i").unwrap();
        let sources = vec![
            Location::local(root.join("a.txt")),
            Location::local(root.join("missing")),
            Location::local(root.join("d")),
        ];
        let request = OpRequest::removal(OpIdSource::default().alloc(), OpKind::Trash, sources)
            .with_trash_root(trash_root.clone());
        let events = collect(run(&request)).await;

        // One failure, reported once; the items after it still went.
        let errors = errors_of(&events);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, Location::local(root.join("missing")));
        assert!(!root.join("a.txt").exists() && !root.join("d").exists());

        // Exactly the trashed items are announced, in order, and each id
        // restores its item: this is what the undo entry is built from.
        let trashed: Vec<(&Location, &TrashId)> = events
            .iter()
            .filter_map(|e| match e {
                OpEvent::Trashed { original, id } => Some((original, id)),
                _ => None,
            })
            .collect();
        assert_eq!(trashed.len(), 2);
        assert_eq!(trashed[0].0, &Location::local(root.join("a.txt")));
        assert_eq!(trashed[1].0, &Location::local(root.join("d")));
        for (original, id) in trashed {
            assert_eq!(trash::restore(id).unwrap(), original.path);
        }
        assert_eq!(std::fs::read(root.join("d/inner")).unwrap(), b"i");

        cleanup(root);
        cleanup(trash_root.parent().unwrap().to_path_buf());
    }

    #[tokio::test]
    async fn trash_on_a_backend_without_trash_fails_instead_of_deleting() {
        let tree = TreeBackend::default().file("/x");
        let request = OpRequest::removal(
            OpIdSource::default().alloc(),
            OpKind::Trash,
            vec![fake("/x")],
        );
        let events = collect(run_on_tree(&request, &tree)).await;
        assert_eq!(errors_of(&events).len(), 1);
        assert!(tree.exists("/x"), "never a silent permanent delete");
    }

    #[tokio::test]
    async fn only_trash_ops_produce_trashed_events() {
        let root = tempdir();
        std::fs::write(root.join("f"), b"f").unwrap();
        let request = delete_request(vec![Location::local(root.join("f"))]);
        let events = collect(run(&request)).await;
        assert!(!events.iter().any(|e| matches!(e, OpEvent::Trashed { .. })));
        cleanup(root);
    }

    // ── Descent safety, hostile names, bounds, cancel ───────────────────

    /// A tree with a directory-symlink `link` whose target holds `secret`.
    fn assert_link_removed_target_kept(tree: &TreeBackend, link: &str, secret: &str) {
        assert!(tree.attempted().contains(&PathBuf::from(link)));
        assert!(
            !tree.listed().contains(&PathBuf::from(link)),
            "never entered"
        );
        assert!(tree.exists(secret), "the link's target is untouched");
        assert!(!tree.exists(link), "the link itself is gone");
    }

    #[tokio::test]
    async fn an_entry_listed_as_a_directory_but_lstat_says_symlink_is_one_item() {
        // The server reports followed attributes: the listing says "real
        // directory"; only `metadata` (lstat) tells the truth.
        let tree = TreeBackend::default()
            .dir("/t")
            .with("/t/link", EntryKind::Directory, true)
            .claiming("/t/link", EntryKind::Directory, false)
            .file("/t/link/secret");
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        assert!(errors_of(&events).is_empty());
        assert_link_removed_target_kept(&tree, "/t/link", "/t/link/secret");
    }

    #[tokio::test]
    async fn an_entry_that_became_a_symlink_after_the_parent_listing_is_not_entered() {
        let tree = TreeBackend::default()
            .dir("/t")
            .dir("/t/d")
            .file("/t/d/secret")
            // The 2nd list of `/t` is the walk's (the 1st is the pre-scan's),
            // so only the walk's own lstat-before-descent can save `secret`.
            .swapping("/t", "/t/d", 2);
        let _ = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        // `/t/d` was listed once, by the scan (before the swap); the walk
        // must not have listed it again, nor touched what is inside.
        let d_lists = tree
            .listed()
            .iter()
            .filter(|path| **path == Path::new("/t/d"))
            .count();
        assert_eq!(d_lists, 1, "the walk must not enter the swapped entry");
        assert!(tree.exists("/t/d/secret"));
        assert!(!tree.attempted().contains(&PathBuf::from("/t/d/secret")));
    }

    #[tokio::test]
    async fn an_entry_that_becomes_a_symlink_during_the_pre_scan_is_not_counted_into() {
        // The 1st list of `/t` is the pre-scan's.
        let tree = TreeBackend::default()
            .dir("/t")
            .dir("/t/d")
            .file("/t/d/secret")
            .swapping("/t", "/t/d", 1);
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        // /t and d only: `secret` was never counted (nor listed).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OpEvent::Started { files_total: 2, .. }))
        );
        assert!(!tree.listed().contains(&PathBuf::from("/t/d")));
    }

    #[tokio::test]
    async fn an_endless_chain_of_directories_stops_at_the_depth_limit() {
        let tree = TreeBackend::default();
        tree.state.lock().unwrap().endless = true;
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;

        // One failure for the directory at the limit; nothing was removed
        // because every directory above it still has a child.
        assert_eq!(errors_of(&events).len(), 1);
        assert!(tree.attempted().is_empty());
        assert!(
            tree.listed().len() <= MAX_WALK_DEPTH * 2 + 2,
            "bounded work"
        );
    }

    #[tokio::test]
    async fn a_symlink_to_a_directory_three_levels_deep_is_not_entered() {
        let tree = TreeBackend::default()
            .dir("/t")
            .dir("/t/a")
            .dir("/t/a/b")
            .with("/t/a/b/link", EntryKind::Directory, true)
            .claiming("/t/a/b/link", EntryKind::Directory, false)
            .file("/t/a/b/link/secret");
        let _ = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        assert!(!tree.listed().contains(&PathBuf::from("/t/a/b/link")));
        assert!(tree.exists("/t/a/b/link/secret"));
    }

    #[tokio::test]
    async fn a_symlink_loop_terminates_and_unlinks_the_link() {
        let tree = TreeBackend::default()
            .dir("/t")
            .with("/t/loop", EntryKind::Directory, true)
            .claiming("/t/loop", EntryKind::Directory, false);
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        assert!(errors_of(&events).is_empty());
        assert!(!tree.exists("/t/loop") && !tree.exists("/t"));
        assert!(!tree.listed().contains(&PathBuf::from("/t/loop")));
    }

    #[tokio::test]
    async fn a_dangling_link_is_removed_as_one_item() {
        let tree = TreeBackend::default()
            .dir("/t")
            .with("/t/dangling", EntryKind::Other, true);
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        assert!(errors_of(&events).is_empty());
        assert!(!tree.exists("/t"));
    }

    #[tokio::test]
    async fn a_top_level_symlink_source_is_unlinked_not_followed() {
        let tree = TreeBackend::default()
            .with("/link", EntryKind::Directory, true)
            .file("/link/secret");
        let _ = collect(run_on_tree(&delete_request(vec![fake("/link")]), &tree)).await;
        assert!(!tree.listed().contains(&PathBuf::from("/link")));
        assert!(tree.exists("/link/secret"));
    }

    #[tokio::test]
    async fn hostile_entry_names_are_reported_and_never_joined() {
        let tree = TreeBackend::default()
            .dir("/t")
            .file("/t/ok")
            .file("/victim")
            .file("/home/u")
            .phantom("/t", b"")
            .phantom("/t", b".")
            .phantom("/t", b"..")
            .phantom("/t", b"a/../../victim")
            .phantom("/t", b"/home/u");
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;

        let errors = errors_of(&events);
        assert_eq!(errors.len(), 5, "one failure per hostile name");
        assert!(errors.iter().all(|(location, _)| *location == fake("/t")));
        assert!(tree.exists("/victim") && tree.exists("/home/u"));
        for attempted in tree.attempted() {
            assert!(attempted.starts_with("/t"), "touched {attempted:?}");
        }
        assert!(!tree.exists("/t/ok"), "the safe sibling still goes");
        assert!(tree.exists("/t"), "a folder with a skipped entry stays");
    }

    #[test]
    fn unsafe_names_are_judged_by_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let unsafe_name = |b: &[u8]| is_unsafe_entry_name(std::ffi::OsStr::from_bytes(b));
        assert!(unsafe_name(b"") && unsafe_name(b".") && unsafe_name(b".."));
        assert!(unsafe_name(b"a/b") && unsafe_name(b"/abs"));
        assert!(unsafe_name(b"x\xff/y"));
        assert!(!unsafe_name(b"caf\xe9.txt") && !unsafe_name(b"...") && !unsafe_name(b".hidden"));
    }

    #[tokio::test]
    async fn the_pre_scan_counts_only_safe_entries_and_never_follows_links() {
        let tree = TreeBackend::default()
            .dir("/t")
            .file("/t/a")
            .with("/t/link", EntryKind::Directory, true)
            .claiming("/t/link", EntryKind::Directory, false)
            .file("/t/link/secret")
            .phantom("/t", b"..")
            .phantom("/t", b"x/y");
        let events = collect(run_on_tree(&delete_request(vec![fake("/t")]), &tree)).await;
        // /t, a, link
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OpEvent::Started { files_total: 3, .. }))
        );
    }

    #[tokio::test]
    async fn a_huge_number_of_failures_is_bounded_and_still_counted() {
        let mut tree = TreeBackend::default().dir("/d");
        for n in 0..500 {
            let path = format!("/d/f{n:04}");
            tree = tree.file(&path).refusing(&path);
        }
        let events = collect(run_on_tree(&delete_request(vec![fake("/d")]), &tree)).await;
        match events.last() {
            Some(OpEvent::Finished { errors, omitted }) => {
                assert_eq!(errors.len(), MAX_KEPT_FAILURES);
                assert_eq!(errors.len() + omitted, 500);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn started_arrives_before_the_scan_and_cancel_stops_the_scan() {
        let request = delete_request(vec![fake("/d")]);
        let tree = TreeBackend::default().dir("/d").file("/d/1").file("/d/2");
        request.request_cancel(); // cancelled before anything runs
        let events = collect(run_on_tree(&request, &tree)).await;

        assert!(matches!(
            events.first(),
            Some(OpEvent::Started { files_total: 0, .. })
        ));
        assert!(matches!(events.last(), Some(OpEvent::Cancelled)));
        assert!(tree.listed().is_empty(), "the scan did not walk the tree");
        assert!(tree.attempted().is_empty(), "nothing was removed");
    }

    #[tokio::test]
    async fn cancelling_after_a_failure_still_reports_that_failure() {
        let request = delete_request(vec![fake("/d")]);
        let tree = TreeBackend::default()
            .dir("/d")
            .file("/d/a")
            .file("/d/b")
            .file("/d/c")
            .refusing("/d/a");
        // `a` fails, `b` is the first success: cancel lands right after it.
        tree.state.lock().unwrap().cancel_after = Some((1, request.cancel.clone()));
        let events = collect(run_on_tree(&request, &tree)).await;

        let errors = errors_of(&events);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, fake("/d/a"));
        assert!(tree.exists("/d/c"), "the rest remain");
    }

    #[tokio::test]
    async fn a_trash_that_fails_never_falls_back_to_deleting() {
        let root = tempdir();
        std::fs::write(root.join("keep.txt"), b"keep").unwrap();
        // A trash "directory" that cannot be created: its parent is a file.
        std::fs::write(root.join("blocker"), b"x").unwrap();
        let request = OpRequest::removal(
            OpIdSource::default().alloc(),
            OpKind::Trash,
            vec![Location::local(root.join("keep.txt"))],
        )
        .with_trash_root(root.join("blocker/Trash"));
        let events = collect(run(&request)).await;

        assert_eq!(errors_of(&events).len(), 1);
        assert_eq!(std::fs::read(root.join("keep.txt")).unwrap(), b"keep");
        cleanup(root);
    }
}
