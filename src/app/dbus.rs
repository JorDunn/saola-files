//! D-Bus activation (Stage 15): handling decoded
//! `org.freedesktop.FileManager1`/`io.saola.Files1` calls — raise,
//! browse, reveal-and-select, and properties.

use std::ffi::OsString;

use iced::{Task, window};

use core::fs::entry::FileEntry;
use core::vfs::Location;
use saola_files::{core, integration, modules, tr, ui};

use super::properties::resolve_dbus_properties;
use super::{App, Message, directory_task};

impl App {
    /// `Message::Dbus`'s handling (Stage 15): every `integration::dbus::
    /// Event` implies "bring the window to front" (see that module's own
    /// doc comment), so every arm batches `raise_window_task` alongside
    /// whatever else it does.
    pub(super) fn handle_dbus_event(&mut self, event: integration::dbus::Event) -> Task<Message> {
        use integration::dbus::Event;
        let raise = raise_window_task();
        match event {
            Event::Raise => raise,
            Event::Browse(location) => Task::batch([raise, self.navigate_active(location)]),
            Event::Reveal(location) => Task::batch([raise, self.reveal_active(location)]),
            Event::Properties(locations) => Task::batch([
                raise,
                Task::perform(
                    resolve_dbus_properties(locations),
                    Message::DbusPropertiesResolved,
                ),
            ]),
        }
    }

    /// `Event::Reveal`'s handling: lists `location`'s parent and selects
    /// `location` itself once the listing lands, replaying the exact
    /// `dirview::Message::TargetResolved` pathway `DirectoryView::
    /// open_select` already sends through at startup (see that
    /// constructor's own doc comment) — just built from an *already-
    /// running* view's location instead of a fresh one, so this reuses
    /// `DirectoryView::update`'s existing handling rather than adding a
    /// second way to apply a pending selection.
    ///
    /// Degrades to an ordinary browse (`Self::navigate_active`) — no
    /// selection highlight, just landing in the right place — when there's
    /// nothing to select (`location` names a bare root, with no filename
    /// component) or when `location`'s parent is a remote scheme this
    /// session hasn't connected to yet (routes into the same auto-connect
    /// dialog `navigate_active` already opens for that case; there's no
    /// listing to select against until a connection exists).
    fn reveal_active(&mut self, location: Location) -> Task<Message> {
        let (Some(parent), Some(name)) = (
            location.parent(),
            location.path.file_name().map(|n| n.to_os_string()),
        ) else {
            return self.navigate_active(location);
        };
        if !parent.is_local() && self.remote.pooled(&parent).is_none() {
            return self.navigate_active(parent);
        }
        directory_task(
            self.focused,
            Task::perform(
                list_for_reveal(parent, name),
                |(location, select, result)| {
                    ui::dirview::Message::TargetResolved(location, select, result)
                },
            ),
        )
    }
}

/// Brings the app's one window to front — every `integration::dbus::
/// Event` implies "look at me" (Stage 15: D-Bus activation). `window::
/// latest()` is the iced 0.14 idiom for "the window this single-window
/// app happens to have" (CLAUDE.md's "Single-window Tasks" gotcha);
/// `and_then` only chains into `window::gain_focus` if a window actually
/// exists, degrading to doing nothing rather than assuming one always
/// does.
fn raise_window_task() -> Task<Message> {
    window::latest().and_then(window::gain_focus)
}

/// `Self::reveal_active`'s async half (Stage 15): lists `parent` and
/// pairs the result with `name` in exactly the `(Location, Option<
/// OsString>, Result<Vec<FileEntry>, VfsError>)` shape `dirview::Message::
/// TargetResolved` already expects — see that variant's own doc comment.
/// A `parent` with no backend (an unresolvable scheme, or a remote
/// authority that raced ahead and dropped its pooled connection between
/// `reveal_active`'s own check and this call actually running) surfaces as
/// an ordinary worded `VfsError` the directory view already knows how to
/// render as its empty state, not a panic or a silently dropped call.
async fn list_for_reveal(
    parent: Location,
    name: OsString,
) -> (
    Location,
    Option<OsString>,
    Result<Vec<FileEntry>, core::vfs::VfsError>,
) {
    let listing = match modules::resolve(&parent) {
        Some(backend) => backend.list(&parent).await,
        None => Err(core::vfs::VfsError::Unavailable {
            message: tr!("error-no-backend", scheme = parent.scheme.as_str()),
        }),
    };
    (parent, Some(name), listing)
}
