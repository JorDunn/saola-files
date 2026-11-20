//! Where the active view goes and what opening something does:
//! `navigate_active` (the one navigation funnel, including the Trash
//! sentinel and the remote auto-connect detour), the directory view's
//! `Event` dispatch, and launching default apps.

use std::path::PathBuf;

use iced::Task;

use core::fs::{ops, undo};
use core::vfs::Location;
use saola_files::{core, ui};

use super::file_ops::write_foreign_clipboard;
use super::{App, Message, directory_task, trash_task};

impl App {
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
    pub(super) fn navigate_active(&mut self, location: Location) -> Task<Message> {
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
    pub(super) fn handle_directory_event(&mut self, event: ui::dirview::Event) -> Task<Message> {
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
            ui::dirview::Event::PropertiesRequested(items) => self.open_properties(items),
        }
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
}
