//! Inline rename (F2 / the context menu's "Rename…") and the name-
//! uniquification helper New Folder/New File both use.
//!
//! [`RenameState`] lives on `DirectoryView` (`mod.rs`'s private `rename`
//! field) rather than getting its own file-scoped state struct with a
//! `Message`/`update` of its own — there is exactly one row being renamed
//! at a time, per view, the same "one thing, not a collection" shape
//! `path_edit`/`type_ahead` already have, and `mod.rs`'s `Message::Rename*`
//! variants are few enough not to earn a submodule `Message` the way
//! `typeahead`'s own internal state machine did.
//!
//! Renaming itself is deliberately **not** routed through
//! `core::fs::ops`'s engine: it's one `Backend::rename` call, already
//! `async` and already cheap (same-directory, no streaming, no conflict
//! prompt — a name collision just surfaces as an inline error and leaves
//! the field open to retry, see `mod.rs`'s `Message::RenameResult`). Wiring
//! it through the ops engine's progress/conflict machinery for a single
//! metadata-only call would be ceremony with no payoff.
//!
//! The `impl DirectoryView` block below holds the rename/New Folder/New
//! File methods and their `Message::Rename*`/`CreateResult` arms.

use std::ffi::{OsStr, OsString};

use futures::SinkExt;
use iced::Task;

use crate::core::fs::entry::FileEntry;
use crate::core::vfs::{Location, VfsError};
use crate::tr;

// A module importing itself by name: the methods below moved here from
// `mod.rs`, where they spelled these items `rename::RenameState` etc.;
// this keeps those paths resolving unchanged.
use super::rename;
use super::{DirectoryView, Event, Message};

/// Widget id for the inline rename `text_input`. Shared between "who
/// builds the widget" (`list.rs`/`grid.rs`) and "who issues the focus/
/// select_all operations" (`DirectoryView::start_rename_named`) — the same
/// split `breadcrumbs::PATH_INPUT_ID` already has, and for the same reason.
pub const RENAME_INPUT_ID: &str = "saola-files-rename-input";

/// Inline-rename UI state: `Some` on `DirectoryView` while a row is
/// mid-edit.
#[derive(Debug, Clone)]
pub struct RenameState {
    /// The entry's name *before* editing — what `Message::RenameSubmitted`
    /// compares the typed text against (typing the same name back is a
    /// no-op, not a rename) and what the eventual `Backend::rename` call's
    /// `from` half is built from. Never touched again after
    /// `RenameState::new` — renaming what a stale index would have pointed
    /// at is exactly what this avoids: the target is fixed by name at the
    /// moment F2/"Rename…" is pressed, not re-resolved from a row position
    /// that could shift under a concurrent watch event.
    pub original: OsString,
    /// The live `text_input` buffer.
    pub buffer: String,
    /// Set after a failed `Backend::rename` (a name collision, permission
    /// denial, …) — the row stays in edit mode so the typed name isn't
    /// lost, and the message is worded inline rather than in a modal
    /// (CLAUDE.md's capability-honest posture: EACCES is a normal
    /// Tuesday). Cleared the moment the buffer changes again.
    pub error: Option<String>,
}

impl RenameState {
    pub fn new(original: OsString) -> Self {
        let buffer = original.to_string_lossy().into_owned();
        RenameState {
            original,
            buffer,
            error: None,
        }
    }
}

/// A destination name known not to collide with anything in `existing` —
/// `base`, then `base (2)`, `base (3)`, … the same "keep both" numbering
/// `core::fs::ops::unique_rename_dest` uses for copy conflicts, just
/// without a "(copy)" segment (there is nothing here to be a copy *of*: a
/// fresh "New Folder" and a second fresh "New Folder" are two independent
/// new things, not one file colliding with another).
///
/// Checked against `existing` (the view's already-loaded `entries`) rather
/// than a fresh `Backend::list`/`metadata` round trip — good enough for
/// picking an unclaimed starting name; a genuine race (something else
/// creates the exact same name between this check and the real
/// `Backend::mkdir`/`write` call) still surfaces as an ordinary
/// `VfsError::AlreadyExists` the caller words normally, same as any other
/// backend failure.
pub fn unique_name(existing: &[FileEntry], base: &str) -> OsString {
    if !name_taken(existing, base) {
        return OsString::from(base);
    }
    for n in 2..1000u32 {
        let candidate = tr!("dirview-name-with-counter", base = base, n = n);
        if !name_taken(existing, &candidate) {
            return OsString::from(candidate);
        }
    }
    // Every one of 999 candidates is taken — vanishingly unlikely, but the
    // no-panic rule means this still has to return *something* rather than
    // unwrap a `None`. The caller's `Backend::mkdir`/`write` call will
    // simply surface `AlreadyExists`, which is an honest outcome for a
    // directory that genuinely has a thousand same-prefixed entries.
    OsString::from(base)
}

fn name_taken(existing: &[FileEntry], candidate: &str) -> bool {
    existing
        .iter()
        .any(|entry| entry.name == OsStr::new(candidate))
}

impl DirectoryView {
    /// `update`'s `Message::RenameChanged` arm, moved here verbatim.
    pub(super) fn on_rename_changed(&mut self, value: String) -> (Task<Message>, Option<Event>) {
        if let Some(state) = self.rename.as_mut() {
            state.buffer = value;
            state.error = None;
        }
        (Task::none(), None)
    }

    /// `update`'s `Message::RenameCancelled` arm, moved here verbatim.
    pub(super) fn on_rename_cancelled(&mut self) -> (Task<Message>, Option<Event>) {
        self.rename = None;
        (Task::none(), None)
    }

    /// `update`'s `Message::RenameResult` arm, moved here verbatim.
    pub(super) fn on_rename_result(
        &mut self,
        dir_location: Location,
        old_name: OsString,
        result: Result<(), VfsError>,
    ) -> (Task<Message>, Option<Event>) {
        if dir_location != self.location {
            // A later navigation raced ahead of this response.
            return (Task::none(), None);
        }
        match result {
            Ok(()) => {
                if let Some(state) = self.rename.take() {
                    let new_name = OsString::from(state.buffer.trim());
                    let from = dir_location.join(&old_name);
                    let to = dir_location.join(&new_name);
                    self.rename_entry_by_name(&old_name, new_name.clone());
                    self.selection.rename(&old_name, new_name);
                    self.recompute_visible();
                    (Task::none(), Some(Event::Renamed(from, to)))
                } else {
                    (Task::none(), None)
                }
            }
            Err(err) => {
                if let Some(state) = self.rename.as_mut() {
                    state.error = Some(err.to_string());
                }
                (Task::none(), None)
            }
        }
    }

    /// `update`'s `Message::CreateResult` arm, moved here verbatim.
    pub(super) fn on_create_result(
        &mut self,
        dir_location: Location,
        name: OsString,
        result: Result<(), VfsError>,
    ) -> (Task<Message>, Option<Event>) {
        if dir_location != self.location {
            return (Task::none(), None);
        }
        match result {
            Ok(()) => {
                let created = dir_location.join(&name);
                self.pending_select = Some(name);
                self.pending_rename = true;
                (self.load(), Some(Event::Created(created)))
            }
            Err(err) => {
                eprintln!("saola-files: could not create {name:?} in {dir_location}: {err}");
                (Task::none(), None)
            }
        }
    }

    // ── Stage 8: rename / new folder / new file helpers ─────────────────

    /// Starts inline-renaming `name`, if nothing is already being renamed
    /// (defensive — no known call site can currently trigger this while
    /// `self.rename` is already `Some`, but "start a second rename mid-edit"
    /// has no sane meaning, so this degrades to a no-op rather than
    /// clobbering the in-flight one). Returns the focus/select-all task the
    /// same way `Action::EditPath` does for the breadcrumb editor.
    pub(super) fn start_rename_named(&mut self, name: OsString) -> Task<Message> {
        if self.rename.is_some() {
            return Task::none();
        }
        self.rename = Some(rename::RenameState::new(name));
        self.menu_open = false;
        self.open_with_open = false;
        Task::batch([
            iced::widget::operation::focus(rename::RENAME_INPUT_ID),
            iced::widget::operation::select_all(rename::RENAME_INPUT_ID),
        ])
    }

    /// `Message::RenameSubmitted`'s handling: validates the typed name,
    /// and — if it's both non-empty and actually different from the
    /// original — kicks off the `Backend::rename` call. An empty/`.`/`..`/
    /// path-separator-containing name re-opens the field with an inline
    /// error instead of ever reaching the backend (those would either be
    /// silently meaningless or, for `/`, attempt to rename into a
    /// different directory entirely, which "Rename" must never do —
    /// that's what cut+paste is for). Typing the same name back commits to
    /// a silent no-op, matching every mainstream file manager.
    pub(super) fn submit_rename(&mut self) -> Task<Message> {
        let Some(mut state) = self.rename.take() else {
            return Task::none();
        };
        let trimmed = state.buffer.trim().to_owned();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.contains('/') {
            state.error = Some(tr!("dirview-rename-invalid"));
            self.rename = Some(state);
            return Task::none();
        }
        if trimmed == state.original.to_string_lossy() {
            return Task::none();
        }

        let from = self.location.join(&state.original);
        let to = self.location.join(&trimmed);
        let dir_location = self.location.clone();
        let old_name = state.original.clone();
        // Kept `Some` (not cleared) while the async call is in flight, so
        // the row stays in edit mode showing what was just submitted —
        // `Message::RenameResult` is what finally clears it, on success.
        self.rename = Some(state);

        Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&from) else {
                    return Err(VfsError::Other {
                        message: tr!("error-no-backend", scheme = from.scheme.as_str()),
                    });
                };
                backend.rename(&from, &to).await
            },
            move |result| Message::RenameResult(dir_location.clone(), old_name.clone(), result),
        )
    }

    /// Renames `old_name` to `new_name` in `entries` in place, if present
    /// — the optimistic local half of a successful `Message::RenameResult`
    /// (no re-list round trip needed; a later watch event reconciling the
    /// same change, if the backend emits one, is a harmless no-op layered
    /// on top).
    fn rename_entry_by_name(&mut self, old_name: &OsStr, new_name: OsString) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.name.as_os_str() == old_name)
        {
            entry.name = new_name;
        }
    }

    /// `Action::NewFolder`/the context menu's New Folder/New File: picks an
    /// unclaimed name (`rename::unique_name`) and kicks off the
    /// `Backend::mkdir`/`write` call. `Message::CreateResult` is what
    /// reloads the listing and starts renaming the new entry on success.
    pub(super) fn create_new(&mut self, kind: NewKind) -> Task<Message> {
        let base = match kind {
            NewKind::Folder => tr!("dirview-new-folder-name"),
            NewKind::File => tr!("dirview-new-file-name"),
        };
        let name = rename::unique_name(&self.entries, &base);
        let location = self.location.join(&name);
        let dir_location = self.location.clone();
        let name_for_message = name.clone();

        Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&location) else {
                    return Err(VfsError::Other {
                        message: tr!("error-no-backend", scheme = location.scheme.as_str()),
                    });
                };
                match kind {
                    NewKind::Folder => backend.mkdir(&location).await,
                    NewKind::File => {
                        // An empty file: open the write sink and close it
                        // immediately without ever sending a chunk —
                        // `Backend::write` already creates/truncates on
                        // open (see `modules::local::write`'s
                        // `fs::File::create`), so a bare open-then-close is
                        // a genuine empty-file creation, not a partial one.
                        let mut sink = backend.write(&location).await?;
                        sink.close().await.map_err(|_| VfsError::Other {
                            message: tr!("error-create-failed", location = location.to_string()),
                        })
                    }
                }
            },
            move |result| {
                Message::CreateResult(dir_location.clone(), name_for_message.clone(), result)
            },
        )
    }
}

/// What `DirectoryView::create_new` is creating — kept as a tiny local enum
/// rather than a `bool` so the call sites (`Action::NewFolder`,
/// `menu::Message::NewFolderRequested`, `menu::Message::NewFileRequested`) read
/// as what they mean, not "true or false".
#[derive(Debug, Clone, Copy)]
pub(super) enum NewKind {
    Folder,
    File,
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::fs::entry::EntryKind;
    use crate::keymap::Action;
    use iced::keyboard;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size: 0,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    #[test]
    fn rename_state_seeds_the_buffer_from_the_original_name() {
        let state = RenameState::new(OsString::from("report.txt"));
        assert_eq!(state.buffer, "report.txt");
        assert_eq!(state.original, OsString::from("report.txt"));
        assert!(state.error.is_none());
    }

    #[test]
    fn unique_name_returns_the_base_when_nothing_collides() {
        let existing = vec![entry("readme.txt")];
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder");
    }

    #[test]
    fn unique_name_numbers_past_a_collision() {
        let existing = vec![entry("New Folder"), entry("New Folder (2)")];
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder (3)");
    }

    #[test]
    fn unique_name_fills_a_gap_left_by_a_deleted_middle_candidate() {
        let existing = vec![entry("New Folder"), entry("New Folder (3)")];
        // "New Folder (2)" is free even though "(3)" is taken — the
        // numbering always starts back at 2, it doesn't remember history.
        assert_eq!(unique_name(&existing, "New Folder"), "New Folder (2)");
    }

    #[test]
    fn rename_starts_inline_editing_when_exactly_one_entry_is_selected() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        assert!(view.rename_state().is_none());
        let (_, event) = view.apply_action_for_test(Action::Rename);
        assert!(event.is_none());
        let state = view.rename_state().expect("rename should have started");
        assert_eq!(state.original, OsString::from("a"));
        assert_eq!(state.buffer, "a");
    }

    #[test]
    fn rename_with_multiple_selected_is_a_no_op() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        let _ = view.apply_action_for_test(Action::SelectAll);
        let _ = view.apply_action_for_test(Action::Rename);
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_changed_updates_the_buffer_and_clears_a_previous_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        view.rename.as_mut().unwrap().error = Some("boom".to_owned());

        let _ = view.update(Message::RenameChanged("a-new".to_owned()));
        let state = view.rename_state().unwrap();
        assert_eq!(state.buffer, "a-new");
        assert!(state.error.is_none());
    }

    #[test]
    fn rename_cancelled_clears_the_edit_state() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        assert!(view.rename_state().is_some());
        let _ = view.update(Message::RenameCancelled);
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn escape_while_renaming_cancels_via_the_keyboard_path() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);

        let escape = keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let _ = view.update(Message::Keyboard(escape));
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn submitting_an_empty_or_slash_containing_name_re_opens_with_an_inline_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);

        let _ = view.update(Message::RenameChanged("   ".to_owned()));
        let _ = view.update(Message::RenameSubmitted);
        assert!(view.rename_state().unwrap().error.is_some());

        let _ = view.update(Message::RenameChanged("a/b".to_owned()));
        let _ = view.update(Message::RenameSubmitted);
        assert!(view.rename_state().unwrap().error.is_some());
    }

    #[test]
    fn submitting_the_same_name_back_is_a_silent_no_op() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameSubmitted);
        // No backend call was needed — the field just closes.
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_result_for_a_stale_directory_is_dropped() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.navigate(Location::local("/elsewhere"));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Ok(()),
        ));
        assert!(event.is_none());
        // `navigate` already cleared `rename`; the stale result must not
        // resurrect or otherwise touch it.
        assert!(view.rename_state().is_none());
    }

    #[test]
    fn rename_result_success_renames_the_entry_and_selection_in_place() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameChanged("a-renamed".to_owned()));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Ok(()),
        ));
        // Stage 10: a successful rename bubbles `Event::Renamed` so `App`
        // can push it onto the undo stack.
        match event {
            Some(Event::Renamed(from, to)) => {
                assert_eq!(from, Location::local("/home/a"));
                assert_eq!(to, Location::local("/home/a-renamed"));
            }
            other => panic!("expected Renamed, got {other:?}"),
        }
        assert!(view.rename_state().is_none());
        assert!(view.entries.iter().any(|e| e.name == "a-renamed"));
        assert!(!view.entries.iter().any(|e| e.name == "a"));
        assert!(view.selection.is_selected(OsStr::new("a-renamed")));
    }

    #[test]
    fn rename_result_failure_keeps_editing_with_an_inline_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.apply_action_for_test(Action::MoveCursorDown);
        let _ = view.apply_action_for_test(Action::Rename);
        let _ = view.update(Message::RenameChanged("b".to_owned()));

        let (_, event) = view.update(Message::RenameResult(
            Location::local("/home"),
            OsString::from("a"),
            Err(VfsError::AlreadyExists {
                location: "/home/b".to_owned(),
            }),
        ));
        assert!(event.is_none());
        let state = view
            .rename_state()
            .expect("edit should stay open on failure");
        assert!(state.error.is_some());
        // Nothing renamed in `entries` — the backend call never succeeded.
        assert!(view.entries.iter().any(|e| e.name == "a"));
    }

    #[test]
    fn create_result_success_reloads_selects_and_starts_renaming_the_new_entry() {
        let mut view = listed_view(vec![file("existing")]);
        let (_, event) = view.update(Message::CreateResult(
            Location::local("/home"),
            OsString::from("New Folder"),
            Ok(()),
        ));
        // Stage 10: a successful create bubbles `Event::Created` so `App`
        // can push it onto the undo stack.
        match event {
            Some(Event::Created(created)) => {
                assert_eq!(created, Location::local("/home/New Folder"));
            }
            other => panic!("expected Created, got {other:?}"),
        }
        assert!(view.is_loading());

        // The reload lands, carrying the freshly created entry.
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("existing"), dir("New Folder")]),
        ));
        assert!(view.selection.is_selected(OsStr::new("New Folder")));
        let state = view
            .rename_state()
            .expect("New Folder should start already in rename mode");
        assert_eq!(state.original, OsString::from("New Folder"));
    }

    #[test]
    fn create_result_for_a_stale_directory_is_dropped() {
        let mut view = listed_view(vec![]);
        let _ = view.navigate(Location::local("/elsewhere"));
        let (_, event) = view.update(Message::CreateResult(
            Location::local("/home"),
            OsString::from("New Folder"),
            Ok(()),
        ));
        assert!(event.is_none());
        assert_eq!(view.location(), &Location::local("/elsewhere"));
        assert!(view.rename_state().is_none());
    }
}
