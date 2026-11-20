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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::sink::SinkExt;
use futures::stream::{BoxStream, StreamExt};

use crate::core::fs::entry::EntryKind;
use crate::core::vfs::{Backend, Caps, Location, VfsError, WriteOptions};

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
    pub dest_dir: Location,
    cancel: Arc<AtomicBool>,
}

impl OpRequest {
    pub fn new(id: OpId, kind: OpKind, sources: Vec<Location>, dest_dir: Location) -> Self {
        OpRequest {
            id,
            kind,
            sources,
            dest_dir,
            cancel: Arc::new(AtomicBool::new(false)),
        }
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
    Finished { errors: Vec<(Location, VfsError)> },
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
        send_event(&mut tx, OpEvent::Finished { errors: Vec::new() }).await;
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
        send_event(&mut ctx.tx, OpEvent::Finished { errors: ctx.errors }).await;
    }
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

        // Nothing at `dest`: write straight to it. Created exclusively, so a
        // file that appears there in the meantime is refused ("already
        // exists") instead of truncated. Synced for a Move, whose source is
        // removed next.
        let Ok(existing) = self.dest_backend.metadata(&dest).await else {
            let options = WriteOptions {
                create_new: true,
                sync: self.is_move,
            };
            let outcome = copy_bytes(
                source_backend,
                self.dest_backend.as_ref(),
                &source,
                &dest,
                options,
                &mut Transfer {
                    cancel: &self.cancel,
                    bytes_done: &mut self.bytes_done,
                    tx: &mut self.tx,
                },
            )
            .await;
            self.finish_file(source_backend, source, outcome, true)
                .await;
            return;
        };

        // Identity is settled before the prompt (which is still raised, as
        // for any taken name) so that no answer can copy a file onto itself:
        // reading and writing one file at once is how an Overwrite used to
        // empty it, and a Move would then remove the only copy.
        let same =
            same_backend(&source, &dest) && self.dest_backend.same_file(&source, &dest).await;
        let Some(choice) = self.ask_conflict(&source, &dest).await else {
            return; // cancelled (`ask_conflict` set `self.cancelled`)
        };
        match choice {
            ConflictChoice::Skip => self.count_unwritten(size),
            // The file already is its own destination: nothing to write,
            // and for a Move nothing to remove.
            ConflictChoice::Overwrite if same => self.count_unwritten(size),
            ConflictChoice::Overwrite => {
                let outcome = replace_file(
                    source_backend,
                    self.dest_backend.as_ref(),
                    &source,
                    &dest,
                    &existing,
                    &mut Transfer {
                        cancel: &self.cancel,
                        bytes_done: &mut self.bytes_done,
                        tx: &mut self.tx,
                    },
                )
                .await;
                self.finish_file(source_backend, source, outcome, true)
                    .await;
            }
            ConflictChoice::RenameCopy => {
                let target = unique_rename_dest(self.dest_backend.as_ref(), &dest).await;
                // Exclusive, so the fresh name can never truncate a file
                // that took it after `unique_rename_dest` looked.
                let options = WriteOptions {
                    create_new: true,
                    sync: self.is_move && !same,
                };
                let outcome = copy_bytes(
                    source_backend,
                    self.dest_backend.as_ref(),
                    &source,
                    &target,
                    options,
                    &mut Transfer {
                        cancel: &self.cancel,
                        bytes_done: &mut self.bytes_done,
                        tx: &mut self.tx,
                    },
                )
                .await;
                // Keep both onto itself keeps both, even for a Move: the
                // source is the original the copy sits beside.
                self.finish_file(source_backend, source, outcome, !same)
                    .await;
            }
        }
    }

    /// A file that was skipped (or was already its own destination) still
    /// counts as done for progress, so a skip-heavy op reaches 100%.
    fn count_unwritten(&mut self, size: u64) {
        self.files_done += 1;
        self.bytes_done += size;
        self.emit_progress();
    }

    /// Books the result of one file's copy. Only a finished copy (written,
    /// closed, synced where asked, and in place) lets a Move remove its
    /// source, and only when `remove_source` says the source is not the
    /// copy's own original.
    async fn finish_file(
        &mut self,
        source_backend: &dyn Backend,
        source: Location,
        outcome: Result<CopyOutcome, VfsError>,
        remove_source: bool,
    ) {
        let leftover = match outcome {
            Ok(CopyOutcome::Done) => None,
            Ok(CopyOutcome::DoneLeavingOldFile(err)) => Some(err),
            Ok(CopyOutcome::Cancelled) => {
                self.cancelled = true;
                return;
            }
            Err(err) => {
                self.errors.push((source, err));
                return;
            }
        };
        self.files_done += 1;
        self.emit_progress();
        if let Some(err) = leftover {
            self.errors.push((source.clone(), err));
        }
        if self.is_move
            && remove_source
            && let Err(err) = source_backend.remove(&source).await
        {
            self.errors.push((source, err));
        }
    }

    /// Resolves a conflict to the destination to write to, for
    /// `copy_dir` (a folder meeting a non-folder): `Some(dest)` for
    /// Overwrite, a fresh name for Keep both, `None` for Skip or cancel.
    async fn resolve_conflict(&mut self, source: &Location, dest: &Location) -> Option<Location> {
        match self.ask_conflict(source, dest).await? {
            ConflictChoice::Overwrite => Some(dest.clone()),
            ConflictChoice::Skip => None,
            ConflictChoice::RenameCopy => {
                Some(unique_rename_dest(self.dest_backend.as_ref(), dest).await)
            }
        }
    }

    /// Gets the answer to one conflict: reuses a remembered `apply_to_all`
    /// choice if there is one, otherwise prompts (must-deliver — see
    /// `send_event`) and awaits the answer. `None` means cancelled: a
    /// dropped reply channel (the progress UI was torn down mid-prompt) is
    /// treated as a cancel, because there is no sane default to fall back
    /// to for "should this overwrite", so this sets `self.cancelled` rather
    /// than guessing.
    async fn ask_conflict(&mut self, source: &Location, dest: &Location) -> Option<ConflictChoice> {
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
        Some(choice)
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
    /// The copy is complete and in place, but the file it replaced could
    /// not be removed afterwards and is still there under a temporary
    /// name (only on a backend whose rename refuses to replace; see
    /// [`place_over`]). Reported, never silent.
    DoneLeavingOldFile(VfsError),
    Cancelled,
}

/// The running op's progress plumbing a byte copy updates: the cancel
/// flag, the byte count and the event channel. One struct (borrowing the
/// matching `ExecContext` fields) rather than three more parameters.
struct Transfer<'a> {
    cancel: &'a AtomicBool,
    bytes_done: &'a mut u64,
    tx: &'a mut mpsc::Sender<OpEvent>,
}

/// Copies `source` to a new file at `dest`, opened with `options`.
async fn copy_bytes(
    source_backend: &dyn Backend,
    dest_backend: &dyn Backend,
    source: &Location,
    dest: &Location,
    options: WriteOptions,
    transfer: &mut Transfer<'_>,
) -> Result<CopyOutcome, VfsError> {
    let read = source_backend.read(source).await?;
    // Returns before anything is written if `dest` can't be opened — in
    // particular, an exclusive create that finds the name taken leaves the
    // file there alone (nothing below runs, so nothing removes it).
    let write = dest_backend.write_with(dest, options).await?;
    stream_into(read, write, dest_backend, dest, transfer).await
}

/// Streams `read` into `write` (which is `dest`, already opened by the
/// caller), re-chunking whatever size the source backend's `read()`
/// produces into [`COPY_CHUNK_BYTES`]-sized writes. Checked for
/// cancellation on every chunk the *source* stream yields (finer-grained
/// than the output re-chunking), so a cancel lands well within one output
/// chunk of being requested rather than one whole re-chunked write.
///
/// On any failure or cancel `dest` is removed: the caller only ever passes
/// a file this op created, never one that held user data.
async fn stream_into(
    mut read: crate::core::vfs::ReadStream,
    mut write: crate::core::vfs::WriteSink,
    dest_backend: &dyn Backend,
    dest: &Location,
    transfer: &mut Transfer<'_>,
) -> Result<CopyOutcome, VfsError> {
    let write_err = |dest: &Location| VfsError::Other {
        message: format!("writing {dest} failed"),
    };

    let mut buffer: Vec<u8> = Vec::with_capacity(COPY_CHUNK_BYTES);
    loop {
        if transfer.cancel.load(Ordering::Relaxed) {
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
                        return Err(
                            abort_write(&mut write, dest_backend, dest, write_err(dest)).await
                        );
                    }
                    *transfer.bytes_done += len;
                    try_send_event(
                        transfer.tx,
                        OpEvent::Progress {
                            files_done: 0, // the caller (`copy_file`) overwrites this with the real count
                            bytes_done: *transfer.bytes_done,
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
            return Err(abort_write(&mut write, dest_backend, dest, write_err(dest)).await);
        }
        *transfer.bytes_done += len;
    }
    // Closing is where a backend reports a write that failed on the way to
    // disk (ENOSPC on the final chunks, a failed fsync, a server refusing
    // the close): never treat a failed close as a finished copy.
    if let Err(err) = write.close().await {
        let _ = dest_backend.remove(dest).await;
        return Err(err);
    }
    Ok(CopyOutcome::Done)
}

/// Every temporary file the engine creates beside a destination starts
/// with one of these, so a leftover (the app was killed mid-copy) is
/// recognisable and never mistaken for user data. Hidden (leading dot) so
/// an in-flight copy doesn't flash through the folder view. The rest of
/// the name is the process id, the time and a counter (see [`temp_name`]),
/// never the destination's own name, so it always fits in `NAME_MAX`.
///
/// `PARTIAL_PREFIX`: the new content of an Overwrite, before it is renamed
/// over the old file. `REPLACED_PREFIX`: the old file, moved aside only on
/// a backend whose rename refuses to replace (see [`place_over`]).
pub const PARTIAL_PREFIX: &str = ".saola-files-partial-";
pub const REPLACED_PREFIX: &str = ".saola-files-replaced-";

/// How many fresh temporary names [`open_temp_sibling`] tries before
/// giving up. A clash needs another file to hold this exact pid-time-
/// counter name, so a second attempt is already belt and braces.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// A fresh temporary file name: `prefix`, then pid, nanoseconds and a
/// per-process counter in hex. Unique within this process by the counter,
/// and across processes by the pid; uniqueness is still *checked* by the
/// exclusive create, never assumed.
fn temp_name(prefix: &str) -> OsString {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    OsString::from(format!("{prefix}{:x}-{nanos:x}-{n:x}", std::process::id()))
}

/// Creates a temporary file beside `dest` (same folder, so on a local
/// disk the final rename is atomic: same filesystem), exclusively and
/// with `sync` set, trying a fresh name from `names` whenever one is
/// taken. Returns the temporary location and its open sink.
///
/// Any other failure is returned as is. In particular a folder that isn't
/// writable fails here even when the file itself is: replacing a file
/// safely needs a second name in its folder, and truncating it in place
/// instead is exactly what lost data before, so there is no fallback.
async fn open_temp_sibling(
    dest_backend: &dyn Backend,
    dest: &Location,
    names: &mut (dyn FnMut() -> OsString + Send),
) -> Result<(Location, crate::core::vfs::WriteSink), VfsError> {
    let Some(parent) = dest.parent() else {
        return Err(VfsError::IsADirectory {
            location: dest.to_string(),
        });
    };
    let options = WriteOptions {
        create_new: true,
        sync: true,
    };
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let temp = parent.join(names());
        match dest_backend.write_with(&temp, options).await {
            Ok(sink) => return Ok((temp, sink)),
            Err(VfsError::AlreadyExists { .. }) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(VfsError::AlreadyExists {
        location: dest.to_string(),
    })
}

/// Overwrite, safely: copies `source` into a temporary sibling of `dest`
/// ([`open_temp_sibling`]), closes it (synced), and only then puts it in
/// `dest`'s place ([`place_over`]). Until that last step `dest` is never
/// opened, so a failed read, a failed write, a failed sync or a cancel
/// leaves it byte for byte as it was, and the temporary file is removed
/// (by [`stream_into`]).
///
/// Because `dest` is replaced by a rename and never written through, a
/// symlink at `dest` is replaced by the new file; its target is untouched.
///
/// `existing` is what `dest` held (from `metadata`, which does not follow
/// symlinks): a folder is refused, as the old in-place write refused it;
/// a regular file's permission bits are carried over to the new file,
/// which the old in-place write kept implicitly (it reused the inode).
async fn replace_file(
    source_backend: &dyn Backend,
    dest_backend: &dyn Backend,
    source: &Location,
    dest: &Location,
    existing: &crate::core::fs::entry::FileEntry,
    transfer: &mut Transfer<'_>,
) -> Result<CopyOutcome, VfsError> {
    if existing.kind == EntryKind::Directory {
        return Err(VfsError::IsADirectory {
            location: dest.to_string(),
        });
    }
    let read = source_backend.read(source).await?;
    let (temp, write) =
        open_temp_sibling(dest_backend, dest, &mut || temp_name(PARTIAL_PREFIX)).await?;
    if let CopyOutcome::Cancelled = stream_into(read, write, dest_backend, &temp, transfer).await? {
        return Ok(CopyOutcome::Cancelled);
    }
    // Best effort, like the rest of the metadata this engine doesn't copy:
    // a failure here leaves the new file with default permissions, which
    // is not worth failing a complete copy over. Set-id bits are dropped
    // (`0o777`): the kernel clears them when a file is written in place,
    // so carrying them onto new content would grant more than before.
    if existing.kind == EntryKind::File
        && !existing.is_symlink
        && dest_backend.caps().contains(Caps::SET_PERMISSIONS)
        && let Some(mode) = existing.mode
    {
        let _ = dest_backend.set_permissions(&temp, mode & 0o777).await;
    }
    place_over(dest_backend, &temp, dest).await
}

/// Puts the finished temporary file `temp` in `dest`'s place.
///
/// **Local:** `rename(2)` replaces `dest` atomically (same folder, same
/// filesystem): every reader sees either the old file or the new one,
/// complete. That is the whole story there.
///
/// **A backend whose rename refuses an existing target** (SFTP: the
/// `SSH_FXP_RENAME` russh-sftp sends is plain v3 rename, which OpenSSH's
/// server answers with a failure when the target exists; the atomic
/// `posix-rename@openssh.com` extension isn't reachable through
/// russh-sftp's high-level session): the first rename fails, so `dest` is
/// first renamed aside to a [`REPLACED_PREFIX`] name, `temp` renamed into
/// place, and the old file removed. Not atomic: between the two renames
/// `dest` is briefly absent, but both versions exist at every moment:
/// - the second rename fails: the old file is renamed back and `temp`
///   removed; the copy fails with the original untouched;
/// - renaming back fails too (connection lost): nothing is removed, and
///   the error names where the new copy and the old file both are;
/// - only removing the old file fails: the copy is done, and the leftover
///   is reported ([`CopyOutcome::DoneLeavingOldFile`]).
///
/// If `dest` is gone after the first failure, or can't be moved aside,
/// the first error is returned and `temp` removed (the source still has
/// the data).
async fn place_over(
    dest_backend: &dyn Backend,
    temp: &Location,
    dest: &Location,
) -> Result<CopyOutcome, VfsError> {
    let first = match dest_backend.rename(temp, dest).await {
        Ok(()) => return Ok(CopyOutcome::Done),
        Err(err) => err,
    };
    let backup = dest
        .parent()
        .map(|parent| parent.join(temp_name(REPLACED_PREFIX)));
    let moved_aside = match &backup {
        Some(backup) if dest_backend.metadata(dest).await.is_ok() => {
            dest_backend.metadata(backup).await.is_err()
                && dest_backend.rename(dest, backup).await.is_ok()
        }
        _ => false,
    };
    let (true, Some(backup)) = (moved_aside, backup) else {
        let _ = dest_backend.remove(temp).await;
        return Err(first);
    };
    match dest_backend.rename(temp, dest).await {
        Ok(()) => match dest_backend.remove(&backup).await {
            Ok(()) => Ok(CopyOutcome::Done),
            Err(_) => Ok(CopyOutcome::DoneLeavingOldFile(VfsError::Other {
                message: crate::tr!("copy-replaced-file-kept", old = file_name_of(&backup)),
            })),
        },
        Err(second) => {
            if dest_backend.rename(&backup, dest).await.is_ok() {
                let _ = dest_backend.remove(temp).await;
                Err(second)
            } else {
                Err(VfsError::Other {
                    message: crate::tr!(
                        "copy-replace-stranded",
                        new = file_name_of(temp),
                        old = file_name_of(&backup)
                    ),
                })
            }
        }
    }
}

/// A location's last component for wording (the catalogue's filename rule:
/// lossy only at display time).
fn file_name_of(location: &Location) -> String {
    location
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A write failed mid-copy: closes the sink to learn the backend's real
/// error (falling back to `fallback` when close succeeds), removes the
/// partial `dest`, and returns the error to report. A truncated file must
/// never be left looking like a finished copy.
async fn abort_write(
    write: &mut crate::core::vfs::WriteSink,
    dest_backend: &dyn Backend,
    dest: &Location,
    fallback: VfsError,
) -> VfsError {
    let err = write.close().await.err().unwrap_or(fallback);
    let _ = dest_backend.remove(dest).await;
    err
}

/// Computes a "keep both" destination for [`ConflictChoice::RenameCopy`]:
/// `name (copy).ext`, then `name (copy 2).ext`, `name (copy 3).ext`, … —
/// the first that doesn't already exist at the destination. Bounded at
/// 1000 attempts purely so a pathological destination (something keeps
/// recreating every candidate name concurrently) can't spin forever; that
/// ceiling is never reached in practice. If it somehow were, `dest` itself
/// comes back, and the caller's exclusive create refuses it ("already
/// exists") instead of overwriting.
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

/// Data-safety tests for Overwrite, same-file copies, failed and cancelled
/// writes and Move's source removal (issue #119). Every assertion is on
/// file *content*, with distinct bytes on each side. Kept apart from
/// `tests` (with its own temp-dir helper) so other work appending to that
/// module merges cleanly.
#[cfg(test)]
mod safety_tests {
    use super::*;
    use crate::core::fs::entry::FileEntry;
    use crate::core::vfs::{DirEvent, FsStats, ReadStream, WriteSink};
    use crate::modules::local::LocalBackend;
    use async_trait::async_trait;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::SystemTime;

    /// A fresh directory (pid + time + per-process counter: see
    /// `tests::tempdir` for why the counter matters).
    fn tempdir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "saola-files-ops-safety-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every engine temporary (`PARTIAL_PREFIX`/`REPLACED_PREFIX`) in `dir`.
    fn temporaries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".saola-files-"))
            .collect()
    }

    /// 3.5 MiB of a repeating pattern: four engine writes (1 + 1 + 1 +
    /// 0.5 MiB), so "first", "middle" and "last" chunk are distinct sends.
    fn new_bytes() -> Vec<u8> {
        (0..(3 * COPY_CHUNK_BYTES + COPY_CHUNK_BYTES / 2))
            .map(|i| (i % 251) as u8)
            .collect()
    }

    const OLD: &[u8] = b"the original destination, which must survive";

    // ── a destination backend with injected failures ─────────────────────

    /// What goes wrong in [`FaultyBackend`]'s write sink.
    #[derive(Clone)]
    enum Fault {
        None,
        /// The n-th `send` (1-based) fails, like a disk filling up or a
        /// server refusing a write.
        FailSend(usize),
        /// Every send succeeds; `close` fails, like a failed fsync.
        FailClose,
        /// Raise the op's cancel flag during the n-th send.
        CancelAtSend(usize, Arc<AtomicBool>),
        /// At `close`, record whether this source path still exists.
        WatchSource(PathBuf, Arc<Mutex<Option<bool>>>),
    }

    /// How [`FaultyBackend::rename`] behaves.
    #[derive(Clone, Copy, PartialEq)]
    enum Renames {
        /// The local backend's: replaces an existing target.
        Replace,
        /// Refuses an existing target, like SFTP v3 on OpenSSH.
        NoReplace,
        /// `NoReplace`, and also refuses to rename a `PARTIAL_PREFIX`
        /// temporary at all, so the new file can never be put in place.
        NoReplaceNoTemp,
    }

    /// The local backend with a [`Fault`] in its sink and a choice of
    /// rename semantics: a stand-in for a remote (or failing) destination,
    /// since there is no fake SFTP session.
    struct FaultyBackend {
        inner: LocalBackend,
        fault: Fault,
        renames: Renames,
    }

    impl FaultyBackend {
        fn new(fault: Fault) -> Self {
            FaultyBackend {
                inner: LocalBackend::new(),
                fault,
                renames: Renames::Replace,
            }
        }

        fn renames(mut self, renames: Renames) -> Self {
            self.renames = renames;
            self
        }
    }

    struct FaultySink {
        inner: WriteSink,
        sent: usize,
        fault: Fault,
    }

    fn injected(what: &str) -> VfsError {
        VfsError::Other {
            message: format!("injected {what} failure"),
        }
    }

    impl futures::sink::Sink<Vec<u8>> for FaultySink {
        type Error = VfsError;

        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
            self.get_mut().inner.as_mut().poll_ready(cx)
        }

        fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), VfsError> {
            let this = self.get_mut();
            this.sent += 1;
            match &this.fault {
                Fault::FailSend(n) if *n == this.sent => return Err(injected("write")),
                Fault::CancelAtSend(n, flag) if *n == this.sent => {
                    flag.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
            this.inner.as_mut().start_send(item)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
            self.get_mut().inner.as_mut().poll_flush(cx)
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
            let this = self.get_mut();
            let closed = futures::ready!(this.inner.as_mut().poll_close(cx));
            match &this.fault {
                Fault::FailClose => Poll::Ready(Err(injected("sync"))),
                Fault::WatchSource(path, seen) => {
                    *seen.lock().unwrap() = Some(path.exists());
                    Poll::Ready(closed)
                }
                _ => Poll::Ready(closed),
            }
        }
    }

    #[async_trait]
    impl Backend for FaultyBackend {
        fn scheme(&self) -> &'static str {
            "file"
        }
        fn caps(&self) -> Caps {
            self.inner.caps()
        }
        async fn list(&self, l: &Location) -> Result<Vec<FileEntry>, VfsError> {
            self.inner.list(l).await
        }
        async fn metadata(&self, l: &Location) -> Result<FileEntry, VfsError> {
            self.inner.metadata(l).await
        }
        async fn read(&self, l: &Location) -> Result<ReadStream, VfsError> {
            self.inner.read(l).await
        }
        async fn write(&self, l: &Location) -> Result<WriteSink, VfsError> {
            self.write_with(l, WriteOptions::default()).await
        }
        async fn write_with(
            &self,
            l: &Location,
            options: WriteOptions,
        ) -> Result<WriteSink, VfsError> {
            let inner = self.inner.write_with(l, options).await?;
            Ok(Box::pin(FaultySink {
                inner,
                sent: 0,
                fault: self.fault.clone(),
            }))
        }
        async fn same_file(&self, a: &Location, b: &Location) -> bool {
            self.inner.same_file(a, b).await
        }
        async fn mkdir(&self, l: &Location) -> Result<(), VfsError> {
            self.inner.mkdir(l).await
        }
        async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError> {
            let is_temp = from
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(PARTIAL_PREFIX));
            let target_taken = std::fs::symlink_metadata(&to.path).is_ok();
            let refused = match self.renames {
                Renames::Replace => false,
                Renames::NoReplace => target_taken,
                Renames::NoReplaceNoTemp => target_taken || is_temp,
            };
            if refused {
                return Err(VfsError::Other {
                    message: "Failure".to_owned(),
                });
            }
            self.inner.rename(from, to).await
        }
        async fn remove(&self, l: &Location) -> Result<(), VfsError> {
            self.inner.remove(l).await
        }
        async fn set_times(
            &self,
            l: &Location,
            a: Option<SystemTime>,
            m: Option<SystemTime>,
        ) -> Result<(), VfsError> {
            self.inner.set_times(l, a, m).await
        }
        async fn set_permissions(&self, l: &Location, mode: u32) -> Result<(), VfsError> {
            self.inner.set_permissions(l, mode).await
        }
        async fn statfs(&self, l: &Location) -> Result<FsStats, VfsError> {
            self.inner.statfs(l).await
        }
        fn watch(&self, _: &Location) -> Option<futures::stream::BoxStream<'static, DirEvent>> {
            None
        }
    }

    // ── driving one file through the engine ──────────────────────────────

    /// The engine's per-op state with every conflict answered `choice`
    /// (as if "apply to all" had been chosen), so `copy_file` runs without
    /// a UI. Returns the receiver too: it must stay alive for the events.
    fn context(
        dest_backend: Box<dyn Backend>,
        is_move: bool,
        choice: ConflictChoice,
        cancel: Arc<AtomicBool>,
    ) -> (ExecContext, mpsc::Receiver<OpEvent>) {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let ctx = ExecContext {
            cancel,
            tx,
            dest_backend,
            files_done: 0,
            bytes_done: 0,
            remembered: Some(choice),
            is_move,
            errors: Vec::new(),
            cancelled: false,
        };
        (ctx, rx)
    }

    /// Copies (or moves) the file `source` to `dest` through
    /// `ExecContext::copy_file` with `dest_backend`, every conflict
    /// answered `choice`. Returns the finished context.
    async fn copy_one(
        dest_backend: impl Backend + 'static,
        is_move: bool,
        choice: ConflictChoice,
        cancel: Arc<AtomicBool>,
        source: &Path,
        dest: &Path,
    ) -> ExecContext {
        let (mut ctx, _rx) = context(Box::new(dest_backend), is_move, choice, cancel);
        let size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
        ctx.copy_file(
            &LocalBackend::new(),
            Location::local(source),
            Location::local(dest),
            size,
        )
        .await;
        ctx
    }

    fn no_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Source with [`new_bytes`] in `src/f.bin`, destination `dst/f.bin`
    /// holding [`OLD`]. Returns `(root, source, dest)`.
    fn overwrite_fixture() -> (PathBuf, PathBuf, PathBuf) {
        let root = tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("dst")).unwrap();
        let (source, dest) = (root.join("src/f.bin"), root.join("dst/f.bin"));
        std::fs::write(&source, new_bytes()).unwrap();
        std::fs::write(&dest, OLD).unwrap();
        (root, source, dest)
    }

    // ── failed or cancelled Overwrite leaves the original intact ─────────

    async fn assert_failed_overwrite_keeps_original(fault: Fault, is_move: bool) {
        let (root, source, dest) = overwrite_fixture();
        let ctx = copy_one(
            FaultyBackend::new(fault),
            is_move,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert_eq!(ctx.errors.len(), 1, "the failure is reported");
        assert_eq!(std::fs::read(&dest).unwrap(), OLD, "original untouched");
        assert_eq!(std::fs::read(&source).unwrap(), new_bytes(), "source kept");
        assert!(temporaries(&root.join("dst")).is_empty(), "no temp left");
        cleanup(root);
    }

    #[tokio::test]
    async fn overwrite_failing_on_the_first_chunk_keeps_the_original() {
        assert_failed_overwrite_keeps_original(Fault::FailSend(1), false).await;
    }

    #[tokio::test]
    async fn overwrite_failing_on_a_middle_chunk_keeps_the_original() {
        assert_failed_overwrite_keeps_original(Fault::FailSend(2), false).await;
    }

    #[tokio::test]
    async fn overwrite_failing_on_the_last_chunk_keeps_the_original() {
        assert_failed_overwrite_keeps_original(Fault::FailSend(4), false).await;
    }

    #[tokio::test]
    async fn overwrite_whose_sync_fails_keeps_the_original() {
        assert_failed_overwrite_keeps_original(Fault::FailClose, false).await;
    }

    #[tokio::test]
    async fn failed_move_overwrite_keeps_the_original_and_the_source() {
        assert_failed_overwrite_keeps_original(Fault::FailSend(2), true).await;
        assert_failed_overwrite_keeps_original(Fault::FailClose, true).await;
    }

    #[tokio::test]
    async fn overwrite_cancelled_mid_copy_keeps_the_original() {
        let (root, source, dest) = overwrite_fixture();
        let cancel = no_cancel();
        let ctx = copy_one(
            FaultyBackend::new(Fault::CancelAtSend(2, cancel.clone())),
            true,
            ConflictChoice::Overwrite,
            cancel,
            &source,
            &dest,
        )
        .await;
        assert!(ctx.cancelled);
        assert_eq!(std::fs::read(&dest).unwrap(), OLD);
        assert_eq!(std::fs::read(&source).unwrap(), new_bytes());
        assert!(temporaries(&root.join("dst")).is_empty());
        cleanup(root);
    }

    #[tokio::test]
    async fn successful_overwrite_replaces_content_and_keeps_permissions() {
        let (root, source, dest) = overwrite_fixture();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o640)).unwrap();
        let ctx = copy_one(
            LocalBackend::new(),
            false,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert_eq!(std::fs::read(&dest).unwrap(), new_bytes());
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert!(temporaries(&root.join("dst")).is_empty());
        cleanup(root);
    }

    #[tokio::test]
    async fn overwrite_onto_a_folder_is_refused_and_the_folder_kept() {
        let (root, source, dest) = overwrite_fixture();
        std::fs::remove_file(&dest).unwrap();
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("inside"), OLD).unwrap();
        let ctx = copy_one(
            LocalBackend::new(),
            true,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(matches!(
            ctx.errors.as_slice(),
            [(_, VfsError::IsADirectory { .. })]
        ));
        assert_eq!(std::fs::read(dest.join("inside")).unwrap(), OLD);
        assert!(source.exists());
        assert!(temporaries(&root.join("dst")).is_empty());
        cleanup(root);
    }

    #[tokio::test]
    async fn overwrite_in_a_read_only_folder_fails_without_truncating() {
        if running_as_root() {
            return; // root ignores the folder's mode bits
        }
        let (root, source, dest) = overwrite_fixture();
        let dst = root.join("dst");
        // The file is writable, its folder is not: an in-place write
        // would work, a safe replacement can't, and must not fall back.
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o555)).unwrap();
        let ctx = copy_one(
            LocalBackend::new(),
            false,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            ctx.errors.as_slice(),
            [(_, VfsError::PermissionDenied { .. })]
        ));
        assert_eq!(std::fs::read(&dest).unwrap(), OLD);
        cleanup(root);
    }

    fn running_as_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .is_some_and(|uid| uid.trim() == "0")
    }

    // ── Overwrite onto a symlink replaces the link ──────────────────────

    #[tokio::test]
    async fn overwrite_onto_a_symlink_replaces_the_link_not_its_target() {
        let (root, source, dest) = overwrite_fixture();
        let target = root.join("target.txt");
        std::fs::rename(&dest, &target).unwrap(); // `target` now holds OLD
        std::os::unix::fs::symlink(&target, &dest).unwrap();
        let ctx = copy_one(
            LocalBackend::new(),
            false,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert!(!std::fs::symlink_metadata(&dest).unwrap().is_symlink());
        assert_eq!(std::fs::read(&dest).unwrap(), new_bytes());
        assert_eq!(std::fs::read(&target).unwrap(), OLD, "target untouched");
        cleanup(root);
    }

    // ── a file onto itself ──────────────────────────────────────────────

    /// `dir/f.txt` holding `OLD`, copied or moved onto `dest` (another
    /// spelling of, or a link to, the same file) with `choice`.
    async fn onto_itself(
        is_move: bool,
        choice: ConflictChoice,
        link: impl Fn(&Path) -> PathBuf,
    ) -> (PathBuf, ExecContext) {
        let dir = tempdir();
        let file = dir.join("f.txt");
        std::fs::write(&file, OLD).unwrap();
        let dest = link(&dir);
        let ctx = copy_one(
            LocalBackend::new(),
            is_move,
            choice,
            no_cancel(),
            &file,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert!(!ctx.cancelled);
        assert_eq!(std::fs::read(&file).unwrap(), OLD, "the file is intact");
        assert_eq!(std::fs::read(&dest).unwrap(), OLD);
        assert!(temporaries(&dir).is_empty());
        (dir, ctx)
    }

    fn same_path(dir: &Path) -> PathBuf {
        dir.join("f.txt")
    }

    fn dot_path(dir: &Path) -> PathBuf {
        dir.join(".").join("f.txt")
    }

    fn hard_link(dir: &Path) -> PathBuf {
        let link = dir.join("hard.txt");
        std::fs::hard_link(dir.join("f.txt"), &link).unwrap();
        link
    }

    fn symlink_to_it(dir: &Path) -> PathBuf {
        let link = dir.join("soft.txt");
        std::os::unix::fs::symlink(dir.join("f.txt"), &link).unwrap();
        link
    }

    fn through_symlinked_folder(dir: &Path) -> PathBuf {
        std::os::unix::fs::symlink(dir, dir.join("via")).unwrap();
        dir.join("via").join("f.txt")
    }

    #[tokio::test]
    async fn overwrite_and_skip_leave_a_file_copied_onto_itself_untouched() {
        let spellings: [fn(&Path) -> PathBuf; 5] = [
            same_path,
            dot_path,
            hard_link,
            symlink_to_it,
            through_symlinked_folder,
        ];
        for is_move in [false, true] {
            for choice in [ConflictChoice::Overwrite, ConflictChoice::Skip] {
                for spelling in spellings {
                    let (dir, ctx) = onto_itself(is_move, choice, spelling).await;
                    assert_eq!(ctx.files_done, 1, "counted as done");
                    // Still exactly what was there: a symlink stays a link.
                    if let Ok(meta) = std::fs::symlink_metadata(dir.join("soft.txt")) {
                        assert!(meta.is_symlink());
                    }
                    cleanup(dir);
                }
            }
        }
    }

    #[tokio::test]
    async fn keep_both_onto_itself_makes_an_identical_copy_and_keeps_the_original() {
        for is_move in [false, true] {
            for spelling in [same_path as fn(&Path) -> PathBuf, dot_path, hard_link] {
                let (dir, _ctx) = onto_itself(is_move, ConflictChoice::RenameCopy, spelling).await;
                let copy = std::fs::read_dir(&dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .find(|entry| entry.file_name().to_string_lossy().contains("(copy)"))
                    .expect("a fresh name was written");
                assert_eq!(std::fs::read(copy.path()).unwrap(), OLD);
                cleanup(dir);
            }
        }
    }

    #[tokio::test]
    async fn pasting_into_the_same_folder_through_the_engine_keeps_the_file() {
        // End to end through `run`, as the paste in the issue does it.
        for choice in [
            ConflictChoice::Overwrite,
            ConflictChoice::Skip,
            ConflictChoice::RenameCopy,
        ] {
            let dir = tempdir();
            std::fs::write(dir.join("f.txt"), OLD).unwrap();
            let request = OpRequest::new(
                OpIdSource::default().alloc(),
                OpKind::Copy,
                vec![Location::local(dir.join("f.txt"))],
                Location::local(&dir),
            );
            let mut stream = run(&request);
            let mut conflicts = 0;
            let mut errors = None;
            while let Some(event) = stream.next().await {
                match event {
                    OpEvent::Conflict { reply, .. } => {
                        conflicts += 1;
                        let mut reply = reply;
                        reply
                            .try_send(ConflictDecision {
                                choice,
                                apply_to_all: false,
                            })
                            .unwrap();
                    }
                    OpEvent::Finished { errors: e } => {
                        errors = Some(e);
                        break;
                    }
                    OpEvent::Cancelled => break,
                    _ => {}
                }
            }
            assert_eq!(conflicts, 1, "the prompt is still raised");
            assert_eq!(errors.map(|e| e.len()), Some(0));
            assert_eq!(std::fs::read(dir.join("f.txt")).unwrap(), OLD);
            if choice == ConflictChoice::RenameCopy {
                assert_eq!(std::fs::read(dir.join("f (copy).txt")).unwrap(), OLD);
            }
            assert!(temporaries(&dir).is_empty());
            cleanup(dir);
        }
    }

    // ── Move removes its source only after a complete destination ───────

    #[tokio::test]
    async fn move_removes_the_source_only_after_the_destination_is_closed() {
        let root = tempdir();
        let (source, dest) = (root.join("from.bin"), root.join("to.bin"));
        std::fs::write(&source, new_bytes()).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let ctx = copy_one(
            FaultyBackend::new(Fault::WatchSource(source.clone(), seen.clone())),
            true,
            ConflictChoice::Skip,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert_eq!(*seen.lock().unwrap(), Some(true), "source there at close");
        assert!(!source.exists(), "and removed after");
        assert_eq!(std::fs::read(&dest).unwrap(), new_bytes());
        cleanup(root);
    }

    #[tokio::test]
    async fn a_failed_remote_write_fails_the_copy_and_a_move_keeps_its_source() {
        // A writer that refuses a chunk (as the SFTP writer now reports a
        // server's refusal), or whose close fails: never "done", never a
        // removed source, never a partial file at the destination.
        for fault in [Fault::FailSend(1), Fault::FailSend(3), Fault::FailClose] {
            let root = tempdir();
            let (source, dest) = (root.join("from.bin"), root.join("to.bin"));
            std::fs::write(&source, new_bytes()).unwrap();
            let ctx = copy_one(
                FaultyBackend::new(fault),
                true,
                ConflictChoice::Skip,
                no_cancel(),
                &source,
                &dest,
            )
            .await;
            assert_eq!(ctx.errors.len(), 1);
            assert_eq!(ctx.files_done, 0);
            assert_eq!(std::fs::read(&source).unwrap(), new_bytes());
            assert!(!dest.exists(), "no partial file");
            cleanup(root);
        }
    }

    // ── leftovers and temporary names ───────────────────────────────────

    #[tokio::test]
    async fn crash_leftovers_do_not_break_a_later_overwrite() {
        let (root, source, dest) = overwrite_fixture();
        let dst = root.join("dst");
        // What a killed earlier run (or another instance mid-copy) leaves.
        let leftover = dst.join(format!("{PARTIAL_PREFIX}1-2-0"));
        std::fs::write(&leftover, b"half a file").unwrap();
        let ctx = copy_one(
            LocalBackend::new(),
            false,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert_eq!(std::fs::read(&dest).unwrap(), new_bytes());
        // Not ours to delete: it may belong to a copy still running.
        assert_eq!(std::fs::read(&leftover).unwrap(), b"half a file");
        assert_eq!(temporaries(&dst).len(), 1);
        cleanup(root);
    }

    #[tokio::test]
    async fn a_taken_temporary_name_is_skipped_never_written_through() {
        let dir = tempdir();
        let precious = dir.join("precious");
        std::fs::write(&precious, OLD).unwrap();
        // The first two candidate names are taken: a plain file, and a
        // symlink planted at a predicted name pointing at a precious file.
        std::fs::write(dir.join("taken-1"), b"leftover").unwrap();
        std::os::unix::fs::symlink(&precious, dir.join("taken-2")).unwrap();
        let mut candidates = ["taken-1", "taken-2", "free"].into_iter();
        let mut names = || OsString::from(candidates.next().unwrap_or("exhausted"));
        let (temp, mut sink) = open_temp_sibling(
            &LocalBackend::new(),
            &Location::local(dir.join("dest")),
            &mut names,
        )
        .await
        .unwrap();
        assert_eq!(temp, Location::local(dir.join("free")));
        sink.send(b"new".to_vec()).await.unwrap();
        sink.close().await.unwrap();
        assert_eq!(std::fs::read(dir.join("free")).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("taken-1")).unwrap(), b"leftover");
        assert_eq!(std::fs::read(&precious).unwrap(), OLD);
        cleanup(dir);
    }

    #[test]
    fn temporary_names_are_hidden_recognisable_short_and_unique() {
        let a = temp_name(PARTIAL_PREFIX);
        let b = temp_name(PARTIAL_PREFIX);
        assert_ne!(a, b);
        let a = a.to_string_lossy();
        assert!(a.starts_with(".saola-files-partial-"));
        assert!(a.len() < 80, "fits any NAME_MAX: {a}");
    }

    // ── a backend whose rename refuses an existing target (SFTP) ────────

    #[tokio::test]
    async fn overwrite_on_a_no_replace_backend_moves_the_old_file_aside_then_removes_it() {
        let (root, source, dest) = overwrite_fixture();
        let ctx = copy_one(
            FaultyBackend::new(Fault::None).renames(Renames::NoReplace),
            true,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert!(ctx.errors.is_empty(), "{:?}", ctx.errors);
        assert_eq!(std::fs::read(&dest).unwrap(), new_bytes());
        assert!(!source.exists(), "a successful Move removes its source");
        assert!(temporaries(&root.join("dst")).is_empty());
        cleanup(root);
    }

    #[tokio::test]
    async fn a_no_replace_backend_that_cannot_place_the_copy_restores_the_original() {
        let (root, source, dest) = overwrite_fixture();
        let ctx = copy_one(
            FaultyBackend::new(Fault::None).renames(Renames::NoReplaceNoTemp),
            true,
            ConflictChoice::Overwrite,
            no_cancel(),
            &source,
            &dest,
        )
        .await;
        assert_eq!(ctx.errors.len(), 1);
        assert_eq!(std::fs::read(&dest).unwrap(), OLD, "renamed back");
        assert_eq!(std::fs::read(&source).unwrap(), new_bytes());
        assert!(temporaries(&root.join("dst")).is_empty());
        cleanup(root);
    }
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

    /// A fresh, empty directory for one test.
    ///
    /// The name carries a per-process counter as well as the pid and the
    /// time: tests run on parallel threads, and two threads can read the
    /// same nanosecond (measured: about one name in ten repeated with 16
    /// threads asking at once). Without the counter two tests could share a
    /// directory and delete or overwrite each other's files (issue #106).
    /// `create_dir` (not `create_dir_all`) also refuses a name that somehow
    /// exists already instead of silently sharing it.
    fn tempdir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-ops-test-{}-{}-{n}",
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

        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));

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
        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));

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
        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));

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
        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));

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
        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));
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

        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));
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

        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));
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
        assert!(matches!(events.last(), Some(OpEvent::Finished { errors }) if errors.is_empty()));
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

    // ── a failed destination write is a failed copy, with no partial file ─

    #[tokio::test]
    async fn copying_to_a_full_device_is_an_error_not_a_done_copy() {
        let dir = tempdir();
        let source = dir.join("src.bin");
        std::fs::write(&source, vec![7u8; 3 * 1024 * 1024]).unwrap();
        let backend = crate::modules::local::LocalBackend::new();
        let mut bytes_done = 0;
        let (mut tx, _rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let cancel = AtomicBool::new(false);
        let result = copy_bytes(
            &backend,
            &backend,
            &Location::local(&source),
            &Location::local("/dev/full"),
            // Plain create-or-truncate: `/dev/full` exists, and the point
            // here is the ENOSPC on write, not the exclusive create.
            WriteOptions::default(),
            &mut Transfer {
                cancel: &cancel,
                bytes_done: &mut bytes_done,
                tx: &mut tx,
            },
        )
        .await;
        assert!(result.is_err(), "ENOSPC must not look like success");
        cleanup(dir);
    }

    #[tokio::test]
    async fn an_aborted_write_removes_the_partial_destination() {
        let dir = tempdir();
        let dest_path = dir.join("partial.bin");
        std::fs::write(&dest_path, b"half").unwrap();
        let backend = crate::modules::local::LocalBackend::new();
        // A sink whose every send fails, standing in for a dead writer.
        let mut sink: crate::core::vfs::WriteSink =
            Box::pin(futures::sink::unfold((), |(), _chunk: Vec<u8>| async {
                Err(VfsError::Other {
                    message: "disk full".to_owned(),
                })
            }));
        let _ = sink.send(vec![1]).await;
        let err = abort_write(
            &mut sink,
            &backend,
            &Location::local(&dest_path),
            VfsError::Other {
                message: "fallback".to_owned(),
            },
        )
        .await;
        assert!(matches!(err, VfsError::Other { .. }));
        assert!(!dest_path.exists(), "the partial file is removed");
        cleanup(dir);
    }
}
