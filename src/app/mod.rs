//! The application: `App`'s state, its `Message` vocabulary, and the
//! iced entry points (`new`/`update`/`view`/`subscription`/`theme`/
//! `style`) `main.rs` hands to `iced::application`.
//!
//! Binary-private on purpose: this tree is declared from `main.rs`, not
//! `lib.rs`, because `App` wires `integration/` (D-Bus) into the UI,
//! and the layer rule forbids `ui/` from importing `integration/` — so
//! the one place allowed to see every layer lives in the binary, not
//! the library. `mod.rs` keeps the types and the `update` dispatch;
//! each child module carries its own `impl App { … }` block for one
//! feature area, so parallel feature work lands in different files.
//! Child modules can read `App`'s private fields (Rust privacy is
//! per-module-tree: a private item is visible to its own module and
//! every descendant), so only methods called *across* siblings are
//! marked `pub(super)`.

mod connect;
mod dbus;
mod file_ops;
mod navigation;
mod properties;
mod session;
mod sniff;
mod subscription;
mod thumbs;
mod view;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use iced::{Task, keyboard};
use saola_theme::{Surface, Theme};

use core::fs::entry::FileEntry;
use core::fs::{ops, size, undo};
use core::vfs::Location;
use saola_files::keymap::AppAction;
use saola_files::{cli, config, core, integration, ui};
use ui::explorer::{ViewId, ViewIdSource};

use core::remote;

use connect::PendingConnect;
use file_ops::PendingConflict;
use properties::PendingProperties;
use thumbs::{THUMB_LRU_CAPACITY, THUMB_MAX_CONCURRENT};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Window(ui::window::Event),
    Explorer(ui::explorer::Message),
    /// A raw keyboard event from `iced::keyboard::listen`. Belongs to the
    /// app, not to a view: `App::update` hands it to the focused view
    /// (`App::focused`), which resolves it through `keymap.rs`.
    Keyboard(keyboard::Event),
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
    /// The properties dialog's "Close" button, or a click on its own modal
    /// scrim (Stage 13) — unlike the conflict dialog's scrim, this closes
    /// it; see `ui::dialogs::properties::Message::CloseRequested`'s doc
    /// comment for why that's the right default here.
    Properties(ui::dialogs::properties::Message),
    /// An event off the properties dialog's live directory-size count
    /// (`core::fs::size::run`'s stream) — the exact same bounded-bridge/
    /// `AtomicBool`-cancel shape `Message::OpEvent` already bridges for
    /// `core::fs::ops`, a different engine for a different, non-mutating
    /// job (see `core::fs::size`'s module doc comment for why it isn't
    /// just `ops::count_totals` reused).
    SizeEvent(size::SizeEvent),
    /// The connect dialog's own messages (Stage 14) — URI field, saved-
    /// server picks, the host-key/auth prompt buttons and fields.
    Connect(ui::dialogs::connect::Message),
    /// An event off a running connect attempt's stream
    /// (`core::remote::connect`'s `Subscription`, bridged by `ui::dialogs::
    /// connect::subscription`) — the same "translate the raw event into
    /// App/dialog state, the dialog module never sees it directly" split
    /// `Message::OpEvent`/`Message::SizeEvent` already establish.
    ConnectEvent(remote::ConnectEvent),
    /// A decoded D-Bus call (Stage 15) — `org.freedesktop.FileManager1`/
    /// `io.saola.Files1`, bridged from `integration::dbus::subscription`
    /// exactly like every other worker's events above. See `Self::
    /// handle_dbus_event`.
    Dbus(integration::dbus::Event),
    /// `Self::handle_dbus_event`'s `Event::Properties` arm resolved every
    /// requested location's metadata (Stage 15) — the async half of
    /// opening the properties dialog for a D-Bus `ShowItemProperties`
    /// call, mirroring `delete_one`'s per-location `modules::resolve` +
    /// `Backend::metadata` pattern. A location that failed to resolve is
    /// simply missing from this list (see `resolve_dbus_properties`'s own
    /// doc comment) rather than failing the whole call.
    DbusPropertiesResolved(Vec<(Location, FileEntry)>),
    /// Swallows a click on the conflict dialog's modal scrim — the dialog
    /// must be answered via one of its three buttons, never dismissed by
    /// clicking outside it (there is no sane default resolution to assume).
    /// The iced 0.14 gotcha CLAUDE.md documents: a `mouse_area` needs a
    /// real `.on_press` to actually capture the click, the same "no
    /// `.on_press` = doesn't capture its press" behavior a bare `button`
    /// has (`ui::menus`'s popover scrim is the same trick, there closing
    /// the menu instead of doing nothing).
    Noop,
    // --- features (alphabetical) ---
    /// Session-state events (`session.rs`): window resizes.
    Session(session::Message),
    /// A batch of background content-sniff head reads finished
    /// (`app::sniff`).
    SniffReady(Vec<sniff::SniffOutcome>),
}

/// Where a raw key event should go; see [`App::keyboard_target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyboardTarget {
    /// The focused `DirectoryView` (which still guards its own popovers).
    FocusedView,
    /// A modal or the Trash view owns the screen: drop the event.
    Nobody,
}

pub(crate) struct App {
    theme: Theme,
    /// Which ground the window draws on — `files.toml`'s `surface` knob,
    /// resolved to a `saola_theme::Surface` once in `Self::new` and never
    /// read back out of `Config`: the same "config knob becomes fixed
    /// per-surface state" posture `trash_view` and `DirectoryView::new`
    /// already take for their own config-derived fields. Threaded down
    /// through `ui::window`/`ui::explorer` to everything anchored to the
    /// window; the popovers, the undo toast and the four modal dialogs
    /// never see it (their surfaces are pinned by the style guide).
    surface: Surface,
    /// Every open directory view, each paired with its id (the tabs
    /// seam). The UI shows only the focused one for now. Looked up by id
    /// (`ui::explorer::find`/`find_mut`), never by position — see
    /// `ui::explorer`'s module docs, "View ids".
    views: Vec<(ViewId, ui::dirview::DirectoryView)>,
    /// The view the window shows and keyboard input goes to.
    focused: ViewId,
    /// Mints a fresh [`ViewId`] for every view opened; ids are never
    /// reused. Only `Self::new` draws from it so far (the first view's id
    /// is taken before `App` exists), so nothing reads the field yet; the
    /// tabs and split-pane features will, when they open more views.
    #[allow(dead_code)]
    view_ids: ViewIdSource,
    /// The places sidebar (Stage 7) — its own self-contained state
    /// (`core::places::Place`s built once at startup, `core::udisks::
    /// Mount`s updated live), composed beside `views`/`focused` by
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
    /// `focused` (the tabs seam) are untouched either way: switching into
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
    /// The properties dialog currently showing, if any (Stage 13) — mirrors
    /// `pending_conflict`'s split: the pure render-time snapshot
    /// (`ui::dialogs::properties::Properties`) bundled with the App-owned
    /// plumbing (`size_request`'s cancel handle) that module has no
    /// business holding itself.
    pending_properties: Option<PendingProperties>,
    /// Monotonic [`size::SizeRequestId`] allocator — one per `App`, the
    /// same "one shared counter, never per-view" posture as `op_ids`.
    size_ids: size::SizeIdSource,
    /// The connection manager for remote backends (Stage 14) — a shared
    /// cache like `mime_db`/`thumb_cache`, never per-view. Its pool is
    /// published globally at construction (`Self::new`'s
    /// `remote.install_global()` call) so `modules::resolve` can reach it
    /// from deep inside `core::fs::ops`/`ui::dirview` — see `core::
    /// remote`'s own module doc comment for the full reasoning.
    remote: remote::RemoteManager,
    /// Monotonic [`remote::ConnectId`] allocator — one per `App`, the same
    /// posture `op_ids`/`size_ids` already take.
    connect_ids: remote::ConnectIdSource,
    /// The connect dialog currently showing, if any (Stage 14) — mirrors
    /// `pending_properties`'s split: the pure render-time snapshot
    /// (`ui::dialogs::connect::Connect`) bundled with the App-owned
    /// plumbing (the in-flight `ConnectRequest` and the two prompt reply
    /// channels) that module has no business holding itself.
    pending_connect: Option<PendingConnect>,
    /// The parsed CLI invocation this process was launched with (Stage
    /// 15) — kept around (not just consumed at construction, like the
    /// rest of `Config`/`Cli` — see `Self::new`'s comment on `config`)
    /// purely to hand a clone to `integration::dbus::subscription` every
    /// frame as `Subscription::run_with`'s identity key. It never changes
    /// after `Self::new` sets it: if this process turns out to be a
    /// *second* instance, that subscription's worker forwards this exact
    /// value to the already-running primary and exits before `App` ever
    /// gets to do anything else with it.
    activation: cli::Cli,
    // --- features (alphabetical) ---
    /// Remembered window size, last location and per-folder view
    /// settings (`session.rs`).
    session: session::Session,
    /// Files with a content-sniff head read in flight (`app::sniff`), so a
    /// second scroll batch does not queue a duplicate read.
    sniff_inflight: HashSet<Location>,
}

impl App {
    pub(crate) fn new(
        config: config::Config,
        args: cli::Cli,
        state: core::state::Store,
    ) -> (Self, Task<Message>) {
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
        //
        // Cloned *before* the `if let Some(select) = args.select { .. }`
        // below partially moves `args` apart (Stage 15): `Self::
        // activation` needs the whole, untouched `Cli` this process was
        // launched with, for `integration::dbus::subscription` to forward
        // if this turns out to be a second instance.
        let activation = args.clone();
        let mut session = session::Session::new(state, fallback.clone());
        let (mut view, task) = if let Some(select) = args.select {
            ui::dirview::DirectoryView::open_select(select, fallback, &config)
        } else if let Some(target) = args.target {
            ui::dirview::DirectoryView::open_target(target, fallback, &config)
        } else {
            // No path given: reopen where the last session ended.
            let start = match session.last_location() {
                Some(last) => {
                    session.begin_restore(last.clone());
                    last
                }
                None => fallback,
            };
            ui::dirview::DirectoryView::open(start, &config)
        };

        // The one-arm translation of the config vocabulary ("paper"/"ink")
        // into the theme's. `config::WindowSurface` exists precisely so
        // `config.rs` never has to import saola-theme; this is the single
        // place the two vocabularies meet.
        let surface = match config.surface {
            config::WindowSurface::Paper => Surface::Paper,
            config::WindowSurface::Ink => Surface::Ink,
        };

        let mut view_ids = ViewIdSource::default();
        let view_id = view_ids.alloc();
        // Apply this folder's saved view settings to the first view.
        session.view_updated(view_id, &mut view);

        let app = App {
            theme: Theme::saola(),
            surface,
            views: vec![(view_id, view)],
            focused: view_id,
            view_ids,
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
            pending_properties: None,
            size_ids: size::SizeIdSource::default(),
            remote: remote::RemoteManager::new(),
            connect_ids: remote::ConnectIdSource::default(),
            pending_connect: None,
            activation,
            // --- features (alphabetical) ---
            session,
            sniff_inflight: HashSet::new(),
        };
        // Stage 14: publish this `App`'s `RemoteManager` pool for
        // `modules::resolve` to reach — see that function's own doc
        // comment and `core::remote`'s module docs for why this is safe
        // (single-instance app) and why it happens exactly once, here.
        app.remote.install_global();
        (app, directory_task(view_id, task))
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Window(event) => {
                // The header's close button: flush session state first.
                if matches!(event, ui::window::Event::Close) {
                    self.session_flush();
                }
                ui::window::update(event)
            }
            Message::Explorer(ui::explorer::Message::Sidebar(inner)) => {
                match self.sidebar.update(inner) {
                    Some(ui::sidebar::Event::OpenDirectory(location)) => {
                        self.navigate_active(location)
                    }
                    Some(ui::sidebar::Event::ConnectRequested) => {
                        self.open_connect_dialog(None, false)
                    }
                    None => Task::none(),
                }
            }
            Message::Explorer(ui::explorer::Message::Directory(id, inner)) => {
                self.update_view(id, inner)
            }
            Message::Keyboard(event) => match self.keyboard_target() {
                // Checked here as well as in `subscription`: an event
                // already queued when a modal opened must not slip through.
                KeyboardTarget::FocusedView => {
                    self.update_view(self.focused, ui::dirview::Message::Keyboard(event))
                }
                KeyboardTarget::Nobody => Task::none(),
            },
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
                self.handle_thumbnail_ready(location, modified, handle)
            }
            Message::Properties(ui::dialogs::properties::Message::CloseRequested) => {
                self.close_properties();
                Task::none()
            }
            Message::SizeEvent(event) => self.handle_size_event(event),
            Message::Connect(inner) => self.handle_connect_message(inner),
            Message::ConnectEvent(event) => self.handle_connect_event(event),
            Message::Dbus(event) => self.handle_dbus_event(event),
            Message::DbusPropertiesResolved(items) => self.open_properties(items),
            // --- features (alphabetical) ---
            Message::Session(inner) => self.handle_session_message(inner),
            Message::SniffReady(outcomes) => self.handle_sniff_ready(outcomes),
        }
    }

    /// Delivers `message` to the view with id `id` and acts on whatever
    /// `Event` it bubbles. A view that has since closed is simply not
    /// found, and the message is dropped — late async results (a listing,
    /// a rename result) for a closed view are expected, not an error.
    fn update_view(&mut self, id: ViewId, message: ui::dirview::Message) -> Task<Message> {
        let Some(view) = ui::explorer::find_mut(&mut self.views, id) else {
            return Task::none();
        };
        let (task, event) = view.update(message);
        // A view-mode or sort change (or back/forward) is a session event.
        let session_task = self.session_view_updated(id);
        let mut tasks = vec![directory_task(id, task), session_task];
        if let Some(event) = event {
            tasks.push(self.handle_directory_event(id, event));
        }
        Task::batch(tasks)
    }

    /// The focused directory view, if it is still open.
    /// The single routing decision for raw key events: who gets them?
    ///
    /// Every App-level modal must be listed here. A new dialog that is not
    /// added lets keys (Delete, Ctrl+Z, ...) fall through to the directory
    /// behind its scrim. Popovers (overflow menu, Open with) are NOT listed:
    /// they live inside the focused `DirectoryView`, whose `handle_keyboard`
    /// swallows every key but Esc (which closes them) while one is open.
    /// The empty-trash confirm is inline in the Trash view, covered by
    /// `trash_active`. Modal dialogs have no keyboard handling of their own
    /// (Esc does nothing today), so `Nobody` keeps that exactly as is.
    pub(crate) fn keyboard_target(&self) -> KeyboardTarget {
        let modal_open = self.pending_conflict.is_some()
            || self.pending_properties.is_some()
            || self.pending_connect.is_some();
        if self.trash_active || modal_open {
            KeyboardTarget::Nobody
        } else {
            KeyboardTarget::FocusedView
        }
    }

    fn focused_view(&self) -> Option<&ui::dirview::DirectoryView> {
        ui::explorer::find(&self.views, self.focused)
    }

    /// Carries out an app-level keymap action (`keymap::Action::App`),
    /// bubbled up from a view as `dirview::Event::App`.
    ///
    /// `AppAction` has no variants yet, so this `match` has no arms: the
    /// compiler knows no value of the type can exist and accepts the empty
    /// match as complete. Each feature that adds an `AppAction` variant
    /// adds its arm here, under the marker.
    pub(super) fn handle_app_action(&mut self, action: AppAction) -> Task<Message> {
        match action {
            // --- features (alphabetical) ---
        }
    }

    pub(crate) fn theme(&self) -> iced::Theme {
        saola_theme::to_iced_theme(&self.theme)
    }

    pub(crate) fn style(&self, theme: &iced::Theme) -> iced::theme::Style {
        saola_theme::chrome::transparent_clear(theme)
    }
}

/// `(ViewId, dirview::Message) -> Message`: the message for view `id`,
/// nested two enums deep. Takes its two parts as one tuple because that is
/// the item type `Subscription::with` produces — see `App::subscription`,
/// which passes this function itself (not a closure) to
/// `Subscription::map`.
fn directory_message((id, inner): (ViewId, ui::dirview::Message)) -> Message {
    Message::Explorer(ui::explorer::Message::Directory(id, inner))
}

/// Wraps a `Task<dirview::Message>` from view `id` into `Task<Message>`,
/// so its result comes back addressed to that same view. Unlike
/// `Subscription::map`, `Task::map` accepts a capturing closure; `move`
/// copies `id` into it.
fn directory_task(id: ViewId, task: Task<ui::dirview::Message>) -> Task<Message> {
    task.map(move |inner| directory_message((id, inner)))
}

/// `Task<ui::trashview::Message> -> Task<Message>`, the `ui::trashview`
/// counterpart to `directory_task` above.
fn trash_task(task: Task<ui::trashview::Message>) -> Task<Message> {
    task.map(Message::Trash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(
            config::Config::default(),
            cli::Cli::default(),
            core::state::Store::default(),
        )
        .0
    }

    /// Opens the copy-conflict prompt the way a running op does.
    fn open_conflict(app: &mut App) {
        let (reply, _rx) = futures::channel::mpsc::channel(1);
        let loc = Location::local(PathBuf::from("/nonexistent"));
        let _ = app.handle_op_event(ops::OpEvent::Conflict {
            conflict: ops::Conflict {
                source: loc.clone(),
                dest: loc,
            },
            reply,
        });
    }

    #[test]
    fn keys_reach_the_view_when_nothing_is_open() {
        assert_eq!(app().keyboard_target(), KeyboardTarget::FocusedView);
    }

    #[test]
    fn conflict_dialog_takes_keys_away_from_the_view() {
        let mut app = app();
        open_conflict(&mut app);
        assert_eq!(app.keyboard_target(), KeyboardTarget::Nobody);
    }

    #[test]
    fn trash_view_takes_keys_away_from_the_view() {
        let mut app = app();
        app.trash_active = true;
        assert_eq!(app.keyboard_target(), KeyboardTarget::Nobody);
    }

    #[test]
    fn properties_dialog_takes_keys_away_from_the_view() {
        let mut app = app();
        app.pending_properties = Some(PendingProperties {
            properties: ui::dialogs::properties::Properties::new(
                Vec::new(),
                core::vfs::Caps::empty(),
            ),
            size_request: None,
        });
        assert_eq!(app.keyboard_target(), KeyboardTarget::Nobody);
    }

    #[test]
    fn connect_dialog_takes_keys_away_from_the_view() {
        let mut app = app();
        let _ = app.open_connect_dialog(None, false);
        assert_eq!(app.keyboard_target(), KeyboardTarget::Nobody);
    }
}
