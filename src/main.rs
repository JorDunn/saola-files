//! saola-files — the file manager for the Saola desktop environment.
//!
//! Current scope (through Stage 10): the application shell (Stage 1), CLI
//! parsing and `files.toml` loading (Stage 2), real directory browsing
//! through a VFS `Backend` trait and a local backend, navigation chrome
//! and live updates (Stages 4–5), mime/icons/opening (Stage 6), the places
//! sidebar (Stage 7), copy/move/rename/new-folder/new-file with live
//! progress and conflict prompts (Stage 8), Trash (Stage 9), and now a
//! session undo stack plus foreign (Wayland) clipboard interop (Stage 10).
//!
//! This binary is a thin shell over the `saola_files` library crate
//! (`src/lib.rs`) — see that file's docs for why the split exists.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use iced::{Element, Fill, Size, Subscription, Task, window};
use saola_theme::{Theme, convert};

use core::clipboard_interop;
use core::fs::{ops, undo};
use core::vfs::Location;
use saola_files::{cli, config, core, modules, ui};

/// The App-owned decoded-thumbnail LRU's capacity (Stage 11 — CLAUDE.md's
/// "~512-handle LRU"). A plain implementation constant, the same posture
/// `modules::local`'s `CHANNEL_CAPACITY`/`WATCH_CHANNEL_CAPACITY` already
/// take for their own bounded-resource sizes: not derived from the theme
/// (it bounds a background cache, not anything rendered), not a config
/// knob (nothing in `files.toml`'s documented shape covers it, and a
/// wrong value here degrades gracefully — a smaller cache just means more
/// re-decodes on scroll-back, never a correctness issue).
const THUMB_LRU_CAPACITY: usize = 512;

/// How many thumbnails may be generated concurrently (Stage 11 — CLAUDE.md:
/// "never unbounded task spawns"). Small on purpose: thumbnail generation
/// is blocking CPU work (image decode + resize) sharing the same blocking
/// pool `modules::local`'s file I/O already uses; a handful at a time keeps
/// a 5k-image directory's initial scroll responsive rather than saturating
/// every blocking-pool thread with decode work at once.
const THUMB_MAX_CONCURRENT: usize = 4;

fn main() -> iced::Result {
    let invocation = match cli::parse(std::env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("saola-files: {message}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let args = match invocation {
        cli::Invocation::Version => {
            println!("saola-files {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        cli::Invocation::Help => {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        cli::Invocation::Run(args) => args,
    };

    let config_path = config::Config::resolve_path(args.config_dir.as_deref());
    let config = config::Config::load(config_path.as_deref());

    // `default_font` wants an owned `Font` up front, before any `App`
    // exists, so build a throwaway theme just for the font lookup. (The
    // `Box::leak` this implies is saola-theme's documented, once-per-load
    // exception — see saola-theme's convert.rs.)
    let ui_font = convert::ui_font(&Theme::saola());

    iced::application(
        move || App::new(config.clone(), args.clone()),
        App::update,
        App::view,
    )
    .theme(App::theme)
    // Transparent app background: without this, iced clears the surface
    // to the theme's ink background before drawing, and the window's
    // rounded corners render as square ink wedges (capture learned this
    // live — see ui/window.rs's module docs).
    .style(App::style)
    .subscription(App::subscription)
    .title("Files")
    .default_font(ui_font)
    .window(window::Settings {
        // niri draws no decorations and Saola windows draw their own
        // header; there is no taskbar, so minimise must not exist.
        decorations: false,
        transparent: true,
        minimizable: false,
        ..window::Settings::default()
    })
    .window_size(Size::new(1100.0, 720.0))
    .run()
}

#[derive(Debug, Clone)]
enum Message {
    Window(ui::window::Event),
    Explorer(ui::explorer::Message),
    /// Stage 8: an event off the active op's progress stream
    /// (`ui::dialogs::progress::subscription`).
    OpEvent(ops::OpEvent),
    /// The ops strip's Cancel button.
    Progress(ui::dialogs::progress::Message),
    /// The conflict dialog's buttons/checkbox.
    Conflict(ui::dialogs::conflict::Message),
    /// The Trash browser (Stage 9) — routed here rather than nested inside
    /// `ui::explorer::Message` the way `sidebar`/`dirview` are, because
    /// `TrashView` isn't part of that portal seam at all (see
    /// `ui::trashview`'s module doc comment): it's a separate top-level
    /// surface `App` swaps in for the explorer body, not a child of it.
    Trash(ui::trashview::Message),
    /// The undo toast's own tick/button messages (Stage 10) — same
    /// top-level status as `Progress`/`Conflict`, for the same reason
    /// (`ui::dialogs::undo_toast` isn't part of `ui::explorer`'s portal
    /// seam either).
    UndoToast(ui::dialogs::undo_toast::Message),
    /// A best-effort foreign-clipboard read (`core::clipboard_interop::
    /// read`) came back with something to paste — `Event::PasteRequested`'s
    /// fallback path when the internal clipboard is empty (Stage 10). Carries
    /// the same `(op, locations, dest_dir)` shape `Self::start_paste`
    /// already builds an `ops::OpRequest` from for the internal-clipboard
    /// case; see `Self::submit_paste_op`, which both paths funnel through.
    ForeignPasteReady(ops::ClipboardOp, Vec<Location>, Location),
    /// `Self::start_delete`'s trash branch succeeded (Stage 10): `original`
    /// is where the item was before, `TrashId` is everything
    /// `core::fs::trash::restore` needs to undo it — see the free function
    /// `delete_one`'s doc comment. Never fired for a permanent delete
    /// (nothing to undo) or a failure (worded to stderr instead, same as
    /// before this stage).
    Trashed(Location, core::fs::trash::TrashId),
    /// A background thumbnail generation (`core::thumbs::thumbnail_for`)
    /// finished (Stage 11) — `location`/`modified` identify exactly which
    /// request this answers (mirrors `Message::RenameResult`'s staleness-
    /// guard shape, though here the guard is "is this still in
    /// `thumb_inflight`", not a location comparison — see
    /// `Self::request_thumbnails`'s doc comment). `None` means "no
    /// thumbnail" — unsupported mimetype, decode failure, or the file
    /// simply isn't local; the row keeps its glyph icon.
    ThumbnailReady(Location, SystemTime, Option<core::thumbs::ThumbHandle>),
    /// Swallows a click on the conflict dialog's modal scrim — the dialog
    /// must be answered via one of its three buttons, never dismissed by
    /// clicking outside it (there is no sane default resolution to assume).
    /// The iced 0.14 gotcha CLAUDE.md documents: a `mouse_area` needs a
    /// real `.on_press` to actually capture the click, the same "no
    /// `.on_press` = doesn't capture its press" behavior a bare `button`
    /// has (`ui::menus`'s popover scrim is the same trick, there closing
    /// the menu instead of doing nothing).
    Noop,
}

struct App {
    theme: Theme,
    /// The tabs seam (CLAUDE.md: "the app holds `Vec<DirectoryView> +
    /// active`") — Stage 3 only ever shows one.
    views: Vec<ui::dirview::DirectoryView>,
    active: usize,
    /// The places sidebar (Stage 7) — its own self-contained state
    /// (`core::places::Place`s built once at startup, `core::udisks::
    /// Mount`s updated live), composed beside `views`/`active` by
    /// `ui::explorer::view` rather than folded into the tabs seam: unlike
    /// a `DirectoryView`, there is exactly one sidebar for the whole app,
    /// not one per tab.
    sidebar: ui::sidebar::Sidebar,
    /// Shared caches (CLAUDE.md: "Shared caches (thumbs, mime, apps, …)
    /// live on the App, never per-view") — built once at startup, since
    /// both walk real filesystem trees (`$XDG_DATA_DIRS/mime`,
    /// `$XDG_DATA_DIRS/applications`) that don't change while the app is
    /// running.
    mime_db: core::mime::MimeDb,
    apps_db: core::apps::AppsDb,
    /// `files.toml`'s `terminal` knob, the one piece of `Config` this app
    /// still needs after startup (every other knob is baked into the
    /// first `DirectoryView` at construction and never read back — see
    /// `Self::new`'s comment on `config`). Resolved the rest of the way
    /// (`$TERMINAL`, then `alacritty`) at the point of use via
    /// `core::apps::resolve_terminal_from_env`, not cached here — it's a
    /// cheap env read, not worth a second cache.
    terminal: Option<String>,
    /// The internal clipboard (Stage 8) — a shared cache like `mime_db`/
    /// `apps_db`/`sidebar`, not per-view: Ctrl+C in one tab and Ctrl+V in
    /// another (once tabs exist) must see the same clipboard.
    clipboard: ops::Clipboard,
    /// Monotonic [`ops::OpId`] allocator — one per `App`, per
    /// `ops::OpIdSource`'s own doc comment.
    op_ids: ops::OpIdSource,
    /// The one currently-running copy/move op, if any. Stage 8 never runs
    /// more than one at a time — see `Self::start_paste`'s doc comment for
    /// why a second paste while one is in flight is dropped rather than
    /// queued.
    active_op: Option<ops::OpRequest>,
    /// The live progress snapshot for `active_op`, accumulated from its
    /// event stream by `Self::handle_op_event`. Always `Some` exactly when
    /// `active_op` is (kept as two `Option`s rather than one combined type
    /// only because `active_op` needs to be handed to
    /// `ui::dialogs::progress::subscription` by itself, without also
    /// borrowing the progress snapshot `App::subscription` doesn't touch).
    active_op_progress: Option<ui::dialogs::progress::Progress>,
    /// The conflict the active op is waiting on an answer for, if any —
    /// `App`'s half of the capacity-1 reply-channel pattern (CLAUDE.md; see
    /// `core::fs::ops`'s module docs).
    pending_conflict: Option<PendingConflict>,
    /// The Trash browser (Stage 9) — always constructed, like `sidebar`
    /// (not `Option`), so its list survives being switched away from and
    /// back to without reloading from scratch mid-transition. `files.
    /// toml`'s `confirm-empty-trash` knob is baked into it at construction,
    /// the same "config knob becomes fixed per-surface state" posture
    /// `DirectoryView::new` already takes for its own config-derived
    /// fields.
    trash_view: ui::trashview::TrashView,
    /// Whether the Trash browser is what's currently showing in place of
    /// the ordinary explorer body — set by `navigate_active` the moment
    /// the sidebar's Trash place (`core::places::trash_location()`) is
    /// clicked, cleared the moment any other place/mount is. `views`/
    /// `active` (the tabs seam) are untouched either way: switching into
    /// Trash doesn't navigate the hidden `DirectoryView` anywhere, it's
    /// still pointed at whatever it was showing before, and switching back
    /// out shows exactly that again.
    trash_active: bool,
    /// The session undo stack (Stage 10) — a shared cache like
    /// `clipboard`/`op_ids`, never per-view (CLAUDE.md). See
    /// `core::fs::undo`'s module doc comment for exactly what's pushed and
    /// what's a dead end.
    undo_stack: undo::UndoStack,
    /// Set to `true` the moment the *currently running* op reports even
    /// one `OpEvent::Conflict` — `Self::handle_op_event`'s `Finished` arm
    /// reads this to decide whether the just-finished op is safe to push
    /// onto `undo_stack` at all (see `core::fs::undo`'s module doc comment,
    /// "a Move that hit even one conflict prompt is never pushed"). Reset
    /// to `false` every time `Self::submit_paste_op` starts a fresh op —
    /// this is deliberately *not* part of `active_op`/`OpRequest` itself
    /// (which is `Clone`d out to the progress subscription and has no
    /// natural place for one-shot bookkeeping like this).
    active_op_had_conflict: bool,
    /// The undo toast currently fading in/showing/fading out in the ops
    /// strip, if any — `Self::push_undo` sets this every time it pushes
    /// onto `undo_stack`, so it always reflects the current top of the
    /// stack (see `undo::UndoStack::peek_label`'s own doc comment on why
    /// that invariant needs no extra bookkeeping to hold: undoing always
    /// pops *and* clears this together, in `Self::start_undo`).
    undo_toast: Option<ui::dialogs::undo_toast::Toast>,
    /// The ~512-handle LRU of decoded thumbnails (Stage 11) — a shared
    /// cache like `clipboard`/`undo_stack`, never per-view. See
    /// `core::thumbs::ThumbCache`'s own doc comment for why it's keyed by
    /// `(Location, mtime)` rather than `Location` alone.
    thumb_cache: core::thumbs::ThumbCache,
    /// The thumbnailer registry (Stage 11) — built once at startup, `Arc`'d
    /// so `Self::request_thumbnails` can clone it cheaply into each
    /// `Task::perform`'d generation.
    thumb_registry: Arc<core::thumbs::Registry>,
    /// Bounds concurrent thumbnail generation (Stage 11) — see
    /// `THUMB_MAX_CONCURRENT`'s doc comment.
    thumb_semaphore: Arc<tokio::sync::Semaphore>,
    /// `$XDG_CACHE_HOME/thumbnails` (or `~/.cache/thumbnails`), resolved
    /// once at startup — `None` when neither is derivable (no `$HOME`,
    /// e.g. a minimal CI sandbox), in which case thumbnails are still
    /// generated per-request but never persisted to disk (see
    /// `core::thumbs::generate_blocking`'s handling of a `None` cache
    /// root).
    thumb_cache_root: Option<PathBuf>,
    /// Locations with a generation request currently in flight (Stage 11)
    /// — prevents `Self::request_thumbnails` from dispatching a second
    /// generation for the same file while the first hasn't come back yet
    /// (e.g. two `Scrolled` batches in a row both covering the same rows).
    /// Cleared the moment `Message::ThumbnailReady` lands, success or not.
    thumb_inflight: HashSet<Location>,
    /// Locations known to have failed thumbnailing this session, keyed to
    /// the exact `mtime` that failed (Stage 11) — a file that changes
    /// (fresh `mtime`) gets a fresh attempt, but a file that fails
    /// repeatedly at the *same* version isn't retried on every scroll.
    /// **Not** the spec's persistent `fail/` cache directory (see
    /// `core::thumbs`'s module doc comment's "Known gaps") — this resets
    /// every app restart.
    thumb_failed: HashMap<Location, SystemTime>,
    /// `files.toml`'s `thumbnails` knob — `Self::request_thumbnails`'
    /// first gate, checked before any of the more granular ones.
    thumbnails_enabled: bool,
    /// `files.toml`'s `thumbnail-max-mb` knob, unconverted — passed to
    /// `core::thumbs::exceeds_max_size` at the point of use rather than
    /// pre-multiplied into bytes here, so that pure predicate stays the
    /// one place the MiB-to-bytes conversion happens.
    thumbnail_max_mb: u64,
}

/// See `App::pending_conflict`'s doc comment.
struct PendingConflict {
    conflict: ops::Conflict,
    reply: futures::channel::mpsc::Sender<ops::ConflictDecision>,
    /// The dialog's "Apply to all conflicts" checkbox — round-tripped
    /// through `ui::dialogs::conflict::view` each frame, since that module
    /// holds no state of its own (CLAUDE.md's "ui:: is free of app-window
    /// concerns" posture extended to this dialog too).
    apply_to_all: bool,
}

impl App {
    fn new(config: config::Config, args: cli::Cli) -> (Self, Task<Message>) {
        // `$HOME` unset (e.g. a minimal CI sandbox) falls back to `/`
        // rather than failing to start — the config loader takes the same
        // posture for its own directory chain.
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));

        // The sidebar's place list (Stage 7): built once, the same
        // "walk it at startup, never again" posture `mime_db`/`apps_db`
        // already take below — `core::places::UserDirs::load`/
        // `load_bookmarks` degrade to empty on a missing/unreadable file,
        // never fail `App::new` itself.
        let user_dirs = core::places::UserDirs::load(&home);
        let bookmarks = core::places::load_bookmarks(&home);
        let places = core::places::build(&home, &user_dirs, &bookmarks, &config.servers);

        let fallback = Location::local(home);

        // Most of `config` is consumed entirely here (view/sort/
        // show-hidden/actions defaults baked into the first
        // `DirectoryView` at construction, custom actions cloned onto it
        // too); `App` only keeps `terminal` back out as its own field
        // (Stage 6 — resolving "open in terminal"/`Terminal=true` apps
        // needs it after startup, not just at construction). A future
        // "new tab" action that needs the rest of these defaults would
        // resolve `Config::load` again or thread a clone through at that
        // point.
        let (view, task) = if let Some(select) = args.select {
            ui::dirview::DirectoryView::open_select(select, fallback, &config)
        } else if let Some(target) = args.target {
            ui::dirview::DirectoryView::open_target(target, fallback, &config)
        } else {
            ui::dirview::DirectoryView::open(fallback, &config)
        };

        let app = App {
            theme: Theme::saola(),
            views: vec![view],
            active: 0,
            sidebar: ui::sidebar::Sidebar::new(places),
            mime_db: core::mime::MimeDb::new(),
            apps_db: core::apps::AppsDb::load(),
            terminal: config.terminal.clone(),
            clipboard: ops::Clipboard::new(),
            op_ids: ops::OpIdSource::default(),
            active_op: None,
            active_op_progress: None,
            pending_conflict: None,
            trash_view: ui::trashview::TrashView::new(config.confirm_empty_trash),
            trash_active: false,
            undo_stack: undo::UndoStack::new(),
            active_op_had_conflict: false,
            undo_toast: None,
            thumb_cache: core::thumbs::ThumbCache::new(THUMB_LRU_CAPACITY),
            thumb_registry: Arc::new(core::thumbs::Registry::with_defaults()),
            thumb_semaphore: Arc::new(tokio::sync::Semaphore::new(THUMB_MAX_CONCURRENT)),
            thumb_cache_root: core::thumbs::resolve_cache_root(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashMap::new(),
            thumbnails_enabled: config.thumbnails,
            thumbnail_max_mb: config.thumbnail_max_mb,
        };
        (
            app,
            task.map(|m| Message::Explorer(ui::explorer::Message::Directory(m))),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Window(event) => ui::window::update(event),
            Message::Explorer(ui::explorer::Message::Sidebar(inner)) => {
                match self.sidebar.update(inner) {
                    Some(ui::sidebar::Event::OpenDirectory(location)) => {
                        self.navigate_active(location)
                    }
                    None => Task::none(),
                }
            }
            Message::Explorer(ui::explorer::Message::Directory(inner)) => {
                let Some(view) = self.views.get_mut(self.active) else {
                    return Task::none();
                };
                let (task, event) = view.update(inner);
                let mut tasks = vec![directory_task(task)];
                if let Some(event) = event {
                    tasks.push(self.handle_directory_event(event));
                }
                Task::batch(tasks)
            }
            Message::OpEvent(event) => self.handle_op_event(event),
            Message::Progress(ui::dialogs::progress::Message::CancelRequested) => {
                if let Some(request) = &self.active_op {
                    request.request_cancel();
                }
                Task::none()
            }
            Message::Conflict(ui::dialogs::conflict::Message::ChoiceSelected(choice)) => {
                self.answer_conflict(choice);
                Task::none()
            }
            Message::Conflict(ui::dialogs::conflict::Message::ApplyToAllToggled(value)) => {
                if let Some(pending) = self.pending_conflict.as_mut() {
                    pending.apply_to_all = value;
                }
                Task::none()
            }
            Message::Noop => Task::none(),
            Message::Trash(inner) => trash_task(self.trash_view.update(inner)),
            Message::UndoToast(ui::dialogs::undo_toast::Message::UndoClicked) => self.start_undo(),
            Message::UndoToast(ui::dialogs::undo_toast::Message::Tick) => {
                if let Some(toast) = &self.undo_toast
                    && toast.expired(&self.theme, std::time::Instant::now())
                {
                    self.undo_toast = None;
                }
                Task::none()
            }
            Message::ForeignPasteReady(op, locations, dest_dir) => {
                let kind = match op {
                    ops::ClipboardOp::Copy => ops::OpKind::Copy,
                    ops::ClipboardOp::Cut => ops::OpKind::Move,
                };
                self.submit_paste_op(kind, locations, dest_dir)
            }
            Message::Trashed(original, id) => {
                self.push_undo(undo::UndoEntry::Trash { id, original });
                Task::none()
            }
            Message::ThumbnailReady(location, modified, handle) => {
                self.thumb_inflight.remove(&location);
                match handle {
                    Some(handle) => {
                        self.thumb_cache.insert(location.clone(), modified, handle);
                        self.thumb_failed.remove(&location);
                    }
                    None => {
                        self.thumb_failed.insert(location, modified);
                    }
                }
                Task::none()
            }
        }
    }

    /// Translates one [`ops::OpEvent`] off the active op's stream into
    /// `App` state — `ui::dialogs::progress`/`conflict` never see a raw
    /// event themselves, only what this builds from it (see
    /// `ui::dialogs::progress`'s module doc comment).
    fn handle_op_event(&mut self, event: ops::OpEvent) -> Task<Message> {
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
    fn answer_conflict(&mut self, choice: ops::ConflictChoice) {
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
    fn start_paste(&mut self, dest_dir: Location) -> Task<Message> {
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
    fn submit_paste_op(
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
    /// from its label — every push call site in this file goes through
    /// this one method rather than touching `undo_stack`/`undo_toast`
    /// separately, keeping `undo_toast`'s "always reflects the current
    /// top of the stack" invariant (see `App::undo_toast`'s own doc
    /// comment) in exactly one place.
    fn push_undo(&mut self, entry: undo::UndoEntry) {
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
    fn start_undo(&mut self) -> Task<Message> {
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

    /// Navigates the active `DirectoryView` to `location` — the shared
    /// tail end of both `ui::sidebar::Event::OpenDirectory` (a places-row
    /// click) and `ui::dirview::Event::OpenDirectory` (ascend/breadcrumb/
    /// descend) below, since both ultimately mean the same thing: "show
    /// this location in the one tab there currently is."
    ///
    /// **Stage 9's one exception:** the sidebar's Trash place navigates to
    /// `core::places::trash_location()`, a sentinel no backend actually
    /// serves (see that function's doc comment) — caught here, before it
    /// ever reaches a `DirectoryView`, and swapped for switching
    /// `trash_active` on and loading `trash_view` instead. `ui::dirview`'s
    /// own `Event::OpenDirectory` (breadcrumbs/ascend/descend) can never
    /// actually produce this sentinel — a directory view only ever joins
    /// child names onto its *own* location, which is never the trash
    /// scheme — so this check is cheap and never fires from that path in
    /// practice; it's here because both callers share this one function,
    /// not because both need it.
    fn navigate_active(&mut self, location: Location) -> Task<Message> {
        if location == core::places::trash_location() {
            self.trash_active = true;
            return trash_task(self.trash_view.load());
        }
        self.trash_active = false;

        let Some(view) = self.views.get_mut(self.active) else {
            return Task::none();
        };
        directory_task(view.navigate(location))
    }

    /// The owner's response to a `DirectoryView` `Event` — the view only
    /// ever requests a navigation or an open/spawn, never performs one
    /// itself (see `ui::dirview`'s module docs). Every branch here is a
    /// synchronous `Command::spawn` (a detached process launch returns
    /// immediately — there's nothing to `.await`), so none of these need
    /// an `iced::Task`; a spawn failure is worded to stderr rather than
    /// surfaced in the UI (no error-dialog surface exists yet, and a
    /// failed launch is exactly the kind of thing CLAUDE.md's no-panic
    /// posture says should degrade quietly, not take the app down).
    fn handle_directory_event(&mut self, event: ui::dirview::Event) -> Task<Message> {
        match event {
            ui::dirview::Event::OpenDirectory(location) => self.navigate_active(location),
            ui::dirview::Event::Activated(locations) => {
                self.open_with_default_app(&locations);
                Task::none()
            }
            ui::dirview::Event::OpenWith(locations, desktop_id) => {
                let Some(entry) = self.apps_db.entry(&desktop_id).cloned() else {
                    eprintln!("saola-files: {desktop_id} is no longer installed");
                    return Task::none();
                };
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                if let Err(err) = core::apps::open(&entry, &paths, &terminal) {
                    eprintln!("saola-files: could not open with {}: {err}", entry.name);
                }
                Task::none()
            }
            ui::dirview::Event::OpenTerminal(location, is_dir) => {
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                if let Err(err) = core::apps::open_terminal_here(&terminal, is_dir, &location.path)
                {
                    eprintln!("saola-files: could not open a terminal at {location}: {err}");
                }
                Task::none()
            }
            ui::dirview::Event::RunCustomAction(exec, locations) => {
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                for argv in core::apps::build_argv(&exec, &paths) {
                    let Some((program, args)) = argv.split_first() else {
                        continue;
                    };
                    if let Err(err) = core::apps::spawn_argv(program, args) {
                        eprintln!("saola-files: custom action \"{exec}\" failed: {err}");
                    }
                }
                Task::none()
            }
            ui::dirview::Event::CopyRequested(locations) => {
                self.clipboard.set_copy(locations.clone());
                write_foreign_clipboard(ops::ClipboardOp::Copy, locations)
            }
            ui::dirview::Event::CutRequested(locations) => {
                self.clipboard.set_cut(locations.clone());
                write_foreign_clipboard(ops::ClipboardOp::Cut, locations)
            }
            ui::dirview::Event::PasteRequested(dest_dir) => self.start_paste(dest_dir),
            ui::dirview::Event::DeleteRequested(locations, mode) => {
                self.start_delete(locations, mode)
            }
            ui::dirview::Event::Renamed(from, to) => {
                if undo::can_undo_rename(&from, &to) {
                    self.push_undo(undo::UndoEntry::Rename { from, to });
                }
                Task::none()
            }
            ui::dirview::Event::Created(created) => {
                self.push_undo(undo::UndoEntry::New { created });
                Task::none()
            }
            ui::dirview::Event::UndoRequested => self.start_undo(),
            ui::dirview::Event::ThumbnailsNeeded(candidates) => self.request_thumbnails(candidates),
        }
    }

    /// `Event::ThumbnailsNeeded`'s handling (Stage 11): dispatches
    /// background generation for viewport-visible candidates the view
    /// bubbled up. Gated by `files.toml`'s `thumbnails`/`thumbnail-max-mb`
    /// knobs here — `ui::dirview` has no `Config` after construction (see
    /// `DirectoryView::new`'s own comment on why only a handful of
    /// config-derived fields survive past startup), and `core::thumbs` has
    /// no opinion on a UI-facing size cap of its own (see that module's
    /// doc comment) — so this is the one place both gates actually apply.
    /// Every dispatched location is tracked in `thumb_inflight` until its
    /// `Message::ThumbnailReady` lands, so a second viewport batch
    /// covering the same rows before the first finishes doesn't queue a
    /// duplicate generation for it.
    fn request_thumbnails(
        &mut self,
        candidates: Vec<ui::dirview::ThumbCandidate>,
    ) -> Task<Message> {
        if !self.thumbnails_enabled {
            return Task::none();
        }
        let max_mb = self.thumbnail_max_mb;
        // Two passes, deliberately: every `filter`/`filter_map` closure
        // below only ever reads `self` (`thumb_cache`/`thumb_failed`/
        // `thumb_inflight`/`mime_db`), but the dispatch loop after it needs
        // to *mutate* `thumb_inflight` — one chained iterator expression
        // can't hold both an immutable borrow (for the filters) and a
        // mutable one (for the final step) live at the same time, since
        // every closure in a single chain is constructed before any of
        // them run. Collecting the filtered candidates first ends that
        // immutable borrow before the mutating loop starts.
        let filtered: Vec<(ui::dirview::ThumbCandidate, String)> = candidates
            .into_iter()
            .filter(|candidate| !core::thumbs::exceeds_max_size(candidate.size_bytes, max_mb))
            .filter(|candidate| {
                self.thumb_cache
                    .get_for(&candidate.location, candidate.modified)
                    .is_none()
            })
            .filter(|candidate| {
                self.thumb_failed.get(&candidate.location) != Some(&candidate.modified)
            })
            .filter(|candidate| !self.thumb_inflight.contains(&candidate.location))
            .filter_map(|candidate| {
                let name = candidate.location.path.file_name()?;
                let mimetype = self.mime_db.guess(name, None);
                Some((candidate, mimetype))
            })
            .collect();

        let tasks: Vec<Task<Message>> = filtered
            .into_iter()
            .map(|(candidate, mimetype)| {
                self.thumb_inflight.insert(candidate.location.clone());
                let request = core::thumbs::ThumbRequest {
                    location: candidate.location.clone(),
                    mimetype,
                    modified: candidate.modified,
                };
                let registry = self.thumb_registry.clone();
                let semaphore = self.thumb_semaphore.clone();
                let cache_root = self.thumb_cache_root.clone();
                let location = candidate.location.clone();
                let modified = candidate.modified;
                Task::perform(
                    core::thumbs::thumbnail_for(registry, semaphore, cache_root, request),
                    move |handle| Message::ThumbnailReady(location.clone(), modified, handle),
                )
            })
            .collect();
        Task::batch(tasks)
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
    fn start_delete(
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

    /// `Event::Activated`'s handling: resolve each location's default app
    /// from its name-guessed mimetype and open it — the "files open in
    /// the right app" done criterion. A location with no name, no
    /// resolvable mimetype association, or an association naming an app
    /// this database never found all degrade to a worded `eprintln!`
    /// rather than silently doing nothing or panicking.
    fn open_with_default_app(&self, locations: &[Location]) {
        for location in locations {
            let Some(name) = location.path.file_name() else {
                continue;
            };
            let mimetype = self.mime_db.guess(name, None);
            let Some(entry) = self.apps_db.default_for(&mimetype) else {
                eprintln!("saola-files: no app is associated with {mimetype} for {location}");
                continue;
            };
            let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
            if let Err(err) =
                core::apps::open(entry, std::slice::from_ref(&location.path), &terminal)
            {
                eprintln!("saola-files: could not open {location}: {err}");
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        // Stage 9: while the Trash browser is showing, the hidden
        // background `DirectoryView` must not receive keyboard input at
        // all — `Message::Keyboard` mutating its (invisible) cursor/
        // selection would be merely odd on its own, but reaching Delete/
        // Shift+Delete through it would silently trash/permanently-delete
        // files in whatever directory the user last browsed, with no
        // visual feedback in the Trash view showing it happened. Cutting
        // the subscription off here — rather than trying to filter
        // individual `Action`s downstream — is the same "one surface owns
        // the keyboard at a time" posture `DirectoryView::handle_keyboard`
        // already takes for its own path-editor/rename/menu guards, just
        // one level up.
        let keyboard = if self.trash_active {
            Subscription::none()
        } else {
            iced::keyboard::listen().map(|event| {
                Message::Explorer(ui::explorer::Message::Directory(
                    ui::dirview::Message::Keyboard(event),
                ))
            })
        };

        // Stage 5: the active view's own live-update watch, if its backend
        // has one — `None`/an out-of-range `active` degrades to "no watch
        // subscription" rather than a panic, same posture as `App::view`'s
        // own `views.get(self.active)` guard.
        let watch = self
            .views
            .get(self.active)
            .map(|view| view.subscription().map(directory_message))
            .unwrap_or_else(Subscription::none);

        // Stage 7: the places sidebar's own live udisks feed — one for the
        // app's whole lifetime (`ui::sidebar::Sidebar::subscription`'s doc
        // comment), batched in beside the keyboard/watch streams above.
        let mounts = self
            .sidebar
            .subscription()
            .map(|m| Message::Explorer(ui::explorer::Message::Sidebar(m)));

        // Stage 8: the active op's progress stream, if one is running —
        // identified by `OpRequest` (its manual `Hash`-by-`id`, see that
        // type's doc comment), so iced keeps the same running copy/move
        // alive across re-renders and tears it down the moment `active_op`
        // goes back to `None`.
        let ops = self
            .active_op
            .as_ref()
            .map(|request| ui::dialogs::progress::subscription(request).map(Message::OpEvent))
            .unwrap_or_else(Subscription::none);

        // Stage 10: the undo toast's fade tick, gated to only run while a
        // toast is actually showing — CLAUDE.md's "nothing ticks without a
        // documented exception" (see `ui::dialogs::undo_toast`'s own
        // module doc comment for this exception's reasoning).
        let undo_toast = ui::dialogs::undo_toast::subscription(self.undo_toast.is_some())
            .map(Message::UndoToast);

        Subscription::batch([keyboard, watch, mounts, ops, undo_toast])
    }

    fn theme(&self) -> iced::Theme {
        saola_theme::to_iced_theme(&self.theme)
    }

    fn style(&self, theme: &iced::Theme) -> iced::theme::Style {
        saola_theme::chrome::transparent_clear(theme)
    }

    fn view(&self) -> Element<'_, Message> {
        let t = &self.theme;

        // Stage 9: the Trash browser swaps in for the ordinary
        // sidebar+directory-view composition wholesale, but still shows
        // the same sidebar beside it (`ui::trashview` isn't part of
        // `ui::explorer`'s portal seam — see that module's doc comment —
        // so this is composed directly here rather than by threading a
        // "what's on the right" enum through `ui::explorer::view`).
        let body: Element<'_, Message> = if self.trash_active {
            let sidebar_view: Element<'_, Message> = self
                .sidebar
                .view(t, &core::places::trash_location())
                .map(|m| Message::Explorer(ui::explorer::Message::Sidebar(m)));
            let trash_column: Element<'_, Message> = self.trash_view.view(t).map(Message::Trash);
            // Same region geometry `ui::explorer::view` applies to the
            // ordinary composition (see its comment on `sizes.island_gap`):
            // the sidebar is an inset chrome island either way, so the
            // trash browser must not shift it by a pixel when it swaps in.
            iced::widget::row![sidebar_view, trash_column]
                .spacing(t.sizes.island_gap)
                .padding(t.sizes.island_gap)
                .width(Fill)
                .height(Fill)
                .into()
        } else {
            match self.views.get(self.active) {
                Some(view) => ui::explorer::view(
                    t,
                    &self.sidebar,
                    view,
                    &self.mime_db,
                    &self.thumb_cache,
                    &self.apps_db,
                    !self.clipboard.is_empty(),
                    Message::Explorer,
                ),
                // Degrades to a blank paper surface rather than panicking —
                // `active` should always be in range, but the no-panic
                // rule means "should" isn't good enough.
                None => iced::widget::Space::new().into(),
            }
        };

        // Stage 8: the ops strip, stacked below the explorer body (a
        // persistent bar, not a popover) whenever a copy/move is running.
        // Stage 10: the undo toast takes the exact same footer position
        // whenever *no* op is running but an undo-able one just finished —
        // the two are mutually exclusive (an op finishing is what clears
        // `active_op_progress` and, in the same `handle_op_event` call,
        // sets `undo_toast`), so at most one of these two strips is ever
        // showing at once, never both stacked.
        let with_progress: Element<'_, Message> = match (&self.active_op_progress, &self.undo_toast)
        {
            (Some(progress), _) => iced::widget::column![
                body,
                ui::dialogs::progress::view(t, progress).map(Message::Progress),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            (None, Some(toast)) => iced::widget::column![
                body,
                ui::dialogs::undo_toast::view(t, toast, std::time::Instant::now())
                    .map(Message::UndoToast),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            (None, None) => body,
        };

        // The conflict dialog is a true modal: stacked over everything
        // else with a scrim that swallows clicks (`Message::Noop` — see
        // its doc comment) rather than closing on an outside click, since
        // there is no sane default resolution to fall back to.
        let with_conflict: Element<'_, Message> = match &self.pending_conflict {
            Some(pending) => {
                // Stage 12: the invisible click-swallowing scrim now paints
                // the real §2 modal backdrop (`scrim.modal`,
                // `rgba(12,10,0,0.62)`) instead of nothing — the click-swallow
                // behavior (`Message::Noop`, see its own doc comment) is
                // unchanged, only the paint underneath it.
                let scrim = iced::widget::mouse_area(
                    iced::widget::container(iced::widget::Space::new())
                        .style(saola_theme::style::container::scrim(
                            t,
                            saola_theme::style::container::ScrimKind::Modal,
                        ))
                        .width(Fill)
                        .height(Fill),
                )
                .on_press(Message::Noop);
                let dialog = iced::widget::container(
                    ui::dialogs::conflict::view(t, &pending.conflict, pending.apply_to_all)
                        .map(Message::Conflict),
                )
                .width(Fill)
                .height(Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center);
                iced::widget::stack![with_progress, scrim, dialog].into()
            }
            None => with_progress,
        };

        ui::window::view(t, "Files", with_conflict, Message::Window)
    }
}

/// `ui::dirview::Message -> Message`, as a bare `fn` — used wherever a
/// `Subscription`'s own identity depends on the mapper being a plain
/// function pointer rather than a capturing closure (`ui::dirview::watch`'s
/// module docs explain why `Subscription::run_with`'s builder has the same
/// constraint). `Message::Explorer(explorer::Message::Directory(inner))` is
/// itself already `Fn(dirview::Message) -> Message`-shaped via ordinary
/// tuple-variant construction, but nested two enums deep it isn't
/// expressible as a single path the way `Message::Directory` used to be —
/// this free function is that composition, named once.
fn directory_message(inner: ui::dirview::Message) -> Message {
    Message::Explorer(ui::explorer::Message::Directory(inner))
}

/// Wraps a `Task<dirview::Message>` into `Task<Message>` — the same
/// `directory_message` composition, for the `Task::map` call sites in
/// `App::update`.
fn directory_task(task: Task<ui::dirview::Message>) -> Task<Message> {
    task.map(directory_message)
}

/// `Task<ui::trashview::Message> -> Task<Message>`, the `ui::trashview`
/// counterpart to `directory_task` above.
fn trash_task(task: Task<ui::trashview::Message>) -> Task<Message> {
    task.map(Message::Trash)
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
fn write_foreign_clipboard(op: ops::ClipboardOp, locations: Vec<Location>) -> Task<Message> {
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
    let Some(backend) = modules::resolve(&location.scheme) else {
        return Err((
            location.clone(),
            format!("no backend for scheme \"{}\"", location.scheme),
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
