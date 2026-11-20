//! Failures worded for a person: what was being done, to what, and why it
//! didn't work.
//!
//! Everything that can fail on a person's behalf (copy, move, rename,
//! trash, mkdir, open, launch, ...) funnels through one vocabulary here, so
//! the window can show "Couldn't move “a.txt” to “Docs”: permission denied."
//! instead of a silent stderr line. This module is pure: it maps data to
//! sentences and knows nothing about iced. `ui::dialogs::error_notice`
//! draws the result; `App` decides when to show it.
//!
//! # Pieces
//!
//! - [`Operation`] — what the person asked for.
//! - [`Cause`] — what went wrong: a [`VfsError`], or one of the launch
//!   problems that are not filesystem errors (no app, app uninstalled, the
//!   program missing).
//! - [`Failure`] — one thing that failed: operation target, optional
//!   destination, cause.
//! - [`describe`] — one failure as one sentence.
//! - [`summarize`] — several failures from one operation as a headline
//!   ("3 items couldn't be copied") plus one detail line each.
//!
//! # Severity is wording
//!
//! The style guide has no danger colour; the sentence carries the severity.
//! Nothing here (or in the notice that draws it) picks a colour.
//!
//! # Why error text is classified here
//!
//! [`VfsError`] has specific variants for the common cases but folds
//! everything else into `Other { message }`. "No space left on device" and
//! "cross-device link" arrive as `Other`, with the OS error number in the
//! message (`io::Error`'s `Display` ends in `(os error 28)`). [`reason`]
//! recognises those two numbers so they get their own wording. A cleaner
//! fix is dedicated `VfsError` variants produced by `modules::local`; that
//! touches the backend layer and is left to its own task. When it lands,
//! only [`reason`] changes.
//!
//! Filenames stay `OsString` until [`describe`] needs a sentence, and are
//! passed to the catalogue as arguments, never spliced into message text.

use std::ffi::OsString;
use std::io;

use crate::core::vfs::{Location, VfsError};
use crate::tr;

/// One user-visible incident: a single user action (or one engine
/// operation) and everything that failed inside it. Failures aggregate into
/// one notice only when they share an `Incident`, and a failure from an
/// older incident never displaces a newer notice.
///
/// Ordered: a later-minted incident compares greater. Mint one with
/// [`Incident::mint`] when the action *starts* and carry it (it is `Copy`)
/// through the tasks and result messages the action spawns, so ordering
/// reflects when the person asked, not when a result happened to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Incident(u64);

impl Incident {
    /// A new incident, newer than every one minted before it.
    ///
    /// Teaching note: the counter is a process-wide `AtomicU64` rather than
    /// a field on `App`, because incidents are minted from several places
    /// (the app, a directory view starting New Folder, the trash browser)
    /// that must all agree on one global order. `fetch_add` is a single
    /// indivisible "read, add one, write back", so two callers can never get
    /// the same number; `Relaxed` is enough because only the number matters,
    /// not ordering against other memory.
    pub fn mint() -> Incident {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Incident(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// What the person asked for. Picks the sentence in [`describe`] and the
/// headline in [`summarize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Copy,
    Move,
    Rename,
    /// Move to the trash.
    Trash,
    /// Permanent delete.
    Delete,
    /// New Folder or New File; the target is the new item's name and the
    /// destination the folder it was going into.
    Create,
    /// Open with the default app.
    Open,
    /// Open with a specific app; the app's name is the destination.
    OpenWith,
    /// "Open in terminal"; the target is the folder.
    OpenTerminal,
    /// A `[[action]]` from `files.toml`; the target is its command.
    RunAction,
    Undo,
    /// Restore from the trash.
    Restore,
    EmptyTrash,
    /// Publishing copied items to the system clipboard.
    Clipboard,
    /// Not a failure: the "this is a read-only copy" caveat shown after a
    /// file without a local path (SFTP, ...) was downloaded and opened.
    /// It rides the failure notice because that band already gives a
    /// message an incident, a dismiss button, Esc, long-name truncation and
    /// "a newer action replaces it". The target is the file's name; the
    /// cause is unused.
    OpenedCopy,
}

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A backend or filesystem error.
    Vfs(VfsError),
    /// Nothing is registered to open this kind of file.
    NoDefaultApp,
    /// The chosen app's launcher entry has disappeared since the menu was
    /// built (uninstalled while the window was open).
    AppUninstalled,
    /// Starting a program failed. Kept as `kind` plus text because
    /// `io::Error` is not `Clone`, and messages must be.
    Launch {
        kind: io::ErrorKind,
        message: String,
    },
}

impl Cause {
    /// A failed `Command::spawn` (or any `io::Error` from launching).
    pub fn launch(err: &io::Error) -> Self {
        Cause::Launch {
            kind: err.kind(),
            message: err.to_string(),
        }
    }
}

/// For the engines that report failure as plain text (undo, the system
/// clipboard): the text is shown as given.
impl From<String> for Cause {
    fn from(message: String) -> Self {
        Cause::Vfs(VfsError::Other { message })
    }
}

impl From<VfsError> for Cause {
    fn from(err: VfsError) -> Self {
        Cause::Vfs(err)
    }
}

/// One thing that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The item acted on: a file name, or for the operations without one,
    /// whatever [`Operation`] documents. Empty when there is none (undo,
    /// empty trash, clipboard).
    pub target: OsString,
    /// Where it was going: the destination folder (copy, move, create),
    /// the new name (rename) or the app's name (open with). `None` picks
    /// the sentence that doesn't mention one.
    pub destination: Option<OsString>,
    pub cause: Cause,
}

impl Failure {
    pub fn new(target: impl Into<OsString>, cause: impl Into<Cause>) -> Self {
        Failure {
            target: target.into(),
            destination: None,
            cause: cause.into(),
        }
    }

    /// A failure on `location`, named by its last path component (or the
    /// whole location when it has none, e.g. the root of a server).
    pub fn at(location: &Location, cause: impl Into<Cause>) -> Self {
        Failure::new(display_name(location), cause)
    }

    /// Adds the destination ([`Failure::destination`]).
    pub fn to(mut self, destination: impl Into<OsString>) -> Self {
        self.destination = Some(destination.into());
        self
    }

    /// Like [`Failure::to`], naming a destination folder by `location`.
    pub fn into_folder(self, location: &Location) -> Self {
        self.to(display_name(location))
    }
}

/// The short name for `location`: its final path component, or the whole
/// location (`sftp://host/`) when there isn't one.
pub fn display_name(location: &Location) -> OsString {
    match location.path.file_name() {
        Some(name) => name.to_owned(),
        None => OsString::from(location.to_string()),
    }
}

/// Why it failed, in the closed set of reasons the catalogue words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    PermissionDenied,
    NotFound,
    AlreadyExists,
    NoSpace,
    CrossDevice,
    ReadOnly,
    /// A remote backend dropped or could not be reached.
    Disconnected,
    NotAFolder,
    IsAFolder,
    NoDefaultApp,
    AppUninstalled,
    /// The program to launch does not exist.
    ProgramMissing,
    /// Anything else: the backend's own words.
    Other(String),
    /// The location can't do this at all (a backend without the matching
    /// capability); carries the refused operation as a noun phrase.
    Unsupported(String),
    /// An error that came with no words at all.
    Unknown,
}

/// Classifies `cause`. Pure; see the module docs for why `Other` text is
/// inspected.
pub fn reason(cause: &Cause) -> Reason {
    match cause {
        Cause::NoDefaultApp => Reason::NoDefaultApp,
        Cause::AppUninstalled => Reason::AppUninstalled,
        Cause::Launch { kind, message } => match kind {
            io::ErrorKind::NotFound => Reason::ProgramMissing,
            io::ErrorKind::PermissionDenied => Reason::PermissionDenied,
            _ => other_reason(message),
        },
        Cause::Vfs(err) => match err {
            VfsError::NotFound { .. } => Reason::NotFound,
            VfsError::PermissionDenied { .. } => Reason::PermissionDenied,
            VfsError::AlreadyExists { .. } => Reason::AlreadyExists,
            VfsError::NotADirectory { .. } => Reason::NotAFolder,
            VfsError::IsADirectory { .. } => Reason::IsAFolder,
            VfsError::Unavailable { .. } => Reason::Disconnected,
            VfsError::Unsupported { operation } => Reason::Unsupported(operation.clone()),
            VfsError::Other { message } => classify_other(message),
        },
    }
}

/// `io::Error`'s `Display` for an OS error *ends* in `(os error N)`, and
/// local backend messages are `"{path}: {err}"`. Only the end is trusted: a
/// file named `x (os error 28)` that fails for another reason must not be
/// reported as out of space. Linux errno values with their own wording:
/// ENOSPC (28) and EDQUOT (122) no space; EXDEV (18) cross-device; EROFS
/// (30) read-only.
fn classify_other(message: &str) -> Reason {
    let tail = message.trim_end();
    let ends = |errno: u32| tail.ends_with(&format!("(os error {errno})"));
    if ends(28) || ends(122) {
        Reason::NoSpace
    } else if ends(18) {
        Reason::CrossDevice
    } else if ends(30) {
        Reason::ReadOnly
    } else {
        other_reason(message)
    }
}

/// The backend's own words, or [`Reason::Unknown`] when there are none.
fn other_reason(message: &str) -> Reason {
    if message.trim().is_empty() {
        Reason::Unknown
    } else {
        Reason::Other(message.to_owned())
    }
}

/// The reason as a lower-case phrase that completes a sentence
/// ("... : permission denied.").
fn reason_text(reason: &Reason) -> String {
    match reason {
        Reason::PermissionDenied => tr!("failure-reason-permission-denied"),
        Reason::NotFound => tr!("failure-reason-not-found"),
        Reason::AlreadyExists => tr!("failure-reason-already-exists"),
        Reason::NoSpace => tr!("failure-reason-no-space"),
        Reason::CrossDevice => tr!("failure-reason-cross-device"),
        Reason::ReadOnly => tr!("failure-reason-read-only"),
        Reason::Disconnected => tr!("failure-reason-disconnected"),
        Reason::NotAFolder => tr!("failure-reason-not-a-folder"),
        Reason::IsAFolder => tr!("failure-reason-is-a-folder"),
        Reason::NoDefaultApp => tr!("failure-reason-no-default-app"),
        Reason::AppUninstalled => tr!("failure-reason-app-uninstalled"),
        Reason::ProgramMissing => tr!("failure-reason-program-missing"),
        // The sentence supplies its own full stop.
        Reason::Other(message) => {
            let words = message.trim().trim_end_matches('.').trim_end();
            if words.is_empty() {
                tr!("failure-reason-unknown")
            } else {
                tr!("failure-reason-other", message = words)
            }
        }
        Reason::Unsupported(operation) => {
            tr!("failure-reason-unsupported", operation = operation.as_str())
        }
        Reason::Unknown => tr!("failure-reason-unknown"),
    }
}

/// One failure as one sentence ending in a full stop.
pub fn describe(operation: Operation, failure: &Failure) -> String {
    let name = failure.target.to_string_lossy();
    let name = name.as_ref();
    let destination = failure
        .destination
        .as_ref()
        .map(|d| d.to_string_lossy().into_owned());
    let reason = reason_text(&reason(&failure.cause));
    let reason = reason.as_str();

    match (operation, destination.as_deref()) {
        (Operation::Copy, Some(dest)) => {
            tr!(
                "failure-copy-to",
                name = name,
                destination = dest,
                reason = reason
            )
        }
        (Operation::Copy, None) => tr!("failure-copy", name = name, reason = reason),
        (Operation::Move, Some(dest)) => {
            tr!(
                "failure-move-to",
                name = name,
                destination = dest,
                reason = reason
            )
        }
        (Operation::Move, None) => tr!("failure-move", name = name, reason = reason),
        (Operation::Rename, Some(new_name)) => {
            tr!(
                "failure-rename-to",
                name = name,
                destination = new_name,
                reason = reason
            )
        }
        (Operation::Rename, None) => tr!("failure-rename", name = name, reason = reason),
        (Operation::Trash, _) => tr!("failure-trash", name = name, reason = reason),
        (Operation::Delete, _) => tr!("failure-delete", name = name, reason = reason),
        (Operation::Create, Some(dest)) => {
            tr!(
                "failure-create-in",
                name = name,
                destination = dest,
                reason = reason
            )
        }
        (Operation::Create, None) => tr!("failure-create", name = name, reason = reason),
        (Operation::Open, _) => tr!("failure-open", name = name, reason = reason),
        (Operation::OpenWith, Some(app)) => {
            tr!(
                "failure-open-with",
                name = name,
                destination = app,
                reason = reason
            )
        }
        (Operation::OpenWith, None) => tr!("failure-open", name = name, reason = reason),
        (Operation::OpenTerminal, _) => tr!("failure-open-terminal", name = name, reason = reason),
        (Operation::RunAction, _) => tr!("failure-run-action", name = name, reason = reason),
        (Operation::Undo, _) => tr!("failure-undo", reason = reason),
        (Operation::Restore, _) if name.is_empty() => {
            tr!("failure-restore-unnamed", reason = reason)
        }
        (Operation::Restore, _) => tr!("failure-restore", name = name, reason = reason),
        (Operation::EmptyTrash, _) => tr!("failure-empty-trash", reason = reason),
        (Operation::Clipboard, _) => tr!("failure-clipboard", reason = reason),
        (Operation::OpenedCopy, _) => tr!("notice-opened-copy", name = name),
    }
}

/// How an operation's failures read as a group: the line in the notice's
/// strip, and the lines behind its Details button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub headline: String,
    /// One [`describe`] line per failure, in order. Empty for a single
    /// failure, whose headline already is that sentence — so "has details"
    /// is exactly "more than one thing failed".
    pub details: Vec<String>,
}

/// Groups failures of `operation` for display. `shown` is the stored
/// prefix (the notice keeps only the first few) and `total` how many
/// failed in all; `None` when there is nothing to show.
///
/// One failure: the full sentence, no details. Several: a count headline
/// ("3 items couldn't be copied") from `total`, and a sentence per *shown*
/// failure as detail lines. Only what is shown is formatted.
pub fn summarize(operation: Operation, shown: &[Failure], total: usize) -> Option<Summary> {
    let (first, _) = shown.split_first()?;
    if total <= 1 {
        return Some(Summary {
            headline: describe(operation, first),
            details: Vec::new(),
        });
    }
    Some(Summary {
        headline: group_headline(operation, total),
        details: shown.iter().map(|f| describe(operation, f)).collect(),
    })
}

/// The count headline for `count` (two or more) failures of `operation`.
/// Counts go into the catalogue as integers so each language picks its own
/// plural form.
fn group_headline(operation: Operation, count: usize) -> String {
    match operation {
        Operation::Copy => tr!("failure-summary-copy", count = count),
        Operation::Move => tr!("failure-summary-move", count = count),
        Operation::Rename => tr!("failure-summary-rename", count = count),
        Operation::Trash => tr!("failure-summary-trash", count = count),
        Operation::Delete => tr!("failure-summary-delete", count = count),
        Operation::Create => tr!("failure-summary-create", count = count),
        Operation::Open | Operation::OpenWith => tr!("failure-summary-open", count = count),
        Operation::Restore => tr!("failure-summary-restore", count = count),
        Operation::OpenTerminal
        | Operation::RunAction
        | Operation::Undo
        | Operation::EmptyTrash
        | Operation::Clipboard => tr!("failure-summary-other", count = count),
        Operation::OpenedCopy => tr!("notice-opened-copy-many", count = count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission_denied() -> VfsError {
        VfsError::PermissionDenied {
            location: "/x".to_owned(),
        }
    }

    fn other(message: &str) -> VfsError {
        VfsError::Other {
            message: message.to_owned(),
        }
    }

    // --- reason: every VfsError variant, plus the errno text cases ---

    #[test]
    fn every_vfs_error_variant_has_a_reason() {
        let loc = || "/x".to_owned();
        let cases = [
            (VfsError::NotFound { location: loc() }, Reason::NotFound),
            (permission_denied(), Reason::PermissionDenied),
            (
                VfsError::AlreadyExists { location: loc() },
                Reason::AlreadyExists,
            ),
            (
                VfsError::NotADirectory { location: loc() },
                Reason::NotAFolder,
            ),
            (
                VfsError::IsADirectory { location: loc() },
                Reason::IsAFolder,
            ),
            (
                VfsError::Unavailable {
                    message: "gone".to_owned(),
                },
                Reason::Disconnected,
            ),
            (
                VfsError::Unsupported {
                    operation: "creating links".to_owned(),
                },
                Reason::Unsupported("creating links".to_owned()),
            ),
            (other("odd"), Reason::Other("odd".to_owned())),
        ];
        for (err, expected) in cases {
            assert_eq!(reason(&Cause::Vfs(err.clone())), expected, "{err:?}");
        }
    }

    #[test]
    fn enospc_and_edquot_are_no_space() {
        for message in [
            "/x: No space left on device (os error 28)",
            "/x: Disk quota exceeded (os error 122)",
        ] {
            assert_eq!(reason(&Cause::Vfs(other(message))), Reason::NoSpace);
        }
    }

    #[test]
    fn a_file_named_like_an_errno_is_not_mistaken_for_one() {
        // The errno text is only trusted at the very end of the message.
        let text = "/x (os error 28): Text file busy (os error 26)";
        assert_eq!(
            reason(&Cause::Vfs(other(text))),
            Reason::Other(text.to_owned())
        );
        let real = Cause::Vfs(other(
            "/x (os error 18): No space left on device (os error 28)",
        ));
        assert_eq!(reason(&real), Reason::NoSpace);
    }

    #[test]
    fn an_empty_message_gets_a_real_sentence() {
        for message in ["", "   ", " . "] {
            let sentence = describe(Operation::Copy, &Failure::new("a", other(message)));
            assert_eq!(sentence, "Couldn't copy “a”: something went wrong.");
        }
    }

    #[test]
    fn a_restore_without_a_name_does_not_render_empty_quotes() {
        let sentence = describe(
            Operation::Restore,
            &Failure::new(OsString::new(), permission_denied()),
        );
        assert_eq!(
            sentence,
            "Couldn't restore an item from the trash: permission denied."
        );
    }

    #[test]
    fn incidents_are_strictly_increasing() {
        let (a, b) = (Incident::mint(), Incident::mint());
        assert!(a < b);
    }

    #[test]
    fn exdev_is_cross_device() {
        let cause = Cause::Vfs(other("/x: Invalid cross-device link (os error 18)"));
        assert_eq!(reason(&cause), Reason::CrossDevice);
    }

    #[test]
    fn erofs_is_read_only() {
        let cause = Cause::Vfs(other("/x: Read-only file system (os error 30)"));
        assert_eq!(reason(&cause), Reason::ReadOnly);
    }

    #[test]
    fn launch_errors_map_by_kind() {
        let missing = Cause::launch(&io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(reason(&missing), Reason::ProgramMissing);
        let denied = Cause::launch(&io::Error::from(io::ErrorKind::PermissionDenied));
        assert_eq!(reason(&denied), Reason::PermissionDenied);
        let odd = Cause::launch(&io::Error::other("boom"));
        assert_eq!(reason(&odd), Reason::Other("boom".to_owned()));
    }

    #[test]
    fn app_causes_have_their_own_reasons() {
        assert_eq!(reason(&Cause::NoDefaultApp), Reason::NoDefaultApp);
        assert_eq!(reason(&Cause::AppUninstalled), Reason::AppUninstalled);
    }

    // --- describe: wording, single target ---

    fn describe_move(cause: impl Into<Cause>) -> String {
        let failure = Failure::new("a.txt", cause).to("Docs");
        describe(Operation::Move, &failure)
    }

    #[test]
    fn the_issue_example_reads_as_written() {
        assert_eq!(
            describe_move(permission_denied()),
            "Couldn't move “a.txt” to “Docs”: permission denied."
        );
    }

    #[test]
    fn each_required_reason_has_specific_wording() {
        let expected = [
            (Cause::Vfs(permission_denied()), "permission denied"),
            (
                Cause::Vfs(VfsError::NotFound {
                    location: "/x".to_owned(),
                }),
                "it no longer exists",
            ),
            (
                Cause::Vfs(other("No space left on device (os error 28)")),
                "there isn't enough space",
            ),
            (
                Cause::Vfs(VfsError::AlreadyExists {
                    location: "/x".to_owned(),
                }),
                "something with that name already exists",
            ),
            (
                Cause::Vfs(other("Invalid cross-device link (os error 18)")),
                "the folders are on different drives",
            ),
            (
                Cause::Vfs(VfsError::Unavailable {
                    message: "x".to_owned(),
                }),
                "the connection was lost",
            ),
        ];
        for (cause, phrase) in expected {
            let sentence = describe_move(cause);
            assert!(sentence.contains(phrase), "{sentence:?} lacks {phrase:?}");
            assert!(sentence.ends_with('.'), "{sentence:?}");
        }
    }

    #[test]
    fn other_text_is_kept_and_not_given_a_double_full_stop() {
        let sentence = describe_move(other("the disk is on fire."));
        assert_eq!(
            sentence,
            "Couldn't move “a.txt” to “Docs”: the disk is on fire."
        );
    }

    #[test]
    fn a_failure_without_a_destination_drops_the_clause() {
        let failure = Failure::new("a.txt", permission_denied());
        assert_eq!(
            describe(Operation::Copy, &failure),
            "Couldn't copy “a.txt”: permission denied."
        );
    }

    #[test]
    fn every_operation_describes_without_leaking_an_id() {
        let ops = [
            Operation::Copy,
            Operation::Move,
            Operation::Rename,
            Operation::Trash,
            Operation::Delete,
            Operation::Create,
            Operation::Open,
            Operation::OpenWith,
            Operation::OpenTerminal,
            Operation::RunAction,
            Operation::Undo,
            Operation::Restore,
            Operation::EmptyTrash,
            Operation::Clipboard,
        ];
        for op in ops {
            for destination in [None, Some("Dest")] {
                let mut failure = Failure::new("thing", permission_denied());
                failure.destination = destination.map(OsString::from);
                let sentence = describe(op, &failure);
                // A missing catalogue id renders as the id itself.
                assert!(!sentence.starts_with("failure-"), "{op:?}: {sentence}");
                assert!(sentence.ends_with('.'), "{op:?}: {sentence}");
            }
        }
    }

    #[test]
    fn open_failures_explain_the_missing_app() {
        let none = Failure::new("a.xyz", Cause::NoDefaultApp);
        assert_eq!(
            describe(Operation::Open, &none),
            "Couldn't open “a.xyz”: no app is set up to open this kind of file."
        );
        let missing = Failure::new(
            "a.txt",
            Cause::launch(&io::Error::from(io::ErrorKind::NotFound)),
        );
        let sentence = describe(Operation::Open, &missing);
        assert!(sentence.contains("couldn't be found"), "{sentence}");
    }

    #[test]
    fn names_are_arguments_so_fluent_syntax_in_them_is_inert() {
        let failure = Failure::new("{ $reason } x", permission_denied());
        let sentence = describe(Operation::Trash, &failure);
        assert!(sentence.contains("{ $reason } x"), "{sentence}");
    }

    #[test]
    fn non_utf8_names_are_shown_lossily_and_never_panic() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(vec![b'a', 0xFF, b'b']);
        let failure = Failure::new(name, permission_denied());
        let sentence = describe(Operation::Delete, &failure);
        assert!(sentence.contains('\u{FFFD}'), "{sentence}");
    }

    #[test]
    fn at_names_a_location_by_its_last_component_or_whole() {
        let file = Failure::at(&Location::local("/home/me/a.txt"), permission_denied());
        assert_eq!(file.target, OsString::from("a.txt"));
        let root = Failure::at(&Location::local("/"), permission_denied());
        assert_eq!(root.target, OsString::from("/"));
    }

    // --- summarize: single vs several ---

    #[test]
    fn nothing_failed_is_no_summary() {
        assert_eq!(summarize(Operation::Copy, &[], 0), None);
    }

    #[test]
    fn one_failure_is_its_own_sentence_with_no_details() {
        let failure = Failure::new("a.txt", permission_denied()).to("Docs");
        let summary = summarize(Operation::Copy, std::slice::from_ref(&failure), 1);
        let summary = summary.expect("one failure summarises");
        assert_eq!(summary.headline, describe(Operation::Copy, &failure));
        assert!(summary.details.is_empty());
    }

    #[test]
    fn several_failures_aggregate_into_a_count_and_a_detail_per_item() {
        let failures: Vec<Failure> = ["a", "b", "c"]
            .into_iter()
            .map(|n| Failure::new(n, permission_denied()).to("Docs"))
            .collect();
        let summary = summarize(Operation::Copy, &failures, 3).expect("summarises");
        assert_eq!(summary.headline, "3 items couldn't be copied");
        assert_eq!(summary.details.len(), 3);
        assert_eq!(
            summary.details[1],
            "Couldn't copy “b” to “Docs”: permission denied."
        );
    }

    #[test]
    fn every_operation_has_a_group_headline() {
        for op in [
            Operation::Copy,
            Operation::Move,
            Operation::Rename,
            Operation::Trash,
            Operation::Delete,
            Operation::Create,
            Operation::Open,
            Operation::OpenWith,
            Operation::OpenTerminal,
            Operation::RunAction,
            Operation::Undo,
            Operation::Restore,
            Operation::EmptyTrash,
            Operation::Clipboard,
        ] {
            let headline = group_headline(op, 2);
            assert!(!headline.starts_with("failure-"), "{op:?}: {headline}");
            assert!(headline.contains('2'), "{op:?}: {headline}");
        }
    }
}
