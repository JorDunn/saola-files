//! Navigation: building a view pointed somewhere (`open`/`open_target`/
//! `open_select`), moving it (`navigate`, per-view back/forward history,
//! `load`), revealing a `--select` target once its listing lands, and the
//! breadcrumb/path-editor messages that request going somewhere new.

use std::ffi::OsString;
use std::path::PathBuf;

use iced::Task;

use crate::config::Config;
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::vfs::{Location, VfsError};
use crate::tr;

use super::{DirectoryView, Event, Message};

impl DirectoryView {
    /// `update`'s `Message::Listed` arm, moved here verbatim.
    pub(super) fn on_listed(
        &mut self,
        location: Location,
        result: Result<Vec<FileEntry>, VfsError>,
    ) -> (Task<Message>, Option<Event>) {
        if location != self.location {
            // A later navigation raced ahead of this response.
            return (Task::none(), None);
        }
        self.loading = false;
        match result {
            Ok(entries) => {
                self.listed = true;
                self.entries = entries;
                self.recompute_visible();
                let task = self.apply_pending_select();
                (task, self.thumbnail_event())
            }
            Err(err) => {
                self.entries.clear();
                self.visible.clear();
                self.error = Some(err);
                (Task::none(), None)
            }
        }
    }

    /// `update`'s `Message::TargetResolved` arm, moved here verbatim.
    pub(super) fn on_target_resolved(
        &mut self,
        location: Location,
        select: Option<OsString>,
        result: Result<Vec<FileEntry>, VfsError>,
    ) -> (Task<Message>, Option<Event>) {
        self.location = location.clone();
        self.pending_select = select;
        self.update(Message::Listed(location, result))
    }

    /// `update`'s `Message::BreadcrumbClicked` arm, moved here verbatim.
    pub(super) fn on_breadcrumb_clicked(
        &mut self,
        location: Location,
    ) -> (Task<Message>, Option<Event>) {
        self.path_edit = None;
        (Task::none(), Some(Event::OpenDirectory(location)))
    }

    /// `update`'s `Message::PathInputChanged` arm, moved here verbatim.
    pub(super) fn on_path_input_changed(
        &mut self,
        value: String,
    ) -> (Task<Message>, Option<Event>) {
        self.path_edit = Some(value);
        (Task::none(), None)
    }

    /// `update`'s `Message::PathSubmitted` arm, moved here verbatim.
    pub(super) fn on_path_submitted(&mut self) -> (Task<Message>, Option<Event>) {
        let Some(buffer) = self.path_edit.take() else {
            return (Task::none(), None);
        };
        let trimmed = buffer.trim();
        if trimmed.is_empty() {
            return (Task::none(), None);
        }
        (
            Task::none(),
            Some(Event::OpenDirectory(parse_typed_location(trimmed))),
        )
    }

    /// Open `location` directly and kick off its listing — the plain
    /// "no CLI target, no `--select`" startup path, and the general
    /// "build a view pointed here" constructor `explorer`/future-tabs
    /// code can reuse.
    pub fn open(location: Location, config: &Config) -> (Self, Task<Message>) {
        let mut view = Self::new(location, config);
        let task = view.load();
        (view, task)
    }

    /// Open a CLI positional target (`saola-files PATH`). `PATH` may name
    /// either a directory (browse it) or a file (reveal it in its
    /// parent) — telling those apart means asking the backend, never
    /// `std::fs` directly (CLAUDE.md: all file access goes through a
    /// `Backend`), so this kicks off an async probe rather than deciding
    /// synchronously. `fallback` is used if the probe errors (e.g. the
    /// path doesn't exist).
    pub fn open_target(
        target: OsString,
        fallback: Location,
        config: &Config,
    ) -> (Self, Task<Message>) {
        let probed = Location::local(PathBuf::from(&target));
        let view = Self::new(probed.clone(), config);

        let task = Task::perform(
            async move {
                let Some(backend) = crate::modules::resolve(&probed) else {
                    let listing = list_with_fallback(&fallback).await;
                    return (fallback, None, listing);
                };
                match backend.metadata(&probed).await {
                    Ok(entry) if entry.kind == EntryKind::Directory => {
                        let listing = backend.list(&probed).await;
                        (probed, None, listing)
                    }
                    Ok(entry) => {
                        let parent = probed.parent().unwrap_or_else(|| probed.clone());
                        let listing = backend.list(&parent).await;
                        (parent, Some(entry.name), listing)
                    }
                    Err(_) => {
                        let listing = backend.list(&fallback).await;
                        (fallback, None, listing)
                    }
                }
            },
            |(location, select, result)| Message::TargetResolved(location, select, result),
        );

        (view, task)
    }

    /// Open `--select PATH`: always reveal PATH's parent with PATH
    /// selected, whether PATH itself is a file or a directory (per
    /// `cli.rs`'s grammar) — no dir-vs-file probe needed, unlike
    /// [`Self::open_target`].
    pub fn open_select(
        select: PathBuf,
        fallback: Location,
        config: &Config,
    ) -> (Self, Task<Message>) {
        let (location, pending_select) = match (select.parent(), select.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => (
                Location::local(parent.to_path_buf()),
                Some(name.to_os_string()),
            ),
            _ => (fallback, None),
        };
        let mut view = Self::new(location, config);
        view.pending_select = pending_select;
        let task = view.load();
        (view, task)
    }

    /// The owner's response to `Event::OpenDirectory`: point this view at
    /// `location`, clear per-directory state, and kick off the listing.
    /// Never called from within `update()` itself — see the module docs.
    pub fn navigate(&mut self, location: Location) -> Task<Message> {
        self.history.push(self.location.clone());
        // Visiting anywhere new invalidates the redo stack — same as a
        // browser: you can't "forward" into a page you reached by
        // following a fresh link instead of going back from it.
        self.forward.clear();
        self.location = location;
        self.selection.clear();
        self.pending_select = None;
        self.error = None;
        self.path_edit = None;
        self.type_ahead.clear();
        self.menu_open = false;
        self.open_with_open = false;
        self.rename = None;
        self.pending_rename = false;
        self.load()
    }

    /// `Action::HistoryBack`: pop the most recent `back` entry, push the
    /// current location onto `forward` so `go_forward` can return to it,
    /// and load the popped location. A no-op `Task::none()` at the start
    /// of history (`history` empty) rather than anything the caller has
    /// to check first — matches every other guarded-index method in this
    /// file (CLAUDE.md's no-panic rule).
    pub(super) fn go_back(&mut self) -> Task<Message> {
        let Some(target) = self.history.pop() else {
            return Task::none();
        };
        self.forward.push(self.location.clone());
        self.enter(target)
    }

    /// The redo side of [`Self::go_back`].
    pub(super) fn go_forward(&mut self) -> Task<Message> {
        let Some(target) = self.forward.pop() else {
            return Task::none();
        };
        self.history.push(self.location.clone());
        self.enter(target)
    }

    /// Shared tail of `go_back`/`go_forward`: point this view at `target`
    /// and reload, without touching either history stack (the caller
    /// already did that) — unlike `navigate`, which is the "went somewhere
    /// new" path and always clears `forward`.
    fn enter(&mut self, target: Location) -> Task<Message> {
        self.location = target;
        self.selection.clear();
        self.pending_select = None;
        self.error = None;
        self.path_edit = None;
        self.type_ahead.clear();
        self.menu_open = false;
        self.open_with_open = false;
        self.rename = None;
        self.pending_rename = false;
        self.load()
    }

    /// Fetch `self.location`'s listing.
    pub(super) fn load(&mut self) -> Task<Message> {
        self.loading = true;
        self.listed = false;
        let location_for_task = self.location.clone();
        let location_for_message = self.location.clone();
        Task::perform(
            async move {
                let backend = crate::modules::resolve(&location_for_task);
                match backend {
                    Some(backend) => backend.list(&location_for_task).await,
                    None => Err(VfsError::Other {
                        message: tr!(
                            "error-no-backend",
                            scheme = location_for_task.scheme.as_str()
                        ),
                    }),
                }
            },
            move |result| Message::Listed(location_for_message.clone(), result),
        )
    }

    /// If `pending_select` names an entry in the just-loaded listing,
    /// select it and put the cursor there; otherwise leave the selection
    /// as-is (a `--select` target that vanished before we got here is not
    /// worth an error). When `pending_rename` is also set (Stage 8's New
    /// Folder/New File flow), additionally starts inline-renaming that
    /// same entry — see `pending_rename`'s doc comment.
    fn apply_pending_select(&mut self) -> Task<Message> {
        let Some(name) = self.pending_select.take() else {
            self.pending_rename = false;
            return Task::none();
        };
        let found = self
            .visible
            .iter()
            .position(|&i| self.entries.get(i).is_some_and(|entry| entry.name == name));
        let task = match found {
            Some(index) => {
                self.selection.click(index, name.clone());
                if self.pending_rename {
                    self.start_rename_named(name)
                } else {
                    Task::none()
                }
            }
            None => Task::none(),
        };
        self.pending_rename = false;
        task
    }
}

/// `open_target`'s "no backend for this scheme" fallback path: list
/// `fallback` directly rather than leaving the view permanently empty.
async fn list_with_fallback(fallback: &Location) -> Result<Vec<FileEntry>, VfsError> {
    match crate::modules::resolve(fallback) {
        Some(backend) => backend.list(fallback).await,
        None => Err(VfsError::Other {
            message: tr!("error-no-backend", scheme = fallback.scheme.as_str()),
        }),
    }
}

/// Parses the breadcrumb path/URI editor's submitted text into a
/// [`Location`]. A thin wrapper over [`Location::parse`] (Stage 7 lifted
/// the actual grammar up to `core::vfs` so `core::places`' saved-server
/// entries can share it too) — kept as a named function here purely so the
/// call site below reads as "what the user typed", not "the shared URI
/// grammar".
fn parse_typed_location(input: &str) -> Location {
    Location::parse(input)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::keymap::Action;
    use std::ffi::OsStr;

    #[test]
    fn open_marks_the_view_loading_and_returns_a_task() {
        let (view, _task) = DirectoryView::open(Location::local("/home"), &config());
        assert!(view.is_loading());
    }

    #[test]
    fn stale_listed_response_is_ignored_after_a_navigation() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.navigate(Location::local("/elsewhere"));
        // A slow response for the *old* location arrives after the new
        // navigation already started.
        let (_, event) = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("stale")]),
        ));
        assert!(event.is_none());
        assert_eq!(view.entries, vec![file("a")]); // untouched by the stale response
    }

    #[test]
    fn pending_select_highlights_the_revealed_entry_once_listed() {
        let mut view = DirectoryView::new(Location::local("/home"), &config());
        view.pending_select = Some(OsString::from("b"));
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Ok(vec![file("a"), file("b")]),
        ));
        assert!(view.selection.is_selected(OsStr::new("b")));
        assert!(view.pending_select.is_none());
    }

    #[test]
    fn a_listing_error_clears_entries_and_records_the_error() {
        let mut view = listed_view(vec![file("a")]);
        let _ = view.update(Message::Listed(
            Location::local("/home"),
            Err(VfsError::PermissionDenied {
                location: "/home".to_owned(),
            }),
        ));
        assert!(view.entries.is_empty());
        assert!(view.error.is_some());
    }

    // ── Stage 4: history ────────────────────────────────────────────────

    #[test]
    fn navigate_pushes_back_and_clears_forward() {
        let mut view = listed_view(vec![]);
        assert!(!view.can_go_back());
        let _ = view.navigate(Location::local("/elsewhere"));
        assert!(view.can_go_back());
        assert!(!view.can_go_forward());
        assert_eq!(view.location(), &Location::local("/elsewhere"));
    }

    #[test]
    fn history_back_then_forward_round_trips() {
        let mut view = listed_view(vec![]); // starts at /home
        let _ = view.navigate(Location::local("/a"));
        let _ = view.navigate(Location::local("/b"));
        assert_eq!(view.location(), &Location::local("/b"));

        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &Location::local("/a"));
        assert!(view.can_go_forward());

        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &Location::local("/home"));
        assert!(!view.can_go_back());

        let _ = view.apply_action_for_test(Action::HistoryForward);
        assert_eq!(view.location(), &Location::local("/a"));
        let _ = view.apply_action_for_test(Action::HistoryForward);
        assert_eq!(view.location(), &Location::local("/b"));
        assert!(!view.can_go_forward());
    }

    #[test]
    fn history_back_at_the_start_is_a_no_op() {
        let mut view = listed_view(vec![]);
        let start = view.location().clone();
        let _ = view.apply_action_for_test(Action::HistoryBack);
        assert_eq!(view.location(), &start);
    }

    #[test]
    fn a_fresh_navigate_after_going_back_drops_the_old_forward_branch() {
        let mut view = listed_view(vec![]); // /home
        let _ = view.navigate(Location::local("/a"));
        let _ = view.apply_action_for_test(Action::HistoryBack); // back to /home, /a on forward
        assert!(view.can_go_forward());

        // Going somewhere new (not via forward) invalidates that redo path.
        let _ = view.navigate(Location::local("/c"));
        assert!(!view.can_go_forward());
    }

    // ── Stage 4: breadcrumb/path editing ────────────────────────────────

    #[test]
    fn edit_path_action_seeds_the_buffer_from_the_current_location() {
        let mut view = listed_view(vec![]);
        assert_eq!(view.path_edit(), None);
        let _ = view.apply_action_for_test(Action::EditPath);
        assert_eq!(view.path_edit(), Some("/home"));
    }

    #[test]
    fn path_submitted_requests_navigation_and_closes_the_editor() {
        let mut view = listed_view(vec![]);
        let _ = view.apply_action_for_test(Action::EditPath);
        let _ = view.update(Message::PathInputChanged("/etc".to_owned()));
        let (_, event) = view.update(Message::PathSubmitted);
        assert_eq!(view.path_edit(), None);
        match event {
            Some(Event::OpenDirectory(location)) => {
                assert_eq!(location, Location::local("/etc"));
            }
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn submitting_a_blank_path_is_a_no_op() {
        let mut view = listed_view(vec![]);
        let _ = view.apply_action_for_test(Action::EditPath);
        let _ = view.update(Message::PathInputChanged("   ".to_owned()));
        let (_, event) = view.update(Message::PathSubmitted);
        assert!(event.is_none());
    }

    #[test]
    fn breadcrumb_clicked_requests_navigation() {
        let mut view = listed_view(vec![]);
        let (_, event) = view.update(Message::BreadcrumbClicked(Location::local("/")));
        match event {
            Some(Event::OpenDirectory(location)) => assert_eq!(location, Location::local("/")),
            other => panic!("expected OpenDirectory, got {other:?}"),
        }
    }

    #[test]
    fn parse_typed_location_handles_local_and_remote_forms() {
        assert_eq!(
            parse_typed_location("/home/jordan"),
            Location::local("/home/jordan")
        );
        assert_eq!(
            parse_typed_location("sftp://jordan@host/srv"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/srv"),
            }
        );
        assert_eq!(
            parse_typed_location("sftp://jordan@host"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/"),
            }
        );
    }
}
