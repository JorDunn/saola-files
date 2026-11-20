//! Opening files that have no local path: download a read-only copy, then
//! open the copy.
//!
//! A backend without `Caps::LOCAL_PATH` (SFTP today) cannot hand an
//! application a path, so Enter / double-click, Open with…, and a custom
//! `[[action]]` with `%f`/`%F` all take this detour (AGENTS.md:
//! capability-honest UI). Local files never come here: [`App::divert_remote`]
//! hands them straight back to the caller, so they open in place exactly as
//! before.
//!
//! # The flow (one `OpenRequest` carries it end to end)
//!
//! 1. [`App::divert_remote`] splits the locations by
//!    `core::open_copy::open_plan`. Local ones go back to the caller. Unsafe
//!    names and busy states are reported at once. The rest start step 2.
//! 2. `prepare` (async): per source, check it is a file (a folder is never
//!    downloaded: the engine would walk it recursively), then create the
//!    private per-open slot directory (`core::open_copy::create_slot`).
//!    Nothing is downloaded when anything here fails.
//! 3. [`App::remote_open_prepared`] submits an ordinary `OpKind::Copy`
//!    through the ops engine into the slot, so the footer strip shows
//!    "Downloading ..." with the existing Cancel button, and a large file is
//!    never fetched silently.
//! 4. [`App::remote_open_event`] intercepts that op's `Started`, `Finished`
//!    and `Cancelled` events (everything else flows through the generic
//!    progress handling). Any failure or cancel: nothing is launched, the
//!    slot (with any partial file) is deleted, and the notice says so.
//! 5. On success `core::open_copy::seal` makes the copy read-only and reads
//!    each head; [`App::remote_open_sealed`] launches the app (the head
//!    sniffs an untyped name, since the bytes are local now) and shows the
//!    caveat in the notice band. If the launch itself fails, the slot is
//!    deleted.
//!
//! Failures use one `Incident` minted when the action starts, so a 3-file
//! open that fails shows one notice ("3 items couldn't be opened").
//!
//! Only one download runs at a time: the ops engine has one active op, and
//! a second open while it is busy is refused with a worded notice rather
//! than queued.

use std::ffi::OsString;
use std::path::PathBuf;

use iced::Task;

use core::apps::DesktopEntry;
use core::failure::{Cause, Failure, Incident, Operation};
use core::fs::entry::EntryKind;
use core::fs::ops;
use core::open_copy::{self, OpenPlan, SealedCopy};
use core::vfs::{Backend, Caps, Location, VfsError};
use saola_files::{core, modules, tr, ui};

use super::{App, Message};

/// What to do with the copy once it is local.
#[derive(Debug, Clone)]
pub(crate) enum Launch {
    /// The default app for its mimetype (Enter, double-click).
    Default,
    /// A specific app, by desktop id (Open with…).
    With(String),
    /// A custom `[[action]]`'s command line.
    Custom(String),
}

impl Launch {
    /// The notice sentence's verb for a failure of this launch.
    fn operation(&self) -> Operation {
        match self {
            Launch::Default => Operation::Open,
            Launch::With(_) => Operation::OpenWith,
            Launch::Custom(_) => Operation::RunAction,
        }
    }
}

/// One open action, from the click to the launch. `Clone` because it rides
/// inside `Message`s.
#[derive(Debug, Clone)]
pub(crate) struct OpenRequest {
    incident: Incident,
    launch: Launch,
    /// The remote files, in order; the sealed copies come back in the same
    /// order.
    sources: Vec<Location>,
}

/// The one download in flight, kept on `App` so the op's events can be
/// recognised as ours.
pub(crate) struct PendingOpen {
    op: ops::OpId,
    request: OpenRequest,
    slot: PathBuf,
}

/// This feature's slice of `app::Message`.
#[derive(Debug, Clone)]
pub(crate) enum OpenMessage {
    /// `prepare` finished: the slot to download into, or why not.
    Prepared(OpenRequest, Result<PathBuf, Vec<Failure>>),
    /// The copies were sealed read-only (or that failed).
    Sealed(OpenRequest, PathBuf, Result<Vec<SealedCopy>, String>),
}

impl App {
    /// Splits `locations` into ones to open in place (returned for the
    /// caller's existing code) and ones that need a downloaded copy
    /// (started here; the returned `Task` drives them).
    pub(super) fn divert_remote(
        &mut self,
        locations: Vec<Location>,
        launch: Launch,
    ) -> (Vec<Location>, Task<Message>) {
        let split = partition(locations, caps_of);
        if split.download.is_empty() && split.refused.is_empty() {
            return (split.direct, Task::none());
        }
        let incident = self.new_incident();
        let operation = launch.operation();

        let mut failures: Vec<Failure> = split
            .refused
            .iter()
            .map(|location| Failure::at(location, Cause::from(tr!("open-unsafe-name"))))
            .collect();

        // A default-app open whose name already says the type, and nothing
        // is registered for it, would only fail after the download: say so
        // now. Untyped names wait for the sniff of the downloaded head.
        let mut sources = Vec::new();
        for location in split.download {
            if matches!(launch, Launch::Default)
                && let Some(name) = location.path.file_name()
            {
                let by_name = self.mime_db.guess(name, None);
                if !core::mime::needs_sniff(&by_name)
                    && self.apps_db.default_for(&by_name).is_none()
                {
                    eprintln!("saola-files: no app is associated with {by_name} for {location}");
                    failures.push(Failure::at(&location, Cause::NoDefaultApp));
                    continue;
                }
            }
            sources.push(location);
        }

        let busy = self.active_op.is_some() || self.pending_open.is_some();
        if busy && !sources.is_empty() {
            failures.extend(
                sources
                    .iter()
                    .map(|location| Failure::at(location, Cause::from(tr!("open-busy")))),
            );
            sources.clear();
        }
        self.report_failures(incident, operation, failures);
        if sources.is_empty() {
            return (split.direct, Task::none());
        }

        let request = OpenRequest {
            incident,
            launch,
            sources: sources.clone(),
        };
        let root = open_copy::open_root_from_env();
        // `Task::perform(future, to_message)`: runs `future` on iced's
        // runtime and turns its output into a `Message` for `update`.
        let task = Task::perform(prepare(sources, root, modules::resolve), move |result| {
            Message::RemoteOpen(OpenMessage::Prepared(request.clone(), result))
        });
        (split.direct, task)
    }

    pub(super) fn handle_remote_open(&mut self, message: OpenMessage) -> Task<Message> {
        match message {
            OpenMessage::Prepared(request, result) => self.remote_open_prepared(request, result),
            OpenMessage::Sealed(request, slot, result) => {
                self.remote_open_sealed(request, slot, result)
            }
        }
    }

    /// Step 3: submit the download as an ordinary copy into the slot.
    fn remote_open_prepared(
        &mut self,
        request: OpenRequest,
        result: Result<PathBuf, Vec<Failure>>,
    ) -> Task<Message> {
        let slot = match result {
            Ok(slot) => slot,
            Err(failures) => {
                self.report_failures(request.incident, request.launch.operation(), failures);
                return Task::none();
            }
        };
        // Another op may have started while `prepare` ran.
        if self.active_op.is_some() || self.pending_open.is_some() {
            let failures = request
                .sources
                .iter()
                .map(|location| Failure::at(location, Cause::from(tr!("open-busy"))))
                .collect();
            self.report_failures(request.incident, request.launch.operation(), failures);
            return remove_slot_task(slot);
        }
        let id = self.op_ids.alloc();
        self.active_op_had_conflict = false;
        self.active_op = Some(ops::OpRequest::new(
            id,
            ops::OpKind::Copy,
            request.sources.clone(),
            Location::local(slot.clone()),
        ));
        self.pending_open = Some(PendingOpen {
            op: id,
            request,
            slot,
        });
        Task::none()
    }

    /// Step 4: recognise our download's events. `None` means "not ours, or
    /// not an event we care about": the generic handler takes it.
    pub(super) fn remote_open_event(&mut self, event: &ops::OpEvent) -> Option<Task<Message>> {
        let pending = self.pending_open.as_ref()?;
        if self.active_op.as_ref().map(|request| request.id) != Some(pending.op) {
            return None;
        }
        match event {
            ops::OpEvent::Started {
                files_total,
                bytes_total,
            } => {
                let mut progress = ui::dialogs::progress::Progress::started(
                    ops::OpKind::Copy,
                    *files_total,
                    *bytes_total,
                );
                progress.downloading = true;
                self.active_op_progress = Some(progress);
                Some(Task::none())
            }
            ops::OpEvent::Finished { errors } => {
                let pending = self.pending_open.take()?;
                self.clear_active_op();
                if !errors.is_empty() {
                    for (location, err) in errors {
                        eprintln!("saola-files: downloading {location}: {err}");
                    }
                    let failures = errors
                        .iter()
                        .map(|(location, err)| Failure::at(location, err.clone()))
                        .collect();
                    return Some(self.abandon(pending, failures));
                }
                let names: Vec<OsString> = pending
                    .request
                    .sources
                    .iter()
                    .filter_map(|location| location.path.file_name().map(OsString::from))
                    .collect();
                let (slot, request) = (pending.slot, pending.request);
                let sealed_slot = slot.clone();
                Some(Task::perform(
                    blocking(move || open_copy::seal(&sealed_slot, &names)),
                    move |result| {
                        Message::RemoteOpen(OpenMessage::Sealed(
                            request.clone(),
                            slot.clone(),
                            result,
                        ))
                    },
                ))
            }
            ops::OpEvent::Cancelled => {
                let pending = self.pending_open.take()?;
                self.clear_active_op();
                let failures = pending
                    .request
                    .sources
                    .iter()
                    .map(|location| Failure::at(location, Cause::from(tr!("open-cancelled"))))
                    .collect();
                Some(self.abandon(pending, failures))
            }
            _ => None,
        }
    }

    /// Drops the engine's per-op state once the download has ended.
    fn clear_active_op(&mut self) {
        self.active_op = None;
        self.active_op_progress = None;
        self.pending_conflict = None;
    }

    /// Reports `failures` of a download and deletes its slot (and with it
    /// any partial file). Nothing is launched.
    fn abandon(&mut self, pending: PendingOpen, failures: Vec<Failure>) -> Task<Message> {
        self.report_failures(
            pending.request.incident,
            pending.request.launch.operation(),
            failures,
        );
        remove_slot_task(pending.slot)
    }

    /// Step 5: launch the app on the sealed copies, then show the caveat.
    fn remote_open_sealed(
        &mut self,
        request: OpenRequest,
        slot: PathBuf,
        result: Result<Vec<SealedCopy>, String>,
    ) -> Task<Message> {
        let operation = request.launch.operation();
        let copies = match result {
            Ok(copies) if copies.len() == request.sources.len() => copies,
            other => {
                let message = match other {
                    Ok(_) => tr!("failure-reason-unknown"),
                    Err(message) => message,
                };
                let failures = request
                    .sources
                    .iter()
                    .map(|location| Failure::at(location, Cause::from(message.clone())))
                    .collect();
                self.report_failures(request.incident, operation, failures);
                return remove_slot_task(slot);
            }
        };

        let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
        let mut failures = Vec::new();
        let mut launched = 0usize;
        match &request.launch {
            Launch::Default => {
                for (source, copy) in request.sources.iter().zip(&copies) {
                    let mimetype = self.copy_mimetype(copy);
                    let Some(entry) = self.apps_db.default_for(&mimetype) else {
                        eprintln!("saola-files: no app is associated with {mimetype} for {source}");
                        failures.push(Failure::at(source, Cause::NoDefaultApp));
                        continue;
                    };
                    match core::apps::open(entry, std::slice::from_ref(&copy.path), &terminal) {
                        Ok(()) => launched += 1,
                        Err(err) => {
                            eprintln!("saola-files: could not open {source}: {err}");
                            failures.push(Failure::at(source, Cause::launch(&err)));
                        }
                    }
                }
            }
            Launch::With(desktop_id) => {
                let paths: Vec<PathBuf> = copies.iter().map(|copy| copy.path.clone()).collect();
                match self.apps_db.entry(desktop_id).cloned() {
                    None => failures.extend(request.sources.iter().map(|source| {
                        Failure::at(source, Cause::AppUninstalled).to(desktop_id.as_str())
                    })),
                    Some(entry) => match core::apps::open(&entry, &paths, &terminal) {
                        Ok(()) => launched = paths.len(),
                        Err(err) => failures.extend(request.sources.iter().map(|source| {
                            Failure::at(source, Cause::launch(&err)).to(entry_name(&entry))
                        })),
                    },
                }
            }
            Launch::Custom(exec) => {
                let paths: Vec<PathBuf> = copies.iter().map(|copy| copy.path.clone()).collect();
                for argv in core::apps::build_argv(exec, &paths) {
                    let Some((program, args)) = argv.split_first() else {
                        continue;
                    };
                    match core::apps::spawn_argv(program, args) {
                        Ok(()) => launched += 1,
                        Err(err) => {
                            eprintln!("saola-files: custom action \"{exec}\" failed: {err}");
                            failures.push(Failure::new(program.clone(), Cause::launch(&err)));
                        }
                    }
                }
            }
        }

        let nothing_launched = launched == 0;
        if failures.is_empty() {
            // The caveat: one entry per copy, so several read "Opened
            // read-only copies of 3 items". It stays until dismissed or a
            // newer action replaces it, like any notice.
            let caveats = request
                .sources
                .iter()
                .map(|source| Failure::at(source, Cause::from(String::new())))
                .collect();
            self.report_failures(request.incident, Operation::OpenedCopy, caveats);
        } else {
            self.report_failures(request.incident, operation, failures);
        }
        if nothing_launched {
            // No application holds the copy: nothing to protect.
            remove_slot_task(slot)
        } else {
            Task::none()
        }
    }

    /// The mimetype to open a downloaded copy with: the name's own type
    /// when it has one, else a sniff of the copy's head.
    fn copy_mimetype(&self, copy: &SealedCopy) -> String {
        let name = copy.path.file_name().unwrap_or_default();
        let by_name = self.mime_db.guess(name, None);
        if core::mime::needs_sniff(&by_name) {
            self.mime_db.guess(name, copy.head.as_deref())
        } else {
            by_name
        }
    }
}

fn entry_name(entry: &DesktopEntry) -> &str {
    entry.name.as_str()
}

/// The startup task: remove slots left by earlier runs that are older than
/// `open_copy::STALE_AFTER`.
pub(super) fn startup_sweep() -> Task<Message> {
    Task::perform(
        blocking(|| {
            let root = open_copy::open_root_from_env();
            Ok(open_copy::sweep_stale(
                &root,
                std::time::SystemTime::now(),
                open_copy::STALE_AFTER,
            ))
        }),
        |_| Message::Noop,
    )
}

fn remove_slot_task(slot: PathBuf) -> Task<Message> {
    Task::perform(
        blocking(move || {
            open_copy::remove_slot(&slot);
            Ok(())
        }),
        |_| Message::Noop,
    )
}

/// Runs blocking `std::fs` work off the UI thread and flattens the two ways
/// it can fail (the I/O itself, or the worker panicking) into a string.
///
/// Teaching note: `spawn_blocking` moves the closure onto tokio's pool of
/// threads meant for blocking calls, so the UI thread never waits on the
/// disk. It returns a `JoinHandle`; awaiting that gives
/// `Result<closure's result, JoinError>`.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, String> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err.to_string()),
        Err(join_err) => Err(join_err.to_string()),
    }
}

/// The capabilities of `location`'s backend. A location with no backend
/// (a disconnected server) is treated as having none, so it is routed
/// through `prepare`, which then words "no backend", rather than being
/// handed to an app as a path that means nothing locally.
fn caps_of(location: &Location) -> Caps {
    modules::resolve(location)
        .map(|backend| backend.caps())
        .unwrap_or_else(Caps::empty)
}

/// The result of [`partition`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Partition {
    /// Open in place, as before.
    direct: Vec<Location>,
    /// Download a copy first.
    download: Vec<Location>,
    /// Names that must not become local files.
    refused: Vec<Location>,
}

/// Sorts `locations` by `open_plan`, keeping order within each group. Pure:
/// the capability lookup is injected.
fn partition(locations: Vec<Location>, caps_of: impl Fn(&Location) -> Caps) -> Partition {
    let mut split = Partition::default();
    for location in locations {
        match open_copy::open_plan(caps_of(&location), &location) {
            OpenPlan::Direct(_) => split.direct.push(location),
            OpenPlan::DownloadFirst { .. } => split.download.push(location),
            OpenPlan::Refused(_) => split.refused.push(location),
        }
    }
    split
}

/// Step 2: checks every source is a downloadable file, then creates the
/// slot under `root`. `backend_for` is injected so tests use a fake backend.
async fn prepare<F>(
    sources: Vec<Location>,
    root: PathBuf,
    backend_for: F,
) -> Result<PathBuf, Vec<Failure>>
where
    F: Fn(&Location) -> Option<Box<dyn Backend>>,
{
    let mut failures = Vec::new();
    for source in &sources {
        let Some(backend) = backend_for(source) else {
            let err = VfsError::Other {
                message: tr!("error-no-backend", scheme = source.scheme.as_str()),
            };
            failures.push(Failure::at(source, err));
            continue;
        };
        match backend.metadata(source).await {
            // A folder is never downloaded: the copy engine would walk it
            // recursively. (A symlink is classified as a symlink, never
            // followed into a folder, so it cannot trigger a walk either.)
            Ok(entry) if entry.kind == EntryKind::Directory => {
                let err = VfsError::IsADirectory {
                    location: source.to_string(),
                };
                failures.push(Failure::at(source, err));
            }
            Ok(_) => {}
            Err(err) => failures.push(Failure::at(source, err)),
        }
    }
    if !failures.is_empty() {
        return Err(failures);
    }
    match blocking(move || open_copy::create_slot(&root)).await {
        Ok(slot) => Ok(slot),
        Err(message) => Err(sources
            .iter()
            .map(|source| {
                Failure::at(
                    source,
                    VfsError::Other {
                        message: message.clone(),
                    },
                )
            })
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saola_files::core::fs::entry::FileEntry;
    use saola_files::core::vfs::FakeBackend;

    fn remote(path: &str) -> Location {
        Location {
            scheme: "fake".to_owned(),
            authority: Some("host".to_owned()),
            path: PathBuf::from(path),
        }
    }

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind,
            size: 3,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    fn fake() -> FakeBackend {
        FakeBackend::new().with_dir(
            "/srv",
            vec![
                entry("note.txt", EntryKind::File),
                entry("album", EntryKind::Directory),
            ],
        )
    }

    fn scratch_root(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("saola-remote-open-{tag}-{}", std::process::id()))
            .join("saola-files")
            .join("open")
    }

    #[test]
    fn local_locations_stay_direct_and_remote_ones_download() {
        let local = Location::local("/home/me/a.txt");
        let remote_file = remote("/srv/note.txt");
        let split = partition(vec![local.clone(), remote_file.clone()], |l| {
            if l.is_local() {
                Caps::LOCAL_PATH
            } else {
                Caps::empty()
            }
        });
        assert_eq!(split.direct, vec![local]);
        assert_eq!(split.download, vec![remote_file]);
        assert!(split.refused.is_empty());
    }

    #[test]
    fn a_fake_backend_without_local_path_is_a_download() {
        let backend = fake();
        let split = partition(vec![remote("/srv/note.txt")], |_| backend.caps());
        assert_eq!(split.download.len(), 1);
        assert!(split.direct.is_empty());
    }

    #[test]
    fn a_rootless_location_is_refused_not_downloaded() {
        let split = partition(vec![remote("/")], |_| Caps::empty());
        assert_eq!(split.refused.len(), 1);
        assert!(split.download.is_empty() && split.direct.is_empty());
    }

    #[test]
    fn a_file_is_prepared_into_a_private_slot() {
        let root = scratch_root("ok");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let slot = runtime
            .block_on(prepare(vec![remote("/srv/note.txt")], root.clone(), |_| {
                Some(Box::new(fake()))
            }))
            .expect("a slot");
        assert!(slot.starts_with(&root) && slot.is_dir());
        let _ = std::fs::remove_dir_all(root.parent().and_then(|p| p.parent()).unwrap_or(&root));
    }

    #[test]
    fn a_folder_is_never_downloaded_and_nothing_is_created() {
        let root = scratch_root("dir");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let failures = runtime
            .block_on(prepare(vec![remote("/srv/album")], root.clone(), |_| {
                Some(Box::new(fake()))
            }))
            .expect_err("a folder must be refused");
        assert_eq!(failures.len(), 1);
        assert!(matches!(
            failures[0].cause,
            Cause::Vfs(VfsError::IsADirectory { .. })
        ));
        assert!(!root.exists(), "no slot for a refused open");
    }

    #[test]
    fn a_missing_file_and_a_missing_backend_are_both_reported() {
        let root = scratch_root("missing");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let missing = runtime
            .block_on(prepare(vec![remote("/srv/gone.txt")], root.clone(), |_| {
                Some(Box::new(fake()))
            }))
            .expect_err("missing file");
        assert!(matches!(
            missing[0].cause,
            Cause::Vfs(VfsError::NotFound { .. })
        ));
        let no_backend = runtime
            .block_on(prepare(vec![remote("/srv/note.txt")], root.clone(), |_| {
                None
            }))
            .expect_err("no backend");
        assert_eq!(no_backend.len(), 1);
        assert!(!root.exists());
    }

    #[test]
    fn every_failing_source_is_reported_under_one_call() {
        let root = scratch_root("several");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let failures = runtime
            .block_on(prepare(
                vec![
                    remote("/srv/album"),
                    remote("/srv/gone.txt"),
                    remote("/srv/note.txt"),
                ],
                root.clone(),
                |_| Some(Box::new(fake())),
            ))
            .expect_err("two of three fail");
        assert_eq!(failures.len(), 2);
    }

    #[test]
    fn each_launch_words_its_own_operation() {
        assert_eq!(Launch::Default.operation(), Operation::Open);
        assert_eq!(
            Launch::With("x.desktop".into()).operation(),
            Operation::OpenWith
        );
        assert_eq!(
            Launch::Custom("x %f".into()).operation(),
            Operation::RunAction
        );
    }

    #[test]
    fn the_caveat_uses_the_wording_the_issue_asks_for() {
        let failure = Failure::new("x", Cause::from(String::new()));
        assert_eq!(
            core::failure::describe(Operation::OpenedCopy, &failure),
            "Opened a read-only copy of “x” — changes won't be saved to the server."
        );
        let summary =
            core::failure::summarize(Operation::OpenedCopy, &[failure.clone(), failure], 2)
                .expect("summary");
        assert_eq!(
            summary.headline,
            "Opened read-only copies of 2 items — changes won't be saved to the server."
        );
    }
}
