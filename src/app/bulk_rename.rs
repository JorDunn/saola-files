//! The bulk-rename dialog on `App`: opening it for a selection, running the
//! batch, reporting what went wrong, and pushing the single undo entry.
//!
//! Flow:
//!
//! 1. A view bubbles `Event::BulkRenameRequested`; [`App::open_bulk_rename`]
//!    checks the backend can rename in place (`Caps::RENAME_IN_PLACE`) and
//!    opens the dialog, or words a failure if it can't.
//! 2. The dialog is pure state (`ui::dialogs::bulk_rename`); Apply hands back
//!    the `(old, new)` pairs and [`App::handle_bulk_rename_message`] spawns
//!    [`run`] as a task. One incident is minted here, when the action starts,
//!    so everything that fails in the batch is one notice.
//! 3. [`App::handle_bulk_rename_done`] reports failures, pushes ONE
//!    `UndoEntry::BulkRename` for whatever changed, and refreshes views of
//!    the folder (a backend without `Caps::WATCH` wouldn't tell them).
//!
//! The dialog is a modal: it is listed in `App::keyboard_target`, so keys
//! don't reach the directory behind it.

use std::ffi::OsString;

use iced::Task;

use saola_files::core::failure::{Failure, Incident, Operation};
use saola_files::core::fs::bulk_rename::{self, ApplyReport};
use saola_files::core::fs::undo;
use saola_files::core::vfs::{Caps, Location, VfsError};
use saola_files::keymap::Action;
use saola_files::ui::dialogs::bulk_rename as dialog;
use saola_files::ui::dirview::BulkRenameRequest;
use saola_files::{modules, tr, ui};

use super::{App, Message};

/// The open dialog. A wrapper rather than the bare dialog state so a later
/// field (say, a "running" flag) has somewhere to go without touching `App`.
pub(super) struct PendingBulkRename {
    pub(super) dialog: dialog::BulkRename,
}

impl App {
    /// `Event::BulkRenameRequested`: open the dialog, unless the folder's
    /// backend can't rename in place, in which case say so in words.
    pub(super) fn open_bulk_rename(&mut self, request: BulkRenameRequest) -> Task<Message> {
        let can_rename = modules::resolve(&request.dir)
            .map(|backend| backend.caps().contains(Caps::RENAME_IN_PLACE))
            .unwrap_or(false);
        if !can_rename {
            let name = request.selected.first().cloned().unwrap_or_default();
            let failure = Failure::new(
                name,
                VfsError::Unsupported {
                    operation: tr!("bulk-rename-operation"),
                },
            );
            self.report_new(Operation::Rename, vec![failure]);
            return Task::none();
        }
        let pending = PendingBulkRename {
            dialog: dialog::BulkRename::new(request),
        };
        let focus = focus_task(&pending.dialog);
        self.pending_bulk_rename = Some(pending);
        focus
    }

    /// `Message::BulkRename`: one control changed, or Apply/Cancel.
    pub(super) fn handle_bulk_rename_message(&mut self, message: dialog::Message) -> Task<Message> {
        let Some(pending) = self.pending_bulk_rename.as_mut() else {
            return Task::none();
        };
        let mode_changed = matches!(message, dialog::Message::ModeSelected(_));
        match pending.dialog.update(message) {
            None if mode_changed => focus_task(&pending.dialog),
            None => Task::none(),
            Some(dialog::Outcome::Cancel) => {
                self.pending_bulk_rename = None;
                Task::none()
            }
            Some(dialog::Outcome::Apply(moves)) => {
                let dir = pending.dialog.dir().clone();
                self.pending_bulk_rename = None;
                // One incident for the whole batch, minted as it starts.
                let incident = self.new_incident();
                Task::perform(run(dir.clone(), moves), move |report| {
                    Message::BulkRenameDone(incident, dir.clone(), report)
                })
            }
        }
    }

    /// `Message::BulkRenameDone`: the batch finished (or stopped).
    pub(super) fn handle_bulk_rename_done(
        &mut self,
        incident: Incident,
        dir: Location,
        report: ApplyReport,
    ) -> Task<Message> {
        // What changed, if anything, is ONE undo step. After a clean
        // rollback `moved` is empty, so a failed batch leaves no entry.
        if !report.moved.is_empty() {
            self.push_undo(undo::UndoEntry::BulkRename {
                dir: dir.clone(),
                moves: report.moved.clone(),
            });
        }

        let failures = failures_of(&report);
        if !failures.is_empty() {
            eprintln!(
                "saola-files: bulk rename stopped: {:?}",
                report.failure.as_ref().map(|f| &f.error)
            );
            self.report_failures(incident, Operation::Rename, failures);
        }

        // Re-list every view of this folder: names changed, and a backend
        // without `Caps::WATCH` won't say so itself. (Local watching also
        // reports it; a second listing is harmless.)
        let ids: Vec<_> = self
            .views
            .iter()
            .filter(|(_, view)| *view.location() == dir)
            .map(|(id, _)| *id)
            .collect();
        let refreshes: Vec<Task<Message>> = ids
            .into_iter()
            .map(|id| self.update_view(id, ui::dirview::Message::Action(Action::Refresh)))
            .collect();
        Task::batch(refreshes)
    }

    /// The modal layer for the bulk-rename dialog, stacked over `below`
    /// when it is open. Its scrim swallows clicks (`Message::Noop`) instead
    /// of closing: a half-typed pattern shouldn't vanish on a stray click.
    pub(super) fn with_bulk_rename<'a>(
        &'a self,
        below: iced::Element<'a, Message>,
    ) -> iced::Element<'a, Message> {
        let Some(pending) = &self.pending_bulk_rename else {
            return below;
        };
        let t = &self.theme;
        let scrim = iced::widget::mouse_area(
            iced::widget::container(iced::widget::Space::new())
                .style(saola_theme::style::container::scrim(
                    t,
                    saola_theme::style::container::ScrimKind::Modal,
                ))
                .width(iced::Fill)
                .height(iced::Fill),
        )
        .on_press(Message::Noop);
        let card =
            iced::widget::container(dialog::view(t, &pending.dialog).map(Message::BulkRename))
                .width(iced::Fill)
                .height(iced::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center);
        iced::widget::stack![below, scrim, card].into()
    }
}

/// Focus the dialog's first field for its current mode (none for Case).
fn focus_task(dialog: &dialog::BulkRename) -> Task<Message> {
    match dialog.focus_target() {
        Some(id) => iced::widget::operation::focus(id),
        None => Task::none(),
    }
}

/// The failures to show for a report: the step that stopped the batch, then
/// any rollback step that also failed. Each names the item and the name it
/// was going to get, never a temporary name.
fn failures_of(report: &ApplyReport) -> Vec<Failure> {
    report
        .failure
        .iter()
        .chain(report.stuck.iter())
        .map(|step| Failure::new(step.name.clone(), step.error.clone()).to(step.target.clone()))
        .collect()
}

/// The batch itself, off the UI thread: resolves the backend and hands the
/// moves to `core::fs::bulk_rename::apply`.
async fn run(dir: Location, moves: Vec<(OsString, OsString)>) -> ApplyReport {
    let Some(backend) = modules::resolve(&dir) else {
        let (name, target) = moves.first().cloned().unwrap_or_default();
        return ApplyReport {
            failure: Some(bulk_rename::StepFailure {
                name,
                target,
                error: VfsError::Unavailable {
                    message: tr!("bulk-rename-no-backend", scheme = dir.scheme.as_str()),
                },
            }),
            ..ApplyReport::default()
        };
    };
    bulk_rename::apply(backend.as_ref(), &dir, &moves).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use saola_files::core::fs::bulk_rename::StepFailure;

    #[test]
    fn failures_name_the_item_and_its_target_and_include_rollback_trouble() {
        let denied = |name: &str, target: &str| StepFailure {
            name: OsString::from(name),
            target: OsString::from(target),
            error: VfsError::PermissionDenied {
                location: format!("/x/{name}"),
            },
        };
        let report = ApplyReport {
            moved: vec![(OsString::from("B"), OsString::from("b"))],
            failure: Some(denied("c", "C")),
            stuck: vec![denied("b", "B")],
        };
        let failures = failures_of(&report);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].target, OsString::from("c"));
        assert_eq!(failures[0].destination, Some(OsString::from("C")));
        assert_eq!(failures[1].target, OsString::from("b"));
    }

    #[test]
    fn a_clean_success_reports_nothing() {
        let report = ApplyReport {
            moved: vec![(OsString::from("b"), OsString::from("a"))],
            ..ApplyReport::default()
        };
        assert!(failures_of(&report).is_empty());
    }
}
