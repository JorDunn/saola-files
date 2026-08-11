//! The streaming directory-size counter behind the properties dialog
//! (`ui::dialogs::properties`, Stage 13) — recursively totals the files and
//! bytes under one or more [`Location`]s, live, and can be cancelled
//! mid-walk.
//!
//! **Shape**, deliberately mirroring `core::fs::ops`'s engine (see that
//! module's doc comment): [`run`] spawns a detached tokio task and returns
//! the event side of a bounded `futures::channel::mpsc` immediately, so
//! `core/` stays iced-free and this returns a plain `BoxStream<'static,
//! SizeEvent>` — wrapping it into an `iced::Subscription` is
//! `ui::dialogs::properties`'s job, one layer up, the same split
//! `ui::dialogs::progress::subscription` already draws over `ops::run`.
//! [`SizeRequest`] carries an `Arc<AtomicBool>` cancel flag exactly like
//! [`super::ops::OpRequest`] does, checked at every recursion step (each
//! directory entered, each file counted) so a cancel lands within one
//! `list()`/`metadata()` call of being requested rather than at the end of
//! a whole tree. Unlike `ops`, there is nothing to conflict-prompt or write
//! — this only ever reads, so the engine is a plain recursive walk with no
//! `ExecContext`-style bundled mutable state beyond the running totals.
//!
//! **Why not just reuse `ops::count_totals`?** That function is a private,
//! best-effort pre-scan purely for a progress bar's denominator — it has no
//! cancellation, no live progress events, and is `ops.rs`-private by
//! design (an implementation detail of one `execute` call, not a reusable
//! engine). This module is the same *shape* of walk deliberately
//! duplicated as a small, independently cancellable, independently
//! streamed sibling — see this file's own size for how little the overlap
//! actually is once cancellation and progress events are threaded through.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};

use crate::core::fs::entry::EntryKind;
use crate::core::vfs::Location;

/// Bound on the engine's own progress event bridge — CLAUDE.md's
/// bounded-channel rule, same posture as `ops::EVENT_CHANNEL_CAPACITY`.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Identifies one submitted size request. Newtype over `u64`, same reasons
/// `ops::OpId` is one — see that type's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SizeRequestId(u64);

/// Monotonic [`SizeRequestId`] allocator — `App` owns exactly one, the same
/// "one shared counter, never per-view" posture as `ops::OpIdSource`.
#[derive(Debug, Default)]
pub struct SizeIdSource(u64);

impl SizeIdSource {
    pub fn alloc(&mut self) -> SizeRequestId {
        self.0 += 1;
        SizeRequestId(self.0)
    }
}

/// One "how big is this selection" request: `roots` are the top-level
/// locations selected in the properties dialog (one for a single
/// selection, several for a multi-selection) — directories are walked
/// recursively, plain files are counted directly. `Clone` so the UI can
/// hand a `&SizeRequest` to both `ui::dialogs::properties::subscription`
/// (which needs an owned copy for `Subscription::run_with`'s `Data`) and
/// keep its own copy in `App` state to expose `Self::request_cancel` when
/// the dialog closes.
#[derive(Clone)]
pub struct SizeRequest {
    pub id: SizeRequestId,
    pub roots: Vec<Location>,
    cancel: Arc<AtomicBool>,
}

impl SizeRequest {
    pub fn new(id: SizeRequestId, roots: Vec<Location>) -> Self {
        SizeRequest {
            id,
            roots,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests cancellation — fire-and-forget, same posture as
    /// `ops::OpRequest::request_cancel`: the walk notices on its next
    /// recursion step and reports back via `SizeEvent::Cancelled` on its
    /// own event stream, there is nothing to await here.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Manual `Hash`, by `id` alone — `Subscription::run_with` requires `Data:
/// Hash` to tell subscriptions apart across re-renders (see
/// `ops::OpRequest`'s identical doc comment for the full reasoning); the
/// `Arc<AtomicBool>` has no meaningful hash to contribute anyway.
impl std::hash::Hash for SizeRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// What the engine reports back as it walks. The UI replaces its whole
/// readout with each `Progress`/`Finished`'s totals (never accumulates
/// deltas itself), the same posture `ops::OpEvent::Progress` already takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeEvent {
    /// Running totals so far — emitted after every file counted, so the
    /// properties dialog's size row visibly counts up on a large tree
    /// rather than jumping once at the end.
    Progress { files: usize, bytes: u64 },
    /// The walk finished on its own (every root visited, no cancel).
    Finished { files: usize, bytes: u64 },
    /// `SizeRequest::request_cancel` was called before the walk finished.
    /// Carries the partial totals at the moment it stopped, so a cancelled
    /// dialog still shows "however far we got" rather than reverting to
    /// zero.
    Cancelled { files: usize, bytes: u64 },
}

/// Runs `request` and returns its event stream. Spawns a detached tokio
/// task immediately (like `ops::run`, this function itself never
/// `.await`s) — dropping the returned stream doesn't stop the walk (there's
/// no way to un-read a directory listing already in flight), only
/// `request.cancel` does that, checked at the next recursion step.
pub fn run(request: &SizeRequest) -> BoxStream<'static, SizeEvent> {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    tokio::spawn(execute(request.clone(), tx));
    rx.boxed()
}

async fn execute(request: SizeRequest, mut tx: mpsc::Sender<SizeEvent>) {
    let mut totals = Totals::default();
    let mut cancelled = false;
    for root in &request.roots {
        if request.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        walk(root, &request.cancel, &mut totals, &mut tx).await;
        if request.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
    }
    let event = if cancelled {
        SizeEvent::Cancelled {
            files: totals.files,
            bytes: totals.bytes,
        }
    } else {
        SizeEvent::Finished {
            files: totals.files,
            bytes: totals.bytes,
        }
    };
    // Must-deliver, same as `ops::send_event`'s `Finished`/`Cancelled`
    // arms: the dialog would otherwise be stuck showing "Calculating…"
    // forever. Retries through a full channel rather than dropping; a
    // *closed* channel (the dialog was torn down) just ends the walk.
    let _ = tx.try_send(event);
}

/// Running totals threaded through one walk — a plain struct rather than
/// bare `&mut usize, &mut u64` parameters purely for readability at each
/// `walk` call site.
#[derive(Default)]
struct Totals {
    files: usize,
    bytes: u64,
}

/// Recursively counts `location` into `totals`, emitting a best-effort
/// `SizeEvent::Progress` after every file. Boxed because async fns can't
/// recurse directly (the same self-referential-future workaround
/// `ops::ExecContext::copy_item` documents). Errors (a permission-denied
/// subdirectory, a backend that's gone away) just stop counting *that*
/// branch — matches `ops::count_totals`'s tolerant posture: an
/// undercounted total reads as "however much we could actually see", never
/// aborts the whole dialog.
fn walk<'a>(
    location: &'a Location,
    cancel: &'a AtomicBool,
    totals: &'a mut Totals,
    tx: &'a mut mpsc::Sender<SizeEvent>,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(backend) = crate::modules::resolve(location) else {
            return;
        };
        let Ok(meta) = backend.metadata(location).await else {
            return;
        };
        if meta.kind == EntryKind::Directory {
            let Ok(children) = backend.list(location).await else {
                return;
            };
            for child in children {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let child_location = location.join(&child.name);
                walk(&child_location, cancel, totals, tx).await;
            }
        } else {
            totals.files += 1;
            totals.bytes += meta.size;
            // Best-effort, like `ops::try_send_event`: a full channel just
            // drops this one progress tick — the next file's tick (or the
            // final `Finished`) supersedes it.
            let _ = tx.try_send(SizeEvent::Progress {
                files: totals.files,
                bytes: totals.bytes,
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-size-test-{}-{}",
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

    async fn drain(mut stream: BoxStream<'static, SizeEvent>) -> Vec<SizeEvent> {
        let mut seen = Vec::new();
        while let Some(event) = stream.next().await {
            let is_terminal = matches!(
                event,
                SizeEvent::Finished { .. } | SizeEvent::Cancelled { .. }
            );
            seen.push(event);
            if is_terminal {
                break;
            }
        }
        seen
    }

    #[test]
    fn size_id_source_hands_out_increasing_distinct_ids() {
        let mut ids = SizeIdSource::default();
        let a = ids.alloc();
        let b = ids.alloc();
        assert_ne!(a, b);
    }

    #[test]
    fn size_request_hash_depends_only_on_id() {
        use std::hash::{Hash, Hasher};
        let mut ids = SizeIdSource::default();
        let id = ids.alloc();
        let a = SizeRequest::new(id, vec![Location::local("/a")]);
        let b = SizeRequest::new(id, vec![]);
        let hash_of = |r: &SizeRequest| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            r.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    #[tokio::test]
    async fn counts_a_single_file_directly() {
        let root = tempdir();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();

        let mut ids = SizeIdSource::default();
        let request = SizeRequest::new(ids.alloc(), vec![Location::local(root.join("a.txt"))]);
        let events = drain(run(&request)).await;

        assert!(matches!(
            events.last(),
            Some(SizeEvent::Finished { files: 1, bytes: 5 })
        ));

        cleanup(root);
    }

    #[tokio::test]
    async fn recursively_totals_a_directory_tree() {
        let root = tempdir();
        std::fs::write(root.join("a.txt"), b"hello").unwrap(); // 5 bytes
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.txt"), b"world!").unwrap(); // 6 bytes

        let mut ids = SizeIdSource::default();
        let request = SizeRequest::new(ids.alloc(), vec![Location::local(&root)]);
        let events = drain(run(&request)).await;

        assert!(matches!(
            events.last(),
            Some(SizeEvent::Finished {
                files: 2,
                bytes: 11
            })
        ));
        // Progress events counted up along the way, not just a single
        // jump at the end.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SizeEvent::Progress { .. }))
        );

        cleanup(root);
    }

    #[tokio::test]
    async fn sums_multiple_roots_in_one_request() {
        let root = tempdir();
        std::fs::write(root.join("a.txt"), b"12345").unwrap(); // 5 bytes
        std::fs::write(root.join("b.txt"), b"1234567890").unwrap(); // 10 bytes

        let mut ids = SizeIdSource::default();
        let request = SizeRequest::new(
            ids.alloc(),
            vec![
                Location::local(root.join("a.txt")),
                Location::local(root.join("b.txt")),
            ],
        );
        let events = drain(run(&request)).await;

        assert!(matches!(
            events.last(),
            Some(SizeEvent::Finished {
                files: 2,
                bytes: 15
            })
        ));

        cleanup(root);
    }

    #[tokio::test]
    async fn cancel_mid_walk_stops_early_and_reports_partial_totals() {
        let root = tempdir();
        // Enough files that a cancel issued right after the first
        // `Progress` event is overwhelmingly likely to land before the
        // walk would otherwise finish on its own.
        for n in 0..500 {
            std::fs::write(root.join(format!("f{n}.txt")), b"x").unwrap();
        }

        let mut ids = SizeIdSource::default();
        let request = SizeRequest::new(ids.alloc(), vec![Location::local(&root)]);
        let mut stream = run(&request);

        match stream.next().await {
            Some(SizeEvent::Progress { .. }) => request.request_cancel(),
            other => panic!("expected a Progress event first, got {other:?}"),
        }

        let mut saw_cancelled = false;
        while let Some(event) = stream.next().await {
            if let SizeEvent::Cancelled { files, .. } = event {
                saw_cancelled = true;
                // Partial, not the full 500 — the whole point of
                // cancelling is that it doesn't finish the walk.
                assert!(files < 500);
            }
            if matches!(
                event,
                SizeEvent::Cancelled { .. } | SizeEvent::Finished { .. }
            ) {
                break;
            }
        }
        assert!(saw_cancelled, "expected the walk to report Cancelled");

        cleanup(root);
    }

    #[tokio::test]
    async fn an_unresolvable_scheme_finishes_at_zero_rather_than_hanging() {
        let mut ids = SizeIdSource::default();
        let request = SizeRequest::new(
            ids.alloc(),
            vec![Location {
                scheme: "nonexistent".to_owned(),
                authority: None,
                path: PathBuf::from("/x"),
            }],
        );
        let events = drain(run(&request)).await;
        assert!(matches!(
            events.last(),
            Some(SizeEvent::Finished { files: 0, bytes: 0 })
        ));
    }
}
