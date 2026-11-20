//! Copy/move/paste, delete/trash and undo: the active op's event
//! handling and conflict prompt, the internal and foreign clipboards,
//! and the session undo stack. Named `file_ops` rather than `ops` so it
//! doesn't collide with the `core::fs::ops` import `app/mod.rs` uses.

use iced::Task;

use core::clipboard_interop;
use core::fs::{ops, undo};
use core::vfs::Location;
use saola_files::{core, modules, tr, ui};

use super::{App, Message};

/// See `App::pending_conflict`'s doc comment.
pub(super) struct PendingConflict {
    pub(super) conflict: ops::Conflict,
    reply: futures::channel::mpsc::Sender<ops::ConflictDecision>,
    /// The dialog's "Apply to all conflicts" checkbox — round-tripped
    /// through `ui::dialogs::conflict::view` each frame, since that module
    /// holds no state of its own (CLAUDE.md's "ui:: is free of app-window
    /// concerns" posture extended to this dialog too).
    pub(super) apply_to_all: bool,
}

impl App {
    /// Translates one [`ops::OpEvent`] off the active op's stream into
    /// `App` state — `ui::dialogs::progress`/`conflict` never see a raw
    /// event themselves, only what this builds from it (see
    /// `ui::dialogs::progress`'s module doc comment).
    pub(super) fn handle_op_event(&mut self, event: ops::OpEvent) -> Task<Message> {
        match event {
            ops::OpEvent::Started {
                files_total,
                bytes_total,
            } => {
                let kind = self
                    .active_op
                    .as_ref()
                    .map(|request| request.kind)
                    .unwrap_or(ops::OpKind::Copy);
                self.active_op_progress = Some(ui::dialogs::progress::Progress::started(
                    kind,
                    files_total,
                    bytes_total,
                ));
            }
            ops::OpEvent::FileStarted { name } => {
                if let Some(progress) = self.active_op_progress.as_mut() {
                    progress.current_name = Some(name);
                }
            }
            ops::OpEvent::Progress {
                files_done,
                bytes_done,
            } => {
                if let Some(progress) = self.active_op_progress.as_mut() {
                    progress.files_done = files_done;
                    progress.bytes_done = bytes_done;
                }
            }
            ops::OpEvent::Conflict { conflict, reply } => {
                self.active_op_had_conflict = true;
                self.pending_conflict = Some(PendingConflict {
                    conflict,
                    reply,
                    apply_to_all: false,
                });
            }
            ops::OpEvent::Finished { errors } => {
                // No error-dialog surface exists yet (same posture
                // `handle_directory_event`'s spawn failures already take)
                // — worded to stderr rather than silently dropped.
                for (location, err) in &errors {
                    eprintln!("saola-files: {location}: {err}");
                }

                // Stage 10: a clean (zero-conflict) Move is undo-able —
                // see `core::fs::undo`'s module doc comment for exactly
                // why a conflict-touched op is excluded wholesale rather
                // than partially reconstructed. `had_conflict` is read
                // into a local first so this closure doesn't need to
                // borrow `self` a second way while `self.active_op` is
                // already borrowed below.
                let had_conflict = self.active_op_had_conflict;
                let move_undo = self.active_op.as_ref().and_then(|request| {
                    if request.kind != ops::OpKind::Move || had_conflict {
                        return None;
                    }
                    let pairs: Vec<(Location, Location)> = request
                        .sources
                        .iter()
                        .filter(|source| !errors.iter().any(|(loc, _)| loc == *source))
                        .filter_map(|source| {
                            let name = source.path.file_name()?;
                            let to = request.dest_dir.join(name);
                            undo::can_undo_rename(source, &to).then(|| (source.clone(), to))
                        })
                        .collect();
                    (!pairs.is_empty()).then_some(undo::UndoEntry::Move { pairs })
                });

                self.active_op = None;
                self.active_op_progress = None;
                self.pending_conflict = None;
                if let Some(entry) = move_undo {
                    self.push_undo(entry);
                }
            }
            ops::OpEvent::Cancelled => {
                self.active_op = None;
                self.active_op_progress = None;
                self.pending_conflict = None;
            }
        }
        Task::none()
    }

    /// Sends the human's choice back down the pending conflict's reply
    /// channel — `try_send` (CLAUDE.md's bounded-bridging rule), and a
    /// failure (the engine gave up waiting — see `core::fs::ops::
    /// send_event`'s doc comment) is silently dropped rather than
    /// retried: the op has already moved on by the time that could happen.
    pub(super) fn answer_conflict(&mut self, choice: ops::ConflictChoice) {
        let Some(mut pending) = self.pending_conflict.take() else {
            return;
        };
        let _ = pending.reply.try_send(ops::ConflictDecision {
            choice,
            apply_to_all: pending.apply_to_all,
        });
    }

    /// `Event::PasteRequested`'s handling: the internal clipboard
    /// (`core::fs::ops::Clipboard`, "authoritative" per CLAUDE.md) wins
    /// when it has contents; an *empty* internal clipboard falls back to a
    /// best-effort read of the foreign (Wayland) clipboard (Stage 10) —
    /// `core::clipboard_interop::read`, which never blocks this call (it's
    /// dispatched as its own `Task::perform`, landing later as
    /// `Message::ForeignPasteReady`) and never fails loudly (a foreign
    /// read that finds nothing, or can't reach a compositor at all, just
    /// resolves to `Message::Noop` — the same silent no-op an empty
    /// internal clipboard already was before this stage). While another op
    /// is already running, both paths are a no-op — Stage 8 deliberately
    /// doesn't queue a second op (see the Stage 8 handoff for what a real
    /// queue would need).
    pub(super) fn start_paste(&mut self, dest_dir: Location) -> Task<Message> {
        if self.active_op.is_some() {
            return Task::none();
        }
        if let Some(clipboard_op) = self.clipboard.op() {
            let sources = self.clipboard.locations().to_vec();
            if !sources.is_empty() {
                let kind = match clipboard_op {
                    ops::ClipboardOp::Copy => ops::OpKind::Copy,
                    ops::ClipboardOp::Cut => ops::OpKind::Move,
                };
                if clipboard_op == ops::ClipboardOp::Cut {
                    self.clipboard.clear();
                }
                return self.submit_paste_op(kind, sources, dest_dir);
            }
        }

        Task::perform(clipboard_interop::read(), move |foreign| match foreign {
            Some(clipboard_interop::ForeignClipboard { op, locations })
                if !locations.is_empty() =>
            {
                Message::ForeignPasteReady(op, locations, dest_dir)
            }
            _ => Message::Noop,
        })
    }

    /// Builds and submits the actual [`ops::OpRequest`] — the tail end
    /// both `Self::start_paste`'s internal-clipboard path and
    /// `Message::ForeignPasteReady`'s foreign-clipboard path funnel
    /// through, so there is exactly one place an op is ever actually
    /// started from a paste (Stage 10). Re-checks `active_op.is_some()`
    /// (already checked once in `start_paste`, before the foreign-read
    /// `Task` was even dispatched) because a foreign read is asynchronous:
    /// another paste — or any other op — could have started in the
    /// meantime.
    pub(super) fn submit_paste_op(
        &mut self,
        kind: ops::OpKind,
        sources: Vec<Location>,
        dest_dir: Location,
    ) -> Task<Message> {
        if self.active_op.is_some() || sources.is_empty() {
            return Task::none();
        }
        let id = self.op_ids.alloc();
        self.active_op_had_conflict = false;
        self.active_op = Some(ops::OpRequest::new(id, kind, sources, dest_dir));
        Task::none()
    }

    /// Pushes `entry` onto the undo stack and (re)seeds the undo toast
    /// from its label — every push call site in `src/app/` goes through
    /// this one method rather than touching `undo_stack`/`undo_toast`
    /// separately, keeping `undo_toast`'s "always reflects the current
    /// top of the stack" invariant (see `App::undo_toast`'s own doc
    /// comment) in exactly one place.
    pub(super) fn push_undo(&mut self, entry: undo::UndoEntry) {
        let label = entry.label();
        self.undo_stack.push(entry);
        self.undo_toast = Some(ui::dialogs::undo_toast::Toast::new(
            label,
            std::time::Instant::now(),
        ));
    }

    /// Ctrl+Z / the undo toast's own "Undo" button: pops the most recent
    /// invertible entry and inverts it. Pops *before* the async `apply`
    /// call starts (see `undo::UndoStack::pop`'s doc comment — there is no
    /// redo to put a failed entry back for) and clears `undo_toast`
    /// unconditionally, since whatever it was showing is necessarily the
    /// entry just popped (`Self::push_undo`'s invariant). A failure is
    /// worded to stderr, the same posture every other backend failure in
    /// this file already takes.
    pub(super) fn start_undo(&mut self) -> Task<Message> {
        let Some(entry) = self.undo_stack.pop() else {
            return Task::none();
        };
        self.undo_toast = None;
        Task::perform(undo::apply(entry), |result| {
            if let Err(err) = result {
                eprintln!("saola-files: couldn't undo: {err}");
            }
            Message::Noop
        })
    }

    /// `Event::DeleteRequested`'s handling (Stage 9): trash where the
    /// backend supports it and `mode` allows it, permanent delete
    /// otherwise — capability-honest per `Caps::TRASH`, see
    /// `ui::dirview::DeleteMode`'s doc comment for what decides which.
    /// Fire-and-forget, like every other `handle_directory_event` arm that
    /// isn't Copy/Cut/Paste: each item's trash-move (or permanent delete)
    /// is one fast rename/recursive-remove syscall, not a streamed op, so
    /// there's nothing here for `core::fs::ops`'s progress strip to drive
    /// — see the Stage 9 handoff for the full reasoning. This deliberately
    /// does **not** wait for a result to remove the row from the view:
    /// the local backend's `Caps::WATCH` inotify stream already reports
    /// the resulting `DirEvent::Removed`/`Renamed` (a cross-directory
    /// `rename(2)` into the trash surfaces as a `MOVED_FROM` with no
    /// paired `MOVED_TO`, which `modules::local`'s own watch bridge already
    /// turns into `Removed` once its pairing window closes — see that
    /// module's docs), so `ui::dirview::DirectoryView::apply_watch_events`
    /// updates the row on its own, the same "the view never optimistically
    /// edits itself for something the backend will tell it about anyway"
    /// posture watch-driven changes already have everywhere else. Errors
    /// are worded to stderr (no error-toast surface exists yet, the same
    /// posture every other spawn/backend failure in this function already
    /// takes) — a row that fails to delete simply stays put, or reappears
    /// on the next watch/F5 refresh.
    pub(super) fn start_delete(
        &mut self,
        locations: Vec<Location>,
        mode: ui::dirview::DeleteMode,
    ) -> Task<Message> {
        let force_permanent = mode == ui::dirview::DeleteMode::Permanent;
        let tasks: Vec<Task<Message>> = locations
            .into_iter()
            .map(|location| {
                let original = location.clone();
                Task::perform(delete_one(location, force_permanent), move |result| {
                    match result {
                        // Stage 10: a trash-delete is undo-able — see
                        // `Message::Trashed`'s doc comment.
                        Ok(Some(id)) => Message::Trashed(original, id),
                        Ok(None) => Message::Noop,
                        Err((location, err)) => {
                            eprintln!("saola-files: couldn't delete {location}: {err}");
                            Message::Noop
                        }
                    }
                })
            })
            .collect();
        Task::batch(tasks)
    }
}

/// `Event::CopyRequested`/`CutRequested`'s foreign-clipboard half (Stage
/// 10): writes `locations` to the Wayland clipboard alongside the
/// existing `self.clipboard.set_copy`/`set_cut` call that already updates
/// this app's *internal* clipboard. A free function (not an `App` method)
/// since it touches no `App` state — plain `Task::perform` composition,
/// the same shape `directory_task`/`trash_task` already use. A failure
/// (most commonly: no Wayland compositor) is worded to stderr; this app's
/// own internal clipboard/paste already worked before this stage existed
/// and keeps working regardless of whether this succeeds.
pub(super) fn write_foreign_clipboard(
    op: ops::ClipboardOp,
    locations: Vec<Location>,
) -> Task<Message> {
    Task::perform(clipboard_interop::write(op, locations), |result| {
        if let Err(err) = result {
            eprintln!("saola-files: couldn't update the system clipboard: {err}");
        }
        Message::Noop
    })
}

/// One location's half of `App::start_delete`: trashes it when its
/// backend claims `Caps::TRASH` and `force_permanent` wasn't requested
/// (Shift+Delete), permanently deletes it otherwise. Local-only for now on
/// both branches — `core::fs::trash` is local-only by nature (see its
/// module doc comment), and the non-local permanent-delete branch below
/// falls back to a single non-recursive `Backend::remove`, which is a
/// stated gap for a future non-local backend without `Caps::TRASH` (SFTP,
/// Stage 13): deleting a non-empty remote directory that way would fail
/// rather than recurse. No such backend exists yet, so this isn't
/// reachable today — flagged here for whichever stage adds one.
/// **Stage 10 change:** returns `Ok(Some(TrashId))` for a successful trash
/// (was `Ok(())`) so `start_delete` can carry it back as `Message::Trashed`
/// for `App::push_undo` — the hook point the Stage 9 handoff named
/// ("currently the `TrashId` is discarded with `.map(|_id| ())`"). A
/// permanent delete (either branch) still has nothing to undo, so it stays
/// `Ok(None)`.
async fn delete_one(
    location: Location,
    force_permanent: bool,
) -> Result<Option<core::fs::trash::TrashId>, (Location, String)> {
    let Some(backend) = modules::resolve(&location) else {
        return Err((
            location.clone(),
            tr!("error-no-backend", scheme = location.scheme.as_str()),
        ));
    };
    let caps = backend.caps();

    if location.is_local() && !force_permanent && caps.contains(core::vfs::Caps::TRASH) {
        let path = location.path.clone();
        return core::fs::trash::trash(&path)
            .map(Some)
            .map_err(|err| (location.clone(), err.to_string()));
    }
    if location.is_local() {
        let path = location.path.clone();
        return core::fs::trash::delete_permanently(&path)
            .map(|()| None)
            .map_err(|err| (location.clone(), err.to_string()));
    }
    backend
        .remove(&location)
        .await
        .map(|()| None)
        .map_err(|err| (location.clone(), err.to_string()))
}
