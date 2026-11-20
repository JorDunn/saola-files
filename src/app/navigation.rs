//! Where the active view goes and what opening something does:
//! `navigate_active` (the one navigation funnel, including the Trash
//! sentinel and the remote auto-connect detour), the directory view's
//! `Event` dispatch, and launching default apps.

use std::path::PathBuf;

use iced::Task;

use core::failure::{Cause, Failure, Operation};
use core::fs::{ops, undo};
use core::vfs::{Location, VfsError};
use saola_files::{core, tr, ui};
use ui::explorer::ViewId;

use super::file_ops::write_foreign_clipboard;
use super::remote_open::Launch;
use super::sniff::has_local_path;
use super::{App, Message, directory_task, trash_task};

impl App {
    /// Navigates the focused `DirectoryView` to `location` — what a
    /// places-row click (`ui::sidebar::Event::OpenDirectory`) and a D-Bus
    /// "show this folder" mean. See [`Self::navigate_view`].
    pub(super) fn navigate_active(&mut self, location: Location) -> Task<Message> {
        self.navigate_view(self.focused, location)
    }

    /// Navigates the view with id `id` to `location` — the shared tail end
    /// of both [`Self::navigate_active`] and `ui::dirview::Event::
    /// OpenDirectory` (ascend/breadcrumb/descend, which navigates the view
    /// that asked), since both ultimately mean the same thing: "show this
    /// location in that view."
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
    pub(super) fn navigate_view(&mut self, id: ViewId, location: Location) -> Task<Message> {
        if location == core::places::trash_location() {
            self.trash_active = true;
            return trash_task(self.trash_view.load());
        }
        self.trash_active = false;

        // Stage 14: a remote location this session hasn't connected to
        // yet (a sidebar server row, or a manually-typed `sftp://…`
        // breadcrumb) has no live session for `modules::resolve` to hand
        // back — opening the connect dialog (auto-started, no "enter a
        // URI" step needed since we already have one) is what a plain
        // `view.navigate(location)` would otherwise fail against with a
        // bare "no backend" error. An *already*-pooled remote location
        // (this session already connected to this exact server) skips
        // straight through to the ordinary navigate below, same as local.
        if !location.is_local() && self.remote.pooled(&location).is_none() {
            return self.open_connect_dialog(Some(location.to_string()), true);
        }

        let Some(view) = ui::explorer::find_mut(&mut self.views, id) else {
            return Task::none();
        };
        let task = view.navigate(location);
        let session_task = self.session_view_updated(id);
        Task::batch([directory_task(id, task), session_task])
    }

    /// The owner's response to an `Event` from the view with id `id` — the view only
    /// ever requests a navigation or an open/spawn, never performs one
    /// itself (see `ui::dirview`'s module docs). Every branch here is a
    /// synchronous `Command::spawn` (a detached process launch returns
    /// immediately — there's nothing to `.await`), so none of these need
    /// an `iced::Task`; a spawn failure is worded to stderr rather than
    /// surfaced in the UI (no error-dialog surface exists yet, and a
    /// failed launch is exactly the kind of thing CLAUDE.md's no-panic
    /// posture says should degrade quietly, not take the app down).
    pub(super) fn handle_directory_event(
        &mut self,
        id: ViewId,
        event: ui::dirview::Event,
    ) -> Task<Message> {
        match event {
            ui::dirview::Event::OpenDirectory(location) => self.navigate_view(id, location),
            ui::dirview::Event::Activated(locations) => {
                // Files with no local path (SFTP) are downloaded first
                // (`remote_open`); the rest open in place, as ever.
                let (locations, remote) = self.divert_remote(locations, Launch::Default);
                self.open_with_default_app(id, &locations);
                remote
            }
            ui::dirview::Event::OpenWith(locations, desktop_id) => {
                let Some(entry) = self.apps_db.entry(&desktop_id).cloned() else {
                    eprintln!("saola-files: {desktop_id} is no longer installed");
                    // The app's name is gone with its launcher entry, so the
                    // desktop id stands in for it.
                    let failures = locations
                        .iter()
                        .map(|l| Failure::at(l, Cause::AppUninstalled).to(desktop_id.as_str()))
                        .collect();
                    self.report_new(Operation::OpenWith, failures);
                    return Task::none();
                };
                let (locations, remote) =
                    self.divert_remote(locations, Launch::With(desktop_id.clone()));
                if locations.is_empty() {
                    return remote;
                }
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                if let Err(err) = core::apps::open(&entry, &paths, &terminal) {
                    eprintln!("saola-files: could not open with {}: {err}", entry.name);
                    let failures = locations
                        .iter()
                        .map(|l| Failure::at(l, Cause::launch(&err)).to(entry.name.as_str()))
                        .collect();
                    self.report_new(Operation::OpenWith, failures);
                }
                remote
            }
            ui::dirview::Event::OpenTerminal(location, is_dir) => {
                // A terminal needs a real directory: not available on a
                // backend without a local path.
                if !has_local_path(&location) {
                    let folder = if is_dir {
                        location.clone()
                    } else {
                        location.parent().unwrap_or_else(|| location.clone())
                    };
                    let err = VfsError::Unsupported {
                        operation: tr!("open-terminal-operation"),
                    };
                    self.report_new(Operation::OpenTerminal, vec![Failure::at(&folder, err)]);
                    return Task::none();
                }
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                if let Err(err) = core::apps::open_terminal_here(&terminal, is_dir, &location.path)
                {
                    eprintln!("saola-files: could not open a terminal at {location}: {err}");
                    // The terminal opens *in* a folder: name the folder, not
                    // a file inside it.
                    let folder = if is_dir {
                        location.clone()
                    } else {
                        location.parent().unwrap_or_else(|| location.clone())
                    };
                    let failure = Failure::at(&folder, Cause::launch(&err));
                    self.report_new(Operation::OpenTerminal, vec![failure]);
                }
                Task::none()
            }
            ui::dirview::Event::RunCustomAction(exec, locations) => {
                let (locations, remote) =
                    self.divert_remote(locations, Launch::Custom(exec.clone()));
                if locations.is_empty() {
                    return remote;
                }
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                let mut failures = Vec::new();
                for argv in core::apps::build_argv(&exec, &paths) {
                    let Some((program, args)) = argv.split_first() else {
                        continue;
                    };
                    if let Err(err) = core::apps::spawn_argv(program, args) {
                        eprintln!("saola-files: custom action \"{exec}\" failed: {err}");
                        failures.push(Failure::new(program.clone(), Cause::launch(&err)));
                    }
                }
                self.report_new(Operation::RunAction, failures);
                remote
            }
            ui::dirview::Event::CopyRequested(locations) => {
                self.clipboard.set_copy(locations.clone());
                let incident = self.new_incident();
                write_foreign_clipboard(incident, ops::ClipboardOp::Copy, locations)
            }
            ui::dirview::Event::CutRequested(locations) => {
                self.clipboard.set_cut(locations.clone());
                let incident = self.new_incident();
                write_foreign_clipboard(incident, ops::ClipboardOp::Cut, locations)
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
            ui::dirview::Event::ThumbnailsNeeded(candidates) => {
                // The same visible-range candidates drive content sniffing
                // (`app::sniff`); it only borrows them, so it goes first.
                let sniffs = self.request_sniffs(&candidates);
                Task::batch([sniffs, self.request_thumbnails(candidates)])
            }
            ui::dirview::Event::PropertiesRequested(items) => self.open_properties(items),
            ui::dirview::Event::App(action) => self.handle_app_action(action),
            // --- features (alphabetical) ---
            ui::dirview::Event::BulkRenameRequested(request) => self.open_bulk_rename(request),
            ui::dirview::Event::Failed(incident, operation, failures) => {
                self.report_failures(incident, operation, failures);
                Task::none()
            }
            ui::dirview::Event::FolderPropertiesRequested(location) => {
                // Same async metadata fetch (through the backend) and same
                // dialog path as the D-Bus `ShowItemProperties` call.
                Task::perform(
                    super::properties::resolve_dbus_properties(vec![location]),
                    Message::DbusPropertiesResolved,
                )
            }
        }
    }

    /// `Event::Activated`'s handling: resolve each location's default app
    /// from its name-guessed mimetype and open it — the "files open in
    /// the right app" done criterion. A location with no name, no
    /// resolvable mimetype association, or an association naming an app
    /// this database never found all degrade to a worded `eprintln!`
    /// rather than silently doing nothing or panicking.
    fn open_with_default_app(&mut self, id: ViewId, locations: &[Location]) {
        let mut failures = Vec::new();
        for location in locations {
            let Some(name) = location.path.file_name() else {
                continue;
            };
            // Uses the content-sniff verdict when the background sniff
            // (`app::sniff`) has already produced one for this mtime.
            let mimetype = match location.parent() {
                Some(dir) => {
                    let modified = self.entry_mtime(id, location);
                    self.mime_db.guess_entry(&dir, name, modified)
                }
                None => self.mime_db.guess(name, None),
            };
            let Some(entry) = self.apps_db.default_for(&mimetype) else {
                eprintln!("saola-files: no app is associated with {mimetype} for {location}");
                failures.push(Failure::at(location, Cause::NoDefaultApp));
                continue;
            };
            let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
            if let Err(err) =
                core::apps::open(entry, std::slice::from_ref(&location.path), &terminal)
            {
                eprintln!("saola-files: could not open {location}: {err}");
                failures.push(Failure::at(location, Cause::launch(&err)));
            }
        }
        self.report_new(Operation::Open, failures);
    }
}
