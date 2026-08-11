//! Session undo stack (Stage 10) — Ctrl+Z reverses the most recent
//! invertible op. **Session-scoped, in-memory, no persistence across
//! restarts** (PLAN.md never asked for that), and **no redo**: `UndoStack`
//! is a plain `Vec` that only ever grows (on a successful invertible op)
//! or shrinks by one from the top (on undo) — there is nowhere a redone
//! entry would go back to.
//!
//! **What's invertible, and why "Copy is not undoable" (Jordan's decision,
//! PLAN.md verbatim).** A Copy leaves the source untouched and only ever
//! *adds* bytes at the destination; "undoing" it would mean deleting
//! whatever landed there, but a conflict-resolved Copy (`Overwrite`) may
//! have destructively replaced something that was already at the
//! destination, which a plain delete can never restore — there is no
//! single correct definition of "undo a copy" once conflicts are in play,
//! so this crate doesn't try. `Move`/`Rename`/`Trash`/`New` don't have
//! that problem the same way (see each variant's own doc comment for the
//! precise invertibility argument), which is why they get pushed here and
//! Copy never does — `main.rs` simply never constructs an `UndoEntry` for
//! an `ops::OpKind::Copy` paste.
//!
//! **Remote-ops capability gating.** [`can_undo_rename`] is the one gate
//! every `Move`/`Rename` push goes through before landing on the stack:
//! `from`/`to` must share a backend (scheme + authority) *and* that
//! backend must claim [`Caps::RENAME_IN_PLACE`]. This mirrors
//! `core::fs::ops`'s own same-backend-rename fast path exactly (see that
//! module's doc comment) — undo's `Backend::rename` call *is* that fast
//! path, run in reverse, so a backend that can't do it forward (falling
//! back to a streamed copy+delete instead) can't be trusted to do it
//! backward either. Only the local backend claims `RENAME_IN_PLACE` as of
//! this stage, so today this gate is equivalent to "local only" — but it's
//! written as a capability check, not an `is_local()` check, so a future
//! backend that *does* support real in-place rename gets undo for free
//! without this module changing at all.
//!
//! **Known, stated gap: a Move that hit even one conflict prompt is never
//! pushed.** `main.rs::handle_op_event`'s `Finished` arm only builds a
//! `Move` entry when the just-finished op saw zero `OpEvent::Conflict`s.
//! An `Overwrite` conflict destructively replaces whatever was already at
//! the destination — undoing by renaming back would silently discard that
//! fact, the same non-invertibility problem Copy has; `Skip`/`RenameCopy`
//! change which items actually moved and to what final names, which this
//! module has no way to learn after the fact (the ops engine's event
//! stream reports byte-progress, not a `(source, final_dest)` map — see
//! `core::fs::ops`'s own module doc comment on what it deliberately
//! doesn't build). Excluding the whole op the moment *any* item in it hit
//! a conflict is the simple, honest choice over trying to partially
//! reconstruct which items are safe.

use crate::core::fs::trash::TrashId;
use crate::core::vfs::{Caps, Location};

/// One undo-able action, pushed by `main.rs` the moment the corresponding
/// operation completes successfully (see each push call site: `Move` in
/// `App::handle_op_event`'s `Finished` arm, the rest in
/// `App::handle_directory_event`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoEntry {
    /// A completed Cut→Paste (`ops::OpKind::Move`): `(original, moved_to)`
    /// pairs for every top-level source the op actually moved. Only ever
    /// built when the whole op hit zero conflicts and every pair passed
    /// [`can_undo_rename`] — see the module doc comment. Undoing renames
    /// every pair back, `to -> from`, via a direct `Backend::rename` — the
    /// exact reverse of the same-backend fast path `core::fs::ops` itself
    /// would have taken forming this op in the first place.
    Move { pairs: Vec<(Location, Location)> },
    /// A completed inline rename (F2 / "Rename…"): `from` is the entry's
    /// location before, `to` is where it landed. Always invertible in the
    /// same sense a `Move` pair is — a rename is a same-directory
    /// `Backend::rename`, nothing else touched.
    Rename { from: Location, to: Location },
    /// A trash-delete (Delete key / the context menu's row, where
    /// `Caps::TRASH`): `id` is everything `core::fs::trash::restore` needs
    /// — see that function's own doc comment ("undoing a trash-delete is
    /// exactly `restore(&id)` — nothing else to build", carried over
    /// verbatim from the Stage 9 handoff). `original` is kept only for
    /// [`UndoEntry::label`]'s wording; `restore` itself re-derives the
    /// real original path from the `.trashinfo` sidecar `id` points at,
    /// so `original` here is display-only and never fed back into the
    /// actual restore call.
    Trash { id: TrashId, original: Location },
    /// A freshly created folder/file (Ctrl+Shift+N / the context menu's
    /// New Folder/New File): undoing is `Backend::remove` on exactly where
    /// it landed. Always safe to invert — nothing else in the directory
    /// depends on a brand new, still-empty entry existing.
    New { created: Location },
}

impl UndoEntry {
    /// Short, human-facing wording for the undo toast — built from the
    /// entry's own data rather than anything re-derived at render time, so
    /// `ui::dialogs::undo_toast` never needs to pattern-match the variant
    /// itself (the same "translate once, render the translation" split
    /// `ui::dialogs::progress`'s own module doc comment describes for
    /// `OpEvent` vs. `Progress`).
    pub fn label(&self) -> String {
        match self {
            UndoEntry::Move { pairs } => match pairs.as_slice() {
                [] => "Nothing to undo".to_owned(), // defensive — never pushed empty
                [(from, _)] => format!("Moved \"{}\"", display_name(from)),
                many => format!("Moved {} items", many.len()),
            },
            UndoEntry::Rename { from, .. } => format!("Renamed \"{}\"", display_name(from)),
            UndoEntry::Trash { original, .. } => format!("Deleted \"{}\"", display_name(original)),
            UndoEntry::New { created } => format!("Created \"{}\"", display_name(created)),
        }
    }
}

/// The file/dir name a [`UndoEntry::label`] shows — `to_string_lossy` is
/// fine here (CLAUDE.md's OsString discipline: "`to_string_lossy` only at
/// view time", and a toast label is exactly view time), falling back to
/// the full `Location`'s own `Display` for the vanishingly unlikely case
/// of a path with no final component at all.
fn display_name(location: &Location) -> String {
    location
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| location.to_string())
}

/// True when a `Backend::rename(to, from)` call is a trustworthy way to
/// invert a `from -> to` move/rename: same backend (scheme + authority —
/// `core::fs::ops::same_backend`'s own check, not duplicated as a `pub`
/// item there purely because this module has no other reason to depend on
/// `core::fs::ops`) and that backend claims [`Caps::RENAME_IN_PLACE`]. See
/// the module doc comment's "Remote-ops capability gating" section for the
/// full reasoning.
pub fn can_undo_rename(from: &Location, to: &Location) -> bool {
    if from.scheme != to.scheme || from.authority != to.authority {
        return false;
    }
    crate::modules::resolve(from)
        .map(|backend| backend.caps().contains(Caps::RENAME_IN_PLACE))
        .unwrap_or(false)
}

/// The session undo stack itself: `App` owns exactly one (CLAUDE.md:
/// "Shared caches … live on the App, never per-view"), the same
/// one-per-`App` posture `ops::OpIdSource`/`ops::Clipboard` already take.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: UndoEntry) {
        self.entries.push(entry);
    }

    /// Pops the most recent entry — `main.rs::App::start_undo`'s first
    /// step. Popped *before* the async `apply` call even starts (there is
    /// no redo to put it back for, and re-attempting a failed undo isn't
    /// this stage's job — see [`apply`]'s doc comment).
    pub fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The label the undo toast would show for the current top of the
    /// stack, without consuming it — `main.rs` calls this once right after
    /// a push to seed `App::undo_toast`; it never re-derives a label from
    /// a live stack afterward (the toast holds its own frozen copy so it
    /// keeps reading correctly even after a later push replaces the top).
    pub fn peek_label(&self) -> Option<String> {
        self.entries.last().map(UndoEntry::label)
    }
}

/// Performs one [`UndoEntry`]'s inversion. Consuming (`self`, not `&self`)
/// because an entry is popped off the stack before this is ever called —
/// there is nothing left to put back on a failure (no redo, and retrying a
/// failed undo automatically would risk repeating whatever made it fail
/// the first time, e.g. a permission error). `main.rs::App::start_undo`
/// runs this via `Task::perform` and words a failure to stderr, the same
/// "no error-dialog surface yet" posture every other backend failure in
/// that file already takes.
pub async fn apply(entry: UndoEntry) -> Result<(), String> {
    match entry {
        UndoEntry::Move { pairs } => {
            // Best-effort per pair: one failed rename-back (a permission
            // change, or something else claimed the original name in the
            // meantime) doesn't stop the rest from being restored. The
            // first error is what's reported; every pair is still
            // attempted regardless.
            let mut first_err = None;
            for (from, to) in pairs {
                if let Err(err) = rename_back(&to, &from).await {
                    first_err.get_or_insert(err);
                }
            }
            match first_err {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
        UndoEntry::Rename { from, to } => rename_back(&to, &from).await,
        UndoEntry::Trash { id, .. } => {
            // `restore` is blocking `std::fs` I/O (see `core::fs::trash`'s
            // own module doc comment on why it's allowed to be) — run on a
            // blocking-pool thread rather than the async task calling this,
            // the same `spawn_blocking` + `JoinError`-mapping shape
            // `ui::trashview`'s own `run_blocking` helper uses.
            match tokio::task::spawn_blocking(move || crate::core::fs::trash::restore(&id)).await {
                Ok(Ok(_path)) => Ok(()),
                Ok(Err(err)) => Err(err.to_string()),
                Err(join_err) => Err(format!("internal error undoing a delete: {join_err}")),
            }
        }
        UndoEntry::New { created } => remove_one(&created).await,
    }
}

/// `Backend::rename(from, to)`, worded as a plain `String` error — every
/// `UndoEntry` variant's inversion bottoms out in either this or
/// [`remove_one`], both of which resolve a fresh backend per call
/// (backends are cheap to construct — `modules::resolve`'s own doc
/// comment) rather than threading one through from the push site.
async fn rename_back(from: &Location, to: &Location) -> Result<(), String> {
    let Some(backend) = crate::modules::resolve(from) else {
        return Err(format!("no backend for scheme \"{}\"", from.scheme));
    };
    backend
        .rename(from, to)
        .await
        .map_err(|err| err.to_string())
}

async fn remove_one(location: &Location) -> Result<(), String> {
    let Some(backend) = crate::modules::resolve(location) else {
        return Err(format!("no backend for scheme \"{}\"", location.scheme));
    };
    backend
        .remove(location)
        .await
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── tiny temp-dir helpers, matching every other `core::fs::` test
    // module's own copies (see e.g. `core::fs::ops`'s tests for why each
    // file keeps its own rather than sharing one) ───────────────────────

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-undo-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── UndoStack: pure logic, no I/O ───────────────────────────────────

    #[test]
    fn stack_starts_empty() {
        let stack = UndoStack::new();
        assert!(stack.is_empty());
        assert_eq!(stack.peek_label(), None);
    }

    #[test]
    fn push_then_pop_round_trips_the_same_entry() {
        let mut stack = UndoStack::new();
        let entry = UndoEntry::New {
            created: Location::local("/tmp/New Folder"),
        };
        stack.push(entry.clone());
        assert!(!stack.is_empty());
        assert_eq!(stack.pop(), Some(entry));
        assert!(stack.is_empty());
    }

    #[test]
    fn pop_on_an_empty_stack_is_none() {
        let mut stack = UndoStack::new();
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn peek_label_reflects_the_most_recently_pushed_entry() {
        let mut stack = UndoStack::new();
        stack.push(UndoEntry::New {
            created: Location::local("/tmp/a"),
        });
        stack.push(UndoEntry::New {
            created: Location::local("/tmp/b"),
        });
        assert_eq!(stack.peek_label(), Some("Created \"b\"".to_owned()));
    }

    // ── UndoEntry::label ─────────────────────────────────────────────────

    #[test]
    fn move_label_names_a_lone_item_but_counts_a_multi_move() {
        let one = UndoEntry::Move {
            pairs: vec![(Location::local("/a/x.txt"), Location::local("/b/x.txt"))],
        };
        assert_eq!(one.label(), "Moved \"x.txt\"");

        let many = UndoEntry::Move {
            pairs: vec![
                (Location::local("/a/x.txt"), Location::local("/b/x.txt")),
                (Location::local("/a/y.txt"), Location::local("/b/y.txt")),
            ],
        };
        assert_eq!(many.label(), "Moved 2 items");
    }

    #[test]
    fn rename_trash_new_labels_name_the_original() {
        let rename = UndoEntry::Rename {
            from: Location::local("/a/old.txt"),
            to: Location::local("/a/new.txt"),
        };
        assert_eq!(rename.label(), "Renamed \"old.txt\"");

        let created = UndoEntry::New {
            created: Location::local("/a/New Folder"),
        };
        assert_eq!(created.label(), "Created \"New Folder\"");
    }

    // ── can_undo_rename ──────────────────────────────────────────────────

    #[test]
    fn can_undo_rename_is_true_for_two_local_locations() {
        assert!(can_undo_rename(
            &Location::local("/a/old.txt"),
            &Location::local("/a/new.txt")
        ));
    }

    #[test]
    fn can_undo_rename_is_false_across_schemes() {
        let local = Location::local("/a/x.txt");
        let remote = Location {
            scheme: "sftp".to_owned(),
            authority: Some("host".to_owned()),
            path: PathBuf::from("/a/x.txt"),
        };
        assert!(!can_undo_rename(&local, &remote));
    }

    #[test]
    fn can_undo_rename_is_false_for_an_unregistered_scheme() {
        let a = Location {
            scheme: "gopher".to_owned(),
            authority: None,
            path: PathBuf::from("/a"),
        };
        let b = Location {
            scheme: "gopher".to_owned(),
            authority: None,
            path: PathBuf::from("/b"),
        };
        assert!(!can_undo_rename(&a, &b));
    }

    // ── apply: real temp-dir I/O round trips, one per invertible kind ───

    #[tokio::test]
    async fn apply_rename_moves_the_entry_back_to_its_original_name() {
        let dir = tempdir();
        let original = dir.join("old.txt");
        let renamed = dir.join("new.txt");
        std::fs::write(&renamed, b"hello").unwrap();

        let entry = UndoEntry::Rename {
            from: Location::local(&original),
            to: Location::local(&renamed),
        };
        apply(entry).await.unwrap();

        assert!(original.exists(), "back at its original name");
        assert!(!renamed.exists());
        assert_eq!(std::fs::read(&original).unwrap(), b"hello");

        cleanup(dir);
    }

    #[tokio::test]
    async fn apply_move_reverses_every_pair_even_if_paired_with_others() {
        let src_root = tempdir();
        let dst_root = tempdir();
        std::fs::write(src_root.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src_root.join("b.txt"), b"bbb").unwrap();
        // Simulate the paste having already happened: `a`/`b` now live
        // under `dst_root`.
        std::fs::rename(src_root.join("a.txt"), dst_root.join("a.txt")).unwrap();
        std::fs::rename(src_root.join("b.txt"), dst_root.join("b.txt")).unwrap();

        let entry = UndoEntry::Move {
            pairs: vec![
                (
                    Location::local(src_root.join("a.txt")),
                    Location::local(dst_root.join("a.txt")),
                ),
                (
                    Location::local(src_root.join("b.txt")),
                    Location::local(dst_root.join("b.txt")),
                ),
            ],
        };
        apply(entry).await.unwrap();

        assert_eq!(std::fs::read(src_root.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(src_root.join("b.txt")).unwrap(), b"bbb");
        assert!(!dst_root.join("a.txt").exists());
        assert!(!dst_root.join("b.txt").exists());

        cleanup(src_root);
        cleanup(dst_root);
    }

    #[tokio::test]
    async fn apply_new_removes_the_freshly_created_entry() {
        let dir = tempdir();
        let created = dir.join("New Folder");
        std::fs::create_dir(&created).unwrap();

        let entry = UndoEntry::New {
            created: Location::local(&created),
        };
        apply(entry).await.unwrap();

        assert!(!created.exists());
        cleanup(dir);
    }

    #[tokio::test]
    async fn apply_trash_restores_via_the_real_trash_id() {
        let source_dir = tempdir();
        let home_trash = tempdir().join("Trash");
        let path = source_dir.join("doc.txt");
        std::fs::write(&path, b"contents").unwrap();

        // `trash::trash_into` is the same env-free testable core
        // `core::fs::trash`'s own tests exercise directly — reusing it
        // here avoids either touching the real `$HOME` trash (what the
        // public `trash()` wrapper would do) or duplicating trash-target
        // setup, and CLAUDE.md bans `std::env::set_var` in tests, which is
        // the only other way to redirect `trash()` itself.
        let id = crate::core::fs::trash::trash_into(&path, &home_trash).unwrap();
        assert!(!path.exists(), "moved into the trash by trash_into");

        let entry = UndoEntry::Trash {
            id,
            original: Location::local(&path),
        };
        apply(entry).await.unwrap();

        assert!(path.exists(), "restored to its original location");
        assert_eq!(std::fs::read(&path).unwrap(), b"contents");

        cleanup(source_dir);
        cleanup(home_trash.parent().unwrap().to_path_buf());
    }

    #[tokio::test]
    async fn apply_rename_reports_an_error_without_panicking_when_the_source_is_gone() {
        let dir = tempdir();
        // `to` doesn't exist — the rename-back has nothing to move.
        let entry = UndoEntry::Rename {
            from: Location::local(dir.join("old.txt")),
            to: Location::local(dir.join("never-existed.txt")),
        };
        let result = apply(entry).await;
        assert!(result.is_err());
        cleanup(dir);
    }
}
