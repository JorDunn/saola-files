//! Showing failures in the window: `App`'s side of
//! `ui::dialogs::error_notice`.
//!
//! # Reporting a failure (the API other features call)
//!
//! Build one [`Failure`] per thing that failed and pick the
//! [`Operation`] (both in `core::failure`), then either:
//!
//! - from `&mut self` code (an `update` arm, an `Event` handler):
//!   `self.report_failures(Operation::Open, failures);`
//! - from a `Task::perform` completion closure, which has no `self`: return
//!   `failed(Operation::Trash, vec![failure])` as the `Message`.
//!
//! Failures of the same operation that arrive while a notice is showing
//! join it, so reporting one failure per task is fine. The sentence, the
//! grouping and the Details list are all derived; nothing here words
//! anything. stderr logging stays at the call site, next to the code that
//! knows the raw error.
//!
//! # Escape
//!
//! Esc dismisses the notice, but only when the focused view has nothing of
//! its own for Esc to close (menu, rename, path editor): one press closes
//! the thing the person is looking at, the next closes the notice.

use std::ffi::OsString;

use iced::keyboard;
use iced::widget::column;
use iced::{Element, Fill, Task};

use saola_files::core::failure::{Failure, Operation};
use saola_files::core::fs::ops::OpKind;
use saola_files::core::vfs::{Location, VfsError};
use saola_files::ui;
use saola_files::ui::dialogs::error_notice::{self, Notice};

use super::{App, Message};

/// The `Message` that reports `failures` of `operation`. For `Task::perform`
/// completion closures; see the module docs.
pub(super) fn failed(operation: Operation, failures: Vec<Failure>) -> Message {
    Message::ErrorNotice(error_notice::Message::Report(operation, failures))
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
    };
    let failures = errors
        .iter()
        .map(|(location, err)| {
            let failure = Failure::at(location, err.clone());
            if location == dest_dir {
                failure
            } else {
                failure.into_folder(dest_dir)
            }
        })
        .collect();
    (operation, failures)
}

impl App {
    /// Shows `failures` in the window; see the module docs.
    pub(super) fn report_failures(&mut self, operation: Operation, failures: Vec<Failure>) {
        Notice::report(&mut self.error_notice, operation, failures);
    }

    /// Called with a trash-browser message *before* the browser handles it
    /// (so an item index still names the row the person clicked): a failed
    /// restore or empty becomes a notice. The browser's own stderr line
    /// stays.
    pub(super) fn report_trash_failure(&mut self, message: &ui::trashview::Message) {
        use ui::trashview::Message as Trash;
        match message {
            Trash::RestoreResult(index, Err(err)) => {
                let name = self.trash_view.original_name(*index).unwrap_or_default();
                self.report_failures(Operation::Restore, vec![Failure::new(name, err.clone())]);
            }
            Trash::EmptyResult(Err(err)) => {
                self.report_failures(
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

    /// Consumes `event` and returns `true` when it is a bare Escape that
    /// should dismiss the notice (one is showing, and the focused view has
    /// nothing of its own to cancel). Otherwise returns `false` and the key
    /// goes to the view as usual.
    pub(super) fn dismiss_error_notice_on_escape(&mut self, event: &keyboard::Event) -> bool {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return false;
        };
        let is_escape = matches!(key, keyboard::Key::Named(keyboard::key::Named::Escape))
            && modifiers.is_empty();
        if !is_escape || self.error_notice.is_none() {
            return false;
        }
        if self.focused_view().is_some_and(|view| view.owns_escape()) {
            return false;
        }
        self.error_notice = None;
        true
    }

    /// Stacks the notice (when there is one) directly under `body`. The
    /// caller then adds the progress strip / undo toast *below the result*,
    /// which is the whole ordering rule: the notice is a row in the footer
    /// column, never an overlay, so it cannot cover the progress strip.
    pub(super) fn with_error_notice<'a>(
        &'a self,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        match &self.error_notice {
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
    fn copy_and_move_map_to_their_operations() {
        let dest = Location::local("/d");
        assert_eq!(op_failures(OpKind::Copy, &dest, &[]).0, Operation::Copy);
        assert_eq!(op_failures(OpKind::Move, &dest, &[]).0, Operation::Move);
    }
}
