//! Showing failures in the window: `App`'s side of
//! `ui::dialogs::error_notice`.
//!
//! # Reporting a failure (the API other features call)
//!
//! Build one [`Failure`] per thing that failed, pick the [`Operation`]
//! (both in `core::failure`), and attach the **incident**: one user action,
//! or one engine operation, with everything that failed inside it.
//!
//! - Mint the incident when the action *starts*: `self.new_incident()`
//!   (needs `&mut self`), and carry the `Incident` (it is `Copy`) into the
//!   tasks the action spawns.
//! - From a `Task::perform` completion closure, which has no `self`: return
//!   `failed(incident, operation, failures)` as the `Message`.
//! - From `&mut self` code that is its own whole action (an `Event`
//!   handler that fails synchronously): `self.report_new(operation,
//!   failures)` mints and reports in one step.
//! - From `&mut self` code that already holds an incident:
//!   `self.report_failures(incident, operation, failures)`.
//!
//! Failures sharing an incident aggregate into one notice; a newer incident
//! replaces it; an older one is dropped (see `ui::dialogs::error_notice`).
//! The sentence, the grouping and the Details list are all derived; nothing
//! here words anything. stderr logging stays at the call site, next to the
//! code that knows the raw error.
//!
//! # How each path identifies its incident
//!
//! - copy/move/trash/delete: one per engine op, minted in
//!   `submit_paste_op` / `start_delete` and kept per op id (`Incidents`);
//! - undo, clipboard write, open, open with, open terminal, custom action:
//!   one per user action, minted where the action is handled;
//! - trash-browser restore/empty: minted by the browser when the request is
//!   made and carried in its result message;
//! - New Folder/File: minted by the view when the create starts and carried
//!   in `CreateResult` and `Event::Failed`.
//!
//! # Escape
//!
//! The routing lives in `App::key_route` (`mod.rs`), the one place that
//! decides where a key goes; this file only supplies [`is_bare_escape`].

use std::collections::HashMap;
use std::ffi::OsString;

use iced::keyboard;
use iced::widget::column;
use iced::{Element, Fill, Task};

use saola_files::core::failure::{Failure, Incident, Operation};
use saola_files::core::fs::ops::{self, OpKind};
use saola_files::core::vfs::{Location, VfsError};
use saola_files::ui;
use saola_files::ui::dialogs::error_notice;

use super::{App, Message};

/// The `Message` that reports `failures` of `operation`. For `Task::perform`
/// completion closures; see the module docs.
pub(super) fn failed(incident: Incident, operation: Operation, failures: Vec<Failure>) -> Message {
    Message::ErrorNotice(error_notice::Message::Report(incident, operation, failures))
}

/// Incidents of the copy/move ops in flight, keyed by [`ops::OpId`].
///
/// Today `submit_paste_op` refuses a second op while one runs, so at most
/// one entry exists; it is a map keyed by op id anyway, so the incident
/// travels with *its* op and a future queue of ops cannot make one op's
/// failures merge into another's notice. (Every other path carries its
/// incident in the task or message itself: no shared slot.)
#[derive(Debug, Default)]
pub(super) struct Incidents {
    ops: HashMap<ops::OpId, Incident>,
}

impl Incidents {
    /// Starts the incident for op `id`.
    pub(super) fn begin_op(&mut self, id: ops::OpId) {
        self.ops.insert(id, Incident::mint());
    }

    /// Op `id` is over: hands back its incident (minting one if somehow
    /// absent) and forgets it.
    pub(super) fn end_op(&mut self, id: Option<ops::OpId>) -> Incident {
        id.and_then(|id| self.ops.remove(&id))
            .unwrap_or_else(Incident::mint)
    }
}

/// True for an unmodified Escape key press: the only key that does anything
/// while only the notice can take input.
pub(super) fn is_bare_escape(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modifiers,
            ..
        } if modifiers.is_empty()
    )
}

/// The failures of a copy/move op, as the notice wants them: each error
/// names the item it hit, going to `dest_dir` (omitted when the error is
/// about the destination folder itself, where "move X to X" would read as
/// nonsense).
pub(super) fn op_failures(
    kind: OpKind,
    dest_dir: &Location,
    errors: &[(Location, VfsError)],
) -> (Operation, Vec<Failure>) {
    let operation = match kind {
        OpKind::Copy => Operation::Copy,
        OpKind::Move => Operation::Move,
        OpKind::Trash => Operation::Trash,
        OpKind::Delete => Operation::Delete,
    };
    // Trash and delete have no destination: "to <folder>" would be wrong.
    let has_destination = matches!(kind, OpKind::Copy | OpKind::Move);
    let failures = errors
        .iter()
        .map(|(location, err)| {
            let failure = Failure::at(location, err.clone());
            if !has_destination || location == dest_dir {
                failure
            } else {
                failure.into_folder(dest_dir)
            }
        })
        .collect();
    (operation, failures)
}

impl App {
    /// A fresh incident for an action that is starting.
    pub(super) fn new_incident(&mut self) -> Incident {
        Incident::mint()
    }

    /// Shows `failures` of `incident` in the window; see the module docs.
    pub(super) fn report_failures(
        &mut self,
        incident: Incident,
        operation: Operation,
        failures: Vec<Failure>,
    ) {
        self.error_notice.report(incident, operation, failures);
    }

    /// [`Self::report_failures`] for an action that is complete in itself:
    /// mints its incident first.
    pub(super) fn report_new(&mut self, operation: Operation, failures: Vec<Failure>) {
        self.report_failures(Incident::mint(), operation, failures);
    }

    /// Called with a trash-browser message *before* the browser handles it:
    /// a failed restore or empty becomes a notice under the incident the
    /// request carried (minted when it was made, so a late failure from an
    /// older request is dropped when a newer notice is showing). The
    /// browser's own stderr line stays.
    pub(super) fn report_trash_failure(&mut self, message: &ui::trashview::Message) {
        use ui::trashview::Message as Trash;
        match message {
            Trash::RestoreResult(incident, name, Err(err)) => {
                self.report_failures(
                    *incident,
                    Operation::Restore,
                    vec![Failure::new(name.clone(), err.clone())],
                );
            }
            Trash::EmptyResult(incident, Err(err)) => {
                self.report_failures(
                    *incident,
                    Operation::EmptyTrash,
                    vec![Failure::new(OsString::new(), err.clone())],
                );
            }
            _ => {}
        }
    }

    /// `Message::ErrorNotice`'s handling.
    pub(super) fn handle_error_notice(&mut self, message: error_notice::Message) -> Task<Message> {
        error_notice::update(&mut self.error_notice, message);
        Task::none()
    }

    /// Stacks the notice (when there is one) directly under `body`. The
    /// caller then adds the progress strip / undo toast *below the result*,
    /// which is the whole ordering rule: the notice is a row in the footer
    /// column, never an overlay, so it cannot cover the progress strip.
    pub(super) fn with_error_notice<'a>(
        &'a self,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        match self.error_notice.notice() {
            Some(notice) => column![
                body,
                error_notice::view(&self.theme, notice).map(Message::ErrorNotice),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            None => body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(path: &str) -> (Location, VfsError) {
        (
            Location::local(path),
            VfsError::PermissionDenied {
                location: path.to_owned(),
            },
        )
    }

    #[test]
    fn op_failures_name_the_item_and_the_destination_folder() {
        let dest = Location::local("/home/me/Docs");
        let (op, failures) = op_failures(OpKind::Move, &dest, &[denied("/home/me/a.txt")]);
        assert_eq!(op, Operation::Move);
        assert_eq!(failures.len(), 1);
        let sentence = saola_files::core::failure::describe(op, &failures[0]);
        assert_eq!(
            sentence,
            "Couldn't move “a.txt” to “Docs”: permission denied."
        );
    }

    #[test]
    fn an_error_about_the_destination_itself_has_no_to_clause() {
        let dest = Location::local("/home/me/Docs");
        let (op, failures) = op_failures(OpKind::Copy, &dest, &[denied("/home/me/Docs")]);
        assert_eq!(op, Operation::Copy);
        assert_eq!(failures[0].destination, None);
    }

    #[test]
    fn trash_and_delete_failures_have_no_destination() {
        let folder = Location::local("/home/me");
        let (op, failures) = op_failures(OpKind::Delete, &folder, &[denied("/home/me/a.txt")]);
        assert_eq!(op, Operation::Delete);
        assert_eq!(failures[0].destination, None);
        assert_eq!(op_failures(OpKind::Trash, &folder, &[]).0, Operation::Trash);
    }

    #[test]
    fn copy_and_move_map_to_their_operations() {
        let dest = Location::local("/d");
        assert_eq!(op_failures(OpKind::Copy, &dest, &[]).0, Operation::Copy);
        assert_eq!(op_failures(OpKind::Move, &dest, &[]).0, Operation::Move);
    }
}
