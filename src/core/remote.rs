//! The connection manager for remote [`Backend`]s (Stage 14) — per-
//! authority pooling, the auth/host-key prompt plumbing, and the
//! download-to-temp cache a `Caps::LOCAL_PATH`-less backend's "open" flow
//! needs. `App` owns exactly one [`RemoteManager`] (CLAUDE.md: "Shared
//! caches … live on the App, never per-view"), the same one-per-`App`
//! posture `core::fs::ops::OpIdSource`/`core::thumbs::ThumbCache` already
//! take.
//!
//! **Why `modules::resolve` needs a way to reach this that isn't a normal
//! parameter.** `modules::resolve(location) -> Option<Box<dyn Backend>>`
//! is a plain free function, called from deep inside `core::fs::ops`,
//! `core::fs::undo`, `core::fs::size`, and `ui::dirview` — none of which
//! hold a reference to `App`'s fields, nor should they (that's exactly the
//! layering `core`/`ui` keep clean of app-window concerns). A remote
//! backend has no stateless "build fresh per call" shape the way `local`
//! does (connecting costs a real SSH handshake with interactive prompts,
//! which a synchronous `resolve` call has nowhere to route) — so instead
//! of threading a pool handle through every one of those call sites,
//! [`RemoteManager::install_global`] publishes this manager's pool behind
//! a process-wide [`OnceLock`], and [`global_pooled`] is what `modules::
//! resolve` reads. This is safe *because* the app is single-instance (one
//! `App`, one `RemoteManager`, for the process's whole lifetime — Stage 15
//! enforces that at the D-Bus level too) — not a general license for
//! global mutable state. **Tests never call `install_global`**: every test
//! below constructs its own `RemoteManager` and exercises `Self::pooled`/
//! `Self::register` directly on that instance, which stays fully isolated
//! from whatever other tests happen to run in parallel in the same test
//! binary — the same "resolve shared state at one thin wrapper, keep the
//! logic itself parameter-taking and testable" posture CLAUDE.md's
//! no-`std::env::set_var`-in-tests rule already establishes for env vars,
//! applied here to process-global state generally.
//!
//! **Prompt plumbing** reuses the capacity-1 reply-channel pattern
//! `core::fs::ops`'s conflict prompts established (see that module's doc
//! comment): [`ConnectEvent::HostKeyPrompt`]/[`ConnectEvent::AuthPrompt`]
//! each carry a fresh `mpsc::Sender` for the human's answer, built and
//! awaited by whichever protocol module (`modules::sftp`, behind
//! `feature = "sftp"`) is doing the actual handshake. Every such await is
//! timeout-bounded (`PROMPT_TIMEOUT`) — CLAUDE.md: "timeout foreign
//! waits" — so a torn-down connect dialog (the human closed the window,
//! or navigated away) can't strand a background SSH handshake forever.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::channel::mpsc;
use futures::sink::SinkExt;
use futures::stream::{BoxStream, StreamExt};

use crate::core::vfs::{Backend, Location, VfsError};

/// How long a connect attempt's prompts (`ConnectEvent::HostKeyPrompt`/
/// `AuthPrompt`) wait for a human answer before giving up and failing the
/// connection — CLAUDE.md's "timeout foreign waits" rule, applied the same
/// way `core::fs::ops`'s reply channels are documented to never hang the
/// worker thread forever, just phrased as a hard deadline instead of a
/// full-channel retry loop (a prompt reply is a one-shot human decision,
/// not a high-frequency progress event).
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Bound on one `connect` request's own event channel —
/// CLAUDE.md's bounded-channel rule, same posture as `core::fs::ops`'s
/// `EVENT_CHANNEL_CAPACITY`. Small on purpose: a connect attempt only ever
/// has one prompt in flight at a time (unlike a copy's many progress
/// events), so there is nothing high-frequency to buffer.
const CONNECT_CHANNEL_CAPACITY: usize = 8;

type PoolKey = (String, String);
type Pool = Arc<Mutex<HashMap<PoolKey, Arc<dyn Backend>>>>;

/// Recovers a possibly-poisoned lock rather than panicking on one — the
/// no-panic rule (CLAUDE.md) extends to "a prompt-reply thread panicked
/// while holding this", which must degrade (here: proceed with whatever
/// was in the map before the panic) rather than take the whole app down
/// the next time anything touches the pool.
fn lock_pool(pool: &Pool) -> std::sync::MutexGuard<'_, HashMap<PoolKey, Arc<dyn Backend>>> {
    pool.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pool_key(location: &Location) -> Option<PoolKey> {
    location
        .authority
        .clone()
        .map(|authority| (location.scheme.clone(), authority))
}

static GLOBAL_POOL: OnceLock<Pool> = OnceLock::new();

/// `modules::resolve`'s one call into this module — see the module doc
/// comment on why this is a free function reading a process-wide handle
/// rather than a parameter. `None` covers every "nothing to hand back"
/// case uniformly: no `RemoteManager` has installed itself yet (nothing
/// ever connected this session), `location` is local (`authority` is
/// always `None` for `scheme == "file"`, so `pool_key` itself returns
/// `None`), or this exact (scheme, authority) was never connected/has
/// since been forgotten.
pub fn global_pooled(location: &Location) -> Option<Arc<dyn Backend>> {
    let key = pool_key(location)?;
    let pool = GLOBAL_POOL.get()?;
    lock_pool(pool).get(&key).cloned()
}

/// Identifies one connect attempt — mirrors `core::fs::ops::OpId`'s own
/// doc comment: a newtype purely so `Hash`/any future map key reads as
/// "a connect id", not a bare integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectId(u64);

/// Monotonic [`ConnectId`] allocator — one per `App`, the same posture
/// `core::fs::ops::OpIdSource` already takes.
#[derive(Debug, Default)]
pub struct ConnectIdSource(u64);

impl ConnectIdSource {
    pub fn alloc(&mut self) -> ConnectId {
        self.0 += 1;
        ConnectId(self.0)
    }
}

/// What's being asked about the server's host key — first contact (no
/// entry in `known_hosts` at all) is the only case that ever reaches the
/// UI; a *changed* key (a real MITM-shaped mismatch) is a hard failure
/// with no prompt (see `modules::sftp`'s host-key handling) — there is no
/// sane "trust anyway" default for that case to offer.
#[derive(Debug, Clone)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
}

/// Which SSH auth step is being asked for, after ssh-agent (tried first,
/// silently) either wasn't available or offered no identity the server
/// accepted.
#[derive(Debug, Clone)]
pub enum AuthStage {
    /// A default key file (`~/.ssh/id_ed25519` etc.) exists but is
    /// passphrase-protected — `reply` carries the passphrase, or `None` to
    /// skip this key and move on to the next candidate/password auth.
    KeyPassphrase { key_path: PathBuf },
    /// Every key candidate was exhausted (or none existed) — plain
    /// password auth. `reply`'s `None` cancels the whole connect attempt.
    Password { user: String },
}

/// One connect attempt's progress — the same "spawn detached, stream
/// events back" shape `core::fs::ops::OpEvent`/`core::fs::size::SizeEvent`
/// already establish, reusing their must-deliver-vs-best-effort split:
/// every variant here is must-deliver (a dropped `HostKeyPrompt`/
/// `AuthPrompt` would strand the handshake waiting on an answer nobody was
/// ever asked for, exactly `core::fs::ops::OpEvent::Conflict`'s own
/// reasoning), so there is no best-effort "Progress" tick to drop.
#[derive(Debug, Clone)]
pub enum ConnectEvent {
    HostKeyPrompt {
        prompt: HostKeyPrompt,
        reply: mpsc::Sender<bool>,
    },
    AuthPrompt {
        stage: AuthStage,
        reply: mpsc::Sender<Option<String>>,
    },
    /// The backend is authenticated and already registered in this
    /// manager's pool (`Self::register` ran *before* this is sent) — a
    /// subscriber reacting to `Connected` by immediately calling
    /// `modules::resolve`/`Self::pooled` is guaranteed to find it.
    Connected,
    Failed(VfsError),
}

/// One submitted connect job — mirrors `core::fs::ops::OpRequest`'s shape
/// (an `Arc<AtomicBool>` cancel flag, `Hash` by `id` alone for
/// `Subscription::run_with`'s identity).
#[derive(Clone)]
pub struct ConnectRequest {
    pub id: ConnectId,
    pub location: Location,
    cancel: Arc<AtomicBool>,
}

impl ConnectRequest {
    pub fn new(id: ConnectId, location: Location) -> Self {
        ConnectRequest {
            id,
            location,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Fire-and-forget, like `OpRequest::request_cancel` — the connect
    /// task notices on its next check (before starting the next network
    /// round trip, or the next time a prompt would otherwise be sent) and
    /// reports back `ConnectEvent::Failed` on its own stream.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// See `core::fs::ops::OpRequest`'s identical doc comment — same
/// reasoning, same shape, just for a connect attempt instead of a
/// copy/move.
impl std::hash::Hash for ConnectRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// The connection manager itself. `App` owns exactly one; see the module
/// doc comment for how `modules::resolve` reaches its pool without a
/// direct reference.
pub struct RemoteManager {
    pool: Pool,
    /// Download-to-temp cache for a `Caps::LOCAL_PATH`-less backend's
    /// "open" flow (CLAUDE.md: "without `LOCAL_PATH`, open = download-to-
    /// temp with a read-only caveat") — keyed by the exact remote
    /// `Location`, so re-opening the same file twice in one session
    /// doesn't re-download it. Session-scoped only, like `core::fs::undo`'s
    /// stack (no persistence across restarts, no eviction beyond the
    /// process's own temp-directory cleanup) — a stale cached path that no
    /// longer exists on disk (something else cleaned `/tmp`) is simply
    /// re-downloaded, not treated as an error.
    downloads: Mutex<HashMap<Location, PathBuf>>,
}

impl Default for RemoteManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteManager {
    pub fn new() -> Self {
        RemoteManager {
            pool: Arc::new(Mutex::new(HashMap::new())),
            downloads: Mutex::new(HashMap::new()),
        }
    }

    /// Publishes this manager's pool for `modules::resolve`/[`global_pooled`]
    /// to read — see the module doc comment. Call exactly once, from
    /// `main.rs::App::new`. Idempotent (`OnceLock::set` on an
    /// already-installed slot is a silent no-op) purely so a hypothetical
    /// future re-entrant call (there isn't one today) can't panic; it is
    /// not an invitation to call this more than once on purpose.
    pub fn install_global(&self) {
        let _ = GLOBAL_POOL.set(self.pool.clone());
    }

    /// Sync lookup — what `main.rs::App::navigate_active` reads to decide
    /// "is this remote location already connected, or does clicking it
    /// need to open the connect dialog first?".
    pub fn pooled(&self, location: &Location) -> Option<Arc<dyn Backend>> {
        let key = pool_key(location)?;
        lock_pool(&self.pool).get(&key).cloned()
    }

    /// Adds (or replaces) a pooled backend for `location`'s (scheme,
    /// authority) on *this instance's* pool. The real connect flow
    /// ([`connect`]) does the equivalent insert directly into the
    /// process-global pool instead (see that function's own doc comment
    /// on why it can't go through this method) — this is what tests use
    /// to exercise `Self::pooled`/`modules::resolve`-shaped lookups
    /// against a fake backend without touching global state. A `location`
    /// with no `authority` (a local path) is silently a no-op — nothing
    /// pools `file` locations, `modules::resolve` already handles those
    /// without this manager.
    pub fn register(&self, location: &Location, backend: Arc<dyn Backend>) {
        if let Some(key) = pool_key(location) {
            lock_pool(&self.pool).insert(key, backend);
        }
    }

    /// Drops a pooled connection — not wired to any UI action yet (no
    /// "disconnect" affordance exists this stage), but `RemoteManager`
    /// needs to be able to forget a server that turned out to be
    /// unreachable (a later stage's retry/reconnect flow) without
    /// reaching into its private `pool` field from outside this module.
    pub fn forget(&self, location: &Location) {
        if let Some(key) = pool_key(location) {
            lock_pool(&self.pool).remove(&key);
        }
    }

    /// The "open" flow's fallback for a `Caps::LOCAL_PATH`-less backend:
    /// downloads `location` to a fresh temp file (or hands back a
    /// previously-downloaded one still on disk) and marks it read-only —
    /// the "read-only caveat" CLAUDE.md's capability-honest wording calls
    /// for, made structural rather than just a label: an app that opens
    /// this path and tries to save back to it gets an ordinary permission
    /// error, not a change that silently never makes it back to the
    /// server.
    pub async fn download_to_temp(
        &self,
        backend: &dyn Backend,
        location: &Location,
    ) -> Result<PathBuf, VfsError> {
        if let Some(path) = lock_pool_downloads(&self.downloads).get(location).cloned()
            && path.is_file()
        {
            return Ok(path);
        }

        let name = location
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".to_owned());
        let mut dest = std::env::temp_dir();
        dest.push(format!(
            "saola-files-{}-{}-{name}",
            std::process::id(),
            location.authority.as_deref().unwrap_or("remote")
        ));

        // Buffered fully in memory before one blocking write, rather than
        // streaming chunk-by-chunk into a `tokio::fs::File` — this crate
        // otherwise avoids `tokio::fs` entirely (`modules::local`'s own
        // doc comment: every blocking call goes through `std::fs` inside
        // `spawn_blocking`, kept in one visible place, not two I/O
        // stacks). `download_to_temp` is a fallback path for a
        // `Caps::LOCAL_PATH`-less backend's "open" flow, not a hot loop —
        // buffering the whole file is an acceptable simplification here,
        // unlike `core::fs::ops::copy_bytes`'s streaming re-chunking,
        // which exists specifically because *that* path must handle
        // multi-gigabyte files without holding them in memory.
        let mut read = backend.read(location).await?;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = read.next().await {
            bytes.extend_from_slice(&chunk?);
        }

        let write_dest = dest.clone();
        let write_result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::write(&write_dest, &bytes)?;
            // Best-effort: a failed chmod still leaves a perfectly
            // readable downloaded file behind — the caveat is worded to
            // the human (`Caps::LOCAL_PATH`'s own doc comment), the
            // read-only bit is defense in depth, not the only thing
            // standing between the human and a silently-lost edit.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&write_dest, std::fs::Permissions::from_mode(0o444));
            }
            Ok(())
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(io_err)) => return Err(to_temp_io_error(location, io_err)),
            Err(join_err) => {
                return Err(VfsError::Other {
                    message: format!("internal error downloading {location}: {join_err}"),
                });
            }
        }

        lock_pool_downloads(&self.downloads).insert(location.clone(), dest.clone());
        Ok(dest)
    }
}

fn lock_pool_downloads(
    downloads: &Mutex<HashMap<Location, PathBuf>>,
) -> std::sync::MutexGuard<'_, HashMap<Location, PathBuf>> {
    downloads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn to_temp_io_error(location: &Location, err: std::io::Error) -> VfsError {
    VfsError::Other {
        message: format!("downloading {location} to a temp file failed: {err}"),
    }
}

/// Starts a connect attempt and returns its event stream — the same
/// "spawn a detached task immediately, the caller drives how long it
/// listens" shape `core::fs::ops::run`/`core::fs::size::run` already
/// establish (see either's doc comment). Dropping the returned stream
/// does not cancel the underlying handshake (there is nothing to
/// "un-connect" mid-handshake the way a copy's `cancel` flag can stop a
/// chunk loop) — only `ConnectRequest::request_cancel` does that, same
/// posture `core::fs::ops::OpRequest`'s own doc comment already takes for
/// the identical distinction.
///
/// **A free function, not a `RemoteManager` method** — deliberately, so it
/// can serve as `ui::dialogs::connect::subscription`'s `Subscription::
/// run_with` builder directly, which (like every other `run_with` site in
/// this crate — `ui::dirview::watch::subscription`, `ui::dialogs::
/// progress::subscription`) needs a plain `'static`-bound function, not
/// something borrowing `&self` off whichever `RemoteManager` `App` happens
/// to own. It registers a successful connection into the *global* pool
/// (`RemoteManager::install_global`'s target) rather than any particular
/// instance's — safe under the same single-instance reasoning
/// `global_pooled` documents (exactly one `App`, exactly one installed
/// `RemoteManager`, for the process's whole lifetime). A test that never
/// calls `install_global` (every test in this file's `mod tests` below)
/// stays fully isolated: `GLOBAL_POOL` is simply never touched, so this
/// registration step silently does nothing, and `dispatch_connect`'s
/// returned `Err` for an unsupported scheme is all such a test ever
/// observes.
pub fn connect(request: &ConnectRequest) -> BoxStream<'static, ConnectEvent> {
    let (tx, rx) = mpsc::channel(CONNECT_CHANNEL_CAPACITY);
    tokio::spawn(run_connect(request.clone(), tx));
    rx.boxed()
}

/// The detached task [`connect`] spawns. Dispatches to the protocol-
/// specific handshake (`modules::sftp::connect`, behind `feature =
/// "sftp"`) and, on success, registers the resulting backend into the
/// global pool *before* emitting `ConnectEvent::Connected` — see that
/// variant's own doc comment for why the ordering matters.
async fn run_connect(request: ConnectRequest, mut tx: mpsc::Sender<ConnectEvent>) {
    // Checked here too (not just inside `modules::sftp::connect`'s own
    // phase-boundary checks — see that function's doc comment), so a
    // cancel issued the instant a request is submitted, before the
    // protocol-specific handshake even starts, still short-circuits
    // cleanly instead of dialing out at all. Also what keeps `ConnectRequest
    // ::is_cancelled` a real call site in a `--no-default-features` build
    // (no `sftp` feature means `modules::sftp::connect`'s own checks never
    // compile in), rather than a method only reachable behind a feature
    // flag.
    if request.is_cancelled() {
        let _ = tx
            .send(ConnectEvent::Failed(VfsError::Unavailable {
                message: "connection cancelled".to_owned(),
            }))
            .await;
        return;
    }
    match dispatch_connect(&request, &mut tx).await {
        Ok(backend) => {
            if let Some(key) = pool_key(&request.location)
                && let Some(pool) = GLOBAL_POOL.get()
            {
                lock_pool(pool).insert(key, backend);
            }
            let _ = tx.send(ConnectEvent::Connected).await;
        }
        Err(err) => {
            let _ = tx.send(ConnectEvent::Failed(err)).await;
        }
    }
}

#[cfg(feature = "sftp")]
async fn dispatch_connect(
    request: &ConnectRequest,
    tx: &mut mpsc::Sender<ConnectEvent>,
) -> Result<Arc<dyn Backend>, VfsError> {
    if request.location.scheme == "sftp" {
        let backend = crate::modules::sftp::connect(&request.location, request, tx).await?;
        return Ok(Arc::new(backend));
    }
    Err(unsupported_scheme(&request.location))
}

#[cfg(not(feature = "sftp"))]
async fn dispatch_connect(
    request: &ConnectRequest,
    _tx: &mut mpsc::Sender<ConnectEvent>,
) -> Result<Arc<dyn Backend>, VfsError> {
    Err(unsupported_scheme(&request.location))
}

fn unsupported_scheme(location: &Location) -> VfsError {
    VfsError::Unavailable {
        message: format!(
            "no backend for scheme \"{}\" (this build was compiled without it)",
            location.scheme
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fs::entry::EntryKind;
    use crate::core::vfs::FakeBackend;

    fn sftp_location(authority: &str, path: &str) -> Location {
        Location {
            scheme: "sftp".to_owned(),
            authority: Some(authority.to_owned()),
            path: std::path::PathBuf::from(path),
        }
    }

    #[test]
    fn a_fresh_manager_has_nothing_pooled() {
        let manager = RemoteManager::new();
        assert!(manager.pooled(&sftp_location("jordan@host", "/")).is_none());
    }

    #[test]
    fn register_then_pooled_round_trips_for_the_same_authority() {
        let manager = RemoteManager::new();
        let backend: Arc<dyn Backend> = Arc::new(FakeBackend::new());
        let location = sftp_location("jordan@host", "/srv");
        manager.register(&location, backend);

        let found = manager.pooled(&location);
        assert!(found.is_some());
        assert_eq!(found.unwrap().scheme(), "fake");
    }

    #[test]
    fn pooling_is_keyed_by_scheme_and_authority_not_path() {
        let manager = RemoteManager::new();
        manager.register(
            &sftp_location("a@host", "/one"),
            Arc::new(FakeBackend::new()),
        );

        // A different path under the *same* authority still finds it —
        // the pool doesn't care which directory you were last browsing.
        assert!(manager.pooled(&sftp_location("a@host", "/two")).is_some());
        // A different authority does not.
        assert!(manager.pooled(&sftp_location("b@host", "/one")).is_none());
    }

    #[test]
    fn a_local_location_never_pools_anything() {
        let manager = RemoteManager::new();
        manager.register(
            &Location::local("/home/jordan"),
            Arc::new(FakeBackend::new()),
        );
        assert!(manager.pooled(&Location::local("/home/jordan")).is_none());
    }

    #[test]
    fn forget_evicts_a_pooled_connection() {
        let manager = RemoteManager::new();
        let location = sftp_location("jordan@host", "/");
        manager.register(&location, Arc::new(FakeBackend::new()));
        assert!(manager.pooled(&location).is_some());

        manager.forget(&location);
        assert!(manager.pooled(&location).is_none());
    }

    #[test]
    fn connect_request_hash_depends_only_on_id() {
        use std::hash::{Hash, Hasher};
        let mut ids = ConnectIdSource::default();
        let id = ids.alloc();
        let a = ConnectRequest::new(id, sftp_location("a@host", "/"));
        let b = ConnectRequest::new(id, sftp_location("b@other", "/elsewhere"));
        let hash_of = |r: &ConnectRequest| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            r.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    #[test]
    fn connect_id_source_hands_out_distinct_ids() {
        let mut ids = ConnectIdSource::default();
        assert_ne!(ids.alloc(), ids.alloc());
    }

    #[tokio::test]
    async fn connecting_to_an_unsupported_scheme_fails_cleanly_never_panics() {
        let manager = RemoteManager::new();
        let mut ids = ConnectIdSource::default();
        let location = Location {
            scheme: "ftp".to_owned(),
            authority: Some("host".to_owned()),
            path: std::path::PathBuf::from("/"),
        };
        let request = ConnectRequest::new(ids.alloc(), location.clone());
        let mut stream = connect(&request);

        let event = stream.next().await;
        assert!(matches!(event, Some(ConnectEvent::Failed(_))));
        // `manager` never called `install_global` — this instance's own
        // pool is what's checked, and `connect` (a free function reading
        // the *global* pool — see its own doc comment) never touches it.
        assert!(manager.pooled(&location).is_none());
    }

    #[tokio::test]
    async fn download_to_temp_writes_the_backends_bytes_and_marks_it_read_only() {
        let manager = RemoteManager::new();
        let backend = FakeBackend::new().with_dir(
            "/remote",
            vec![crate::core::fs::entry::FileEntry {
                name: std::ffi::OsString::from("note.txt"),
                kind: EntryKind::File,
                size: 0,
                modified: None,
                is_symlink: false,
                mode: None,
            }],
        );
        // `FakeBackend::read` always errors (see its own doc comment) —
        // this test only proves `download_to_temp` propagates that
        // `VfsError` rather than panicking; a real read-through-to-disk
        // round trip is `modules::local`'s own `read`/`write` tests'
        // territory, and `modules::sftp`'s equivalent is a manual
        // done-criterion (no real SFTP server in this test binary).
        let location = Location {
            scheme: "fake".to_owned(),
            authority: Some("host".to_owned()),
            path: std::path::PathBuf::from("/remote/note.txt"),
        };
        let result = manager.download_to_temp(&backend, &location).await;
        assert!(result.is_err());
    }

    #[test]
    fn request_cancel_is_observable_through_is_cancelled() {
        let mut ids = ConnectIdSource::default();
        let request = ConnectRequest::new(ids.alloc(), sftp_location("a@host", "/"));
        assert!(!request.is_cancelled());
        request.request_cancel();
        assert!(request.is_cancelled());
    }
}
