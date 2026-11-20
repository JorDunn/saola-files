//! The properties dialog: opening it for a selection (or a D-Bus
//! `ShowItemProperties` call), its live directory-size count, and
//! closing it.

use iced::Task;

use core::fs::entry::FileEntry;
use core::fs::size;
use core::vfs::Location;
use saola_files::{core, modules, ui};

use super::{App, Message};

/// See `App::pending_properties`'s doc comment.
pub(super) struct PendingProperties {
    pub(super) properties: ui::dialogs::properties::Properties,
    /// `None` once the size walk finishes/is cancelled (`Self::
    /// handle_size_event` clears it) — `Some` is also what `App::
    /// subscription` reads to decide whether to keep the size
    /// `Subscription` alive, the same "`Option` doubles as the
    /// subscription gate" shape `active_op`/`active_op_progress` already
    /// take.
    pub(super) size_request: Option<size::SizeRequest>,
}

impl App {
    /// `Event::PropertiesRequested`'s handling (Stage 13): opens the
    /// properties dialog for `items` and kicks off the live size count.
    /// Replaces (rather than stacks behind) any properties dialog already
    /// open — cancelling its size walk first via `Self::close_properties`
    /// — since Alt+Enter/the menu row on a fresh selection obviously means
    /// "show me *this* selection's properties now", not "queue a second
    /// dialog". `caps` is resolved once here (not carried by `Event::
    /// PropertiesRequested` itself) via the same `modules::resolve` every
    /// other backend-capability check in this file already goes through;
    /// every item shares one backend, since a `DirectoryView`'s selection
    /// never spans two locations at once.
    pub(super) fn open_properties(&mut self, items: Vec<(Location, FileEntry)>) -> Task<Message> {
        if items.is_empty() {
            return Task::none();
        }
        self.close_properties();

        let caps = modules::resolve(&items[0].0)
            .map(|backend| backend.caps())
            .unwrap_or_else(core::vfs::Caps::empty);
        let roots: Vec<Location> = items.iter().map(|(location, _)| location.clone()).collect();
        let request = size::SizeRequest::new(self.size_ids.alloc(), roots);

        self.pending_properties = Some(PendingProperties {
            properties: ui::dialogs::properties::Properties::new(items, caps),
            size_request: Some(request),
        });
        Task::none()
    }

    /// Translates one [`size::SizeEvent`] into the open properties
    /// dialog's live size row — mirrors `Self::handle_op_event`'s "replace
    /// the whole readout, never accumulate deltas" posture. A no-op if the
    /// dialog was closed before this landed (the size walk's own task keeps
    /// running until its next cancel-flag check either way — see
    /// `core::fs::size`'s module doc comment).
    pub(super) fn handle_size_event(&mut self, event: size::SizeEvent) -> Task<Message> {
        let Some(pending) = self.pending_properties.as_mut() else {
            return Task::none();
        };
        match event {
            size::SizeEvent::Progress { files, bytes } => {
                pending.properties.size_files = files;
                pending.properties.size_bytes = bytes;
            }
            size::SizeEvent::Finished { files, bytes }
            | size::SizeEvent::Cancelled { files, bytes } => {
                pending.properties.size_files = files;
                pending.properties.size_bytes = bytes;
                pending.properties.size_done = true;
                pending.size_request = None;
            }
        }
        Task::none()
    }

    /// Closes the properties dialog, if one is open — the done criterion's
    /// "cancels on close" half: a still-running size walk's `SizeRequest::
    /// request_cancel` is called before the dialog state is dropped, the
    /// same "cancel, then let the engine notice on its own" posture
    /// `Message::Progress::CancelRequested`'s handling already takes for a
    /// running copy/move.
    pub(super) fn close_properties(&mut self) {
        if let Some(pending) = self.pending_properties.take()
            && let Some(request) = pending.size_request
        {
            request.request_cancel();
        }
    }
}

/// `Self::handle_dbus_event`'s `Event::Properties` async half (Stage 15):
/// resolves every requested location's metadata, mirroring `delete_one`'s
/// per-location `modules::resolve` + `Backend::metadata` pattern. A
/// location with no backend, or whose `metadata` call fails (removed
/// between the D-Bus call and this resolving, permission denied, a
/// dropped remote session), is simply omitted from the result rather than
/// failing every other item in the same call — `Self::open_properties`
/// already treats an empty list as a no-op, so a `ShowItemProperties` call
/// naming only bad paths quietly does nothing instead of erroring.
pub(super) async fn resolve_dbus_properties(
    locations: Vec<Location>,
) -> Vec<(Location, FileEntry)> {
    let mut items = Vec::with_capacity(locations.len());
    for location in locations {
        if let Some(backend) = modules::resolve(&location)
            && let Ok(entry) = backend.metadata(&location).await
        {
            items.push((location, entry));
        }
    }
    items
}
