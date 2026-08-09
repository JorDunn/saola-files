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
                .then(|| crate::modules::resolve(&request.dest_dir.scheme))
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

    let Some(dest_backend) = crate::modules::resolve(&request.dest_dir.scheme) else {
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

            let Some(source_backend) = crate::modules::resolve(&source.scheme) else {
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
        let Some(backend) = crate::modules::resolve(&source.scheme) else {
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
}
