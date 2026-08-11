//! `SftpBackend` — the SFTP protocol module (Stage 14, `feature = "sftp"`).
//! See the dated survey comment in `Cargo.toml` for why `russh` +
//! `russh-sftp` were chosen over shelling out to the system `ssh` binary,
//! and the documented fallback design if that choice needs revisiting
//! later (the module boundary means switching never touches `core::vfs::
//! Backend`, `core::remote::RemoteManager`, or `ui::dialogs::connect`).
//!
//! **Handshake shape.** [`connect`] is the only entry point, called by
//! `core::remote::RemoteManager::connect`'s spawned task (never by
//! anything else — a real SSH session is too expensive to build
//! speculatively). It runs, in order:
//!
//! 1. **TCP + key exchange** (`russh::client::connect`), during which
//!    [`ClientHandler::check_server_key`] fires exactly once. It checks
//!    the standard `~/.ssh/known_hosts` (`russh::keys::known_hosts`,
//!    which understands OpenSSH's format including hashed `|1|salt|hash`
//!    entries) via a `spawn_blocking` call — file I/O never runs on the
//!    async task. A **known, matching** key returns immediately; a
//!    **changed** key (`known_hosts::Error::KeyChanged`, a real
//!    MITM-shaped mismatch) rejects the connection with no prompt at all
//!    — there is no sane "trust anyway" default for that case, unlike
//!    first contact. A genuinely **unknown** host sends
//!    `ConnectEvent::HostKeyPrompt` up `core::remote`'s event channel and
//!    awaits the human's answer (timeout-bounded — CLAUDE.md's "timeout
//!    foreign waits"); accepting appends to `known_hosts` via
//!    `learn_known_hosts` so the *next* connection to this host is silent.
//! 2. **Auth**, tried in the order CLAUDE.md's stage description names:
//!    ssh-agent (`russh::keys::agent::client::AgentClient::connect_env`,
//!    tried silently — an absent/unreachable agent is routine, not worth
//!    a prompt), then each default key file under `~/.ssh/`
//!    (`id_ed25519`, `id_ecdsa`, `id_rsa` — prompting for a passphrase
//!    only for a key that's actually encrypted, via
//!    `ConnectEvent::AuthPrompt { stage: AuthStage::KeyPassphrase, .. }`),
//!    then a plain password prompt
//!    (`AuthStage::Password`) as the last resort. Every prompt reuses
//!    `core::fs::ops`'s capacity-1 reply-channel pattern.
//! 3. **SFTP subsystem**: `channel_open_session` →
//!    `request_subsystem(true, "sftp")` → `SftpSession::new` over that
//!    channel's byte stream — exactly `russh-sftp`'s own documented
//!    client shape (see its `sftp_client.rs` example).
//!
//! **Disclosed simplifications** (all named here once rather than
//! scattered as inline caveats):
//! - A key file that fails to decode with no passphrase is *always*
//!   treated as "needs a passphrase, ask" — this crate doesn't
//!   distinguish that from a genuinely corrupt/unsupported key file, since
//!   either way the only next step available is asking and moving on if
//!   the answer doesn't help.
//! - Password auth gets exactly one attempt; a wrong password fails the
//!   whole connect rather than re-prompting. A future stage that wants
//!   OpenSSH's usual 3-attempt UX can loop `authenticate`'s password
//!   branch without touching anything else here.
//! - [`parse_authority`] doesn't understand bracketed IPv6 literals
//!   (`[::1]:2222`) — a literal with a colon in it will misparse. Every
//!   sibling Saola surface that types a URI by hand has the same gap
//!   today; revisit if it ever matters in practice.
//! - Every `russh_sftp::client::SftpSession` path method takes `impl
//!   Into<String>`, not raw bytes — see the dated `russh-sftp` survey
//!   comment in `Cargo.toml` for the non-UTF8-remote-filename gap this
//!   implies, and why it's an acceptable, disclosed one rather than a
//!   silent violation of CLAUDE.md's OsString discipline (which this
//!   module still honors at its own boundary: [`wire_path`] is the *one*
//!   place a `Location`'s `PathBuf` becomes a `String`, mirroring
//!   `core::vfs::Location::Display`'s own single `to_string_lossy` site).
//!
//! **No `Caps::WATCH`, `Caps::TRASH`, `Caps::LOCAL_PATH`, or
//! `Caps::THUMBNAILS`** — see [`SftpBackend::caps`]'s own doc comment for
//! why each is honestly absent. `Caps::RENAME_IN_PLACE` *is* set: SFTP's
//! `SSH_FXP_RENAME` is a real single server-side operation, not something
//! this module fakes with a copy+delete.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::sink::{Sink, SinkExt};
use futures::stream::{BoxStream, StreamExt};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::Metadata;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::remote::{AuthStage, ConnectEvent, ConnectRequest, HostKeyPrompt, PROMPT_TIMEOUT};
use crate::core::vfs::{Backend, Caps, DirEvent, Location, ReadStream, VfsError, WriteSink};

/// Default SSH port when the URI/authority didn't specify one — the same
/// default OpenSSH itself uses.
const DEFAULT_PORT: u16 = 22;

/// Bytes read/written per chunk on `read`/`write` — same value and
/// reasoning as `modules::local::CHUNK_SIZE`.
const CHUNK_SIZE: usize = 64 * 1024;

/// Channel capacity for `read`/`write`'s chunk bridge — same value and
/// reasoning as `modules::local::CHANNEL_CAPACITY`.
const CHANNEL_CAPACITY: usize = 4;

/// How long the initial TCP + key-exchange step waits before giving up —
/// separate from `PROMPT_TIMEOUT` (which bounds a *human* answering a
/// prompt): this bounds the network itself being unreachable, which
/// should fail far sooner than two minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Default key files tried, in order, after ssh-agent — the same
/// precedence OpenSSH's own client uses for its default `IdentityFile`
/// list (minus the legacy DSA entry, which nothing this app targets still
/// generates).
const DEFAULT_KEY_NAMES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

pub const SCHEME: &str = "sftp";

/// A live, authenticated SFTP session — see the module doc comment for
/// how [`connect`] builds one. `Clone` is cheap (every field is an `Arc`);
/// `modules::resolve` clones one out of `core::remote`'s pool on every
/// call, the same "backends are cheap to hand out" posture
/// `modules::local::LocalBackend` (a unit struct) already has, just paid
/// for once at connect time instead of never.
#[derive(Clone)]
pub struct SftpBackend {
    /// Keeps the underlying SSH connection alive for as long as any clone
    /// of this backend exists. Never touched again after [`connect`]'s
    /// handshake (every VFS operation goes through `sftp`, whose methods
    /// all take `&self`) — a bare `Arc` (no `Mutex`) is enough because
    /// nothing here ever needs `&mut Handle` a second time. Prefixed `_`
    /// because it's genuinely never *read*, only kept alive; naming it
    /// this way (rather than `#[allow(dead_code)]`) is self-documenting
    /// about why a field with no accessor still earns its place in the
    /// struct.
    _session: Arc<client::Handle<ClientHandler>>,
    sftp: Arc<SftpSession>,
}

#[async_trait]
impl Backend for SftpBackend {
    fn scheme(&self) -> &'static str {
        SCHEME
    }

    fn caps(&self) -> Caps {
        // No `WATCH`: SFTP has no server-push change notification; the UI
        // falls back to refresh-on-navigate + F5 (CLAUDE.md).
        // No `TRASH`: no server-side recycle bin to move into —
        // `main.rs::delete_one`'s permanent-delete branch is what a
        // non-`TRASH` backend gets, worded as such.
        // No `LOCAL_PATH`: files live on the remote host; "open" downloads
        // to a temp file via `core::remote::RemoteManager::
        // download_to_temp`, with a read-only caveat.
        // No `THUMBNAILS`: generating a directory's worth of thumbnails
        // one network round trip per file at a time is a real performance
        // concern this stage doesn't take on (see `core::thumbs`'s "known
        // gaps" for the local equivalent's own stated limits) — a future
        // stage could opt this in once there's a cache-first story.
        // `RENAME_IN_PLACE` *is* honest: `SSH_FXP_RENAME` really is one
        // server-side operation, so `core::fs::ops`'s same-backend fast
        // path and `core::fs::undo::can_undo_rename` both apply for free.
        Caps::RENAME_IN_PLACE
    }

    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError> {
        let path = wire_path(location);
        let entries = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|err| sftp_error(location, err))?;
        Ok(entries
            .filter(|entry| {
                let name = entry.file_name();
                name != "." && name != ".."
            })
            .map(|entry| entry_from_metadata(OsString::from(entry.file_name()), &entry.metadata()))
            .collect())
    }

    async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
        let path = wire_path(location);
        let meta = self
            .sftp
            .symlink_metadata(path)
            .await
            .map_err(|err| sftp_error(location, err))?;
        let name = location
            .path
            .file_name()
            .map(OsString::from)
            .unwrap_or_default();
        Ok(entry_from_metadata(name, &meta))
    }

    async fn read(&self, location: &Location) -> Result<ReadStream, VfsError> {
        let path = wire_path(location);
        let mut file = self
            .sftp
            .open(path)
            .await
            .map_err(|err| sftp_error(location, err))?;

        let (mut tx, rx) = mpsc::channel::<Result<Vec<u8>, VfsError>>(CHANNEL_CAPACITY);
        let loc = location.clone();
        // Detached, like `modules::local::LocalBackend::read` — `tokio::
        // spawn`, not `spawn_blocking`: `File`'s `AsyncRead` impl talks to
        // the SFTP channel over the shared runtime, it isn't a blocking
        // syscall wrapped for the blocking pool.
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        // Network death mid-read surfaces here as an
                        // ordinary `io::Error` — worded, never a panic.
                        let _ = tx
                            .send(Err(VfsError::Unavailable {
                                message: format!("{loc}: {err}"),
                            }))
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(rx.boxed())
    }

    async fn write(&self, location: &Location) -> Result<WriteSink, VfsError> {
        let path = wire_path(location);
        let file = self
            .sftp
            .create(path)
            .await
            .map_err(|err| sftp_error(location, err))?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let loc = location.clone();
        let join = tokio::spawn(async move {
            let mut file = file;
            while let Some(chunk) = rx.next().await {
                if let Err(err) = file.write_all(&chunk).await {
                    eprintln!("saola-files: write to {loc} failed: {err}");
                    return;
                }
            }
            // `File::shutdown` is this crate's "actually flush and close
            // the remote handle" call — see `core::vfs::WriteSink`'s
            // durability contract doc comment (the bug `modules::local::
            // WriterSink` was written to fix): a bare channel close alone
            // would not be durable.
            if let Err(err) = file.shutdown().await {
                eprintln!("saola-files: closing {loc} failed: {err}");
            }
        });

        Ok(Box::pin(SftpWriterSink {
            tx,
            join: Some(join),
            location: location.clone(),
        }))
    }

    async fn mkdir(&self, location: &Location) -> Result<(), VfsError> {
        let path = wire_path(location);
        self.sftp
            .create_dir(path)
            .await
            .map_err(|err| sftp_error(location, err))
    }

    async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError> {
        let (from_path, to_path) = (wire_path(from), wire_path(to));
        self.sftp
            .rename(from_path, to_path)
            .await
            .map_err(|err| sftp_error(from, err))
    }

    /// Non-recursive, like `modules::local::LocalBackend::remove` — the
    /// same contract `core::vfs::Backend::remove` documents everywhere:
    /// removes an empty directory or a single file/symlink, recursion is
    /// `core::fs::ops`'s job.
    async fn remove(&self, location: &Location) -> Result<(), VfsError> {
        let path = wire_path(location);
        let meta = self
            .sftp
            .symlink_metadata(&path)
            .await
            .map_err(|err| sftp_error(location, err))?;
        let result = if meta.is_dir() {
            self.sftp.remove_dir(&path).await
        } else {
            self.sftp.remove_file(&path).await
        };
        result.map_err(|err| sftp_error(location, err))
    }

    async fn set_times(
        &self,
        location: &Location,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), VfsError> {
        let path = wire_path(location);
        let attrs = FileAttributes {
            atime: accessed.and_then(system_time_to_u32),
            mtime: modified.and_then(system_time_to_u32),
            ..FileAttributes::default()
        };
        self.sftp
            .set_metadata(path, attrs)
            .await
            .map_err(|err| sftp_error(location, err))
    }

    /// Always `None` — no `Caps::WATCH` (see [`Self::caps`]).
    fn watch(&self, _location: &Location) -> Option<BoxStream<'static, DirEvent>> {
        None
    }
}

/// `Location::path` -> the wire path string every `SftpSession` method
/// wants — see the module doc comment's disclosed non-UTF8 gap. Named
/// (not inlined) so every call site reads the same way `modules::local`'s
/// blocking helpers do — one obvious conversion point, not scattered
/// `.to_string_lossy()` calls.
fn wire_path(location: &Location) -> String {
    location.path.to_string_lossy().into_owned()
}

/// Builds a [`FileEntry`] from an SFTP [`Metadata`] — the remote
/// equivalent of `modules::local::entry_from_metadata`, same "never
/// resolve a symlink's target kind" posture: a symlink's `file_type()`
/// reports `FileType::Symlink`, which is neither `is_dir()` nor
/// `is_file()`, so it falls to [`EntryKind::Other`] exactly the way a
/// local symlink does.
fn entry_from_metadata(name: OsString, meta: &Metadata) -> FileEntry {
    let is_symlink = meta.file_type().is_symlink();
    let kind = if meta.file_type().is_dir() {
        EntryKind::Directory
    } else if meta.file_type().is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    FileEntry {
        name,
        kind,
        size: meta.len(),
        modified: meta.modified().ok(),
        is_symlink,
        mode: meta.permissions.map(|mode| mode & 0o7777),
    }
}

fn system_time_to_u32(time: SystemTime) -> Option<u32> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u32::try_from(duration.as_secs()).ok())
}

/// Maps an SFTP protocol/transport error onto a human-worded [`VfsError`]
/// — the remote equivalent of `modules::local::io_error`. A
/// `SSH_FX_*`-status failure gets a specific wording where one exists;
/// anything else (a dropped connection, a malformed reply) degrades to
/// [`VfsError::Unavailable`], never a panic — CLAUDE.md: "Network death
/// mid-op must surface as a worded `VfsError`".
fn sftp_error(location: &Location, err: SftpError) -> VfsError {
    if let SftpError::Status(status) = &err {
        return match status.status_code {
            StatusCode::NoSuchFile => VfsError::NotFound {
                location: location.to_string(),
            },
            StatusCode::PermissionDenied => VfsError::PermissionDenied {
                location: location.to_string(),
            },
            StatusCode::NoConnection | StatusCode::ConnectionLost => VfsError::Unavailable {
                message: format!("{location}: the connection to the server was lost"),
            },
            _ => VfsError::Other {
                message: format!("{location}: {err}"),
            },
        };
    }
    VfsError::Unavailable {
        message: format!("{location}: {err}"),
    }
}

/// The [`WriteSink`] `SftpBackend::write` returns — a straight port of
/// `modules::local::WriterSink`'s shape (see that type's doc comment for
/// the full "why `Sink::close` must join the writer task" story); only
/// the writer task's own body differs (async SFTP writes instead of a
/// blocking-pool `std::fs::File`).
struct SftpWriterSink {
    tx: mpsc::Sender<Vec<u8>>,
    join: Option<JoinHandle<()>>,
    location: Location,
}

impl Sink<Vec<u8>> for SftpWriterSink {
    type Error = VfsError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        Pin::new(&mut self.tx)
            .poll_ready(cx)
            .map_err(|_| channel_closed(&self.location))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), VfsError> {
        Pin::new(&mut self.tx)
            .start_send(item)
            .map_err(|_| channel_closed(&self.location))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        Pin::new(&mut self.tx)
            .poll_flush(cx)
            .map_err(|_| channel_closed(&self.location))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        let _ = Pin::new(&mut self.tx).poll_close(cx);

        let Some(join) = self.join.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(join).poll(cx) {
            Poll::Ready(result) => {
                self.join = None;
                if let Err(err) = result {
                    return Poll::Ready(Err(VfsError::Other {
                        message: format!("internal error writing {}: {err}", self.location),
                    }));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn channel_closed(location: &Location) -> VfsError {
    VfsError::Other {
        message: format!("write channel to {location} closed"),
    }
}

// ── Connect / auth / host-key handshake ─────────────────────────────────

/// `russh::client::Handler` — see the module doc comment for the full
/// host-key flow. Everything else (`channel_open_confirmation` etc.) uses
/// the trait's own default (no-op) implementations; this backend only
/// ever opens one session channel and never receives unsolicited server
/// requests it needs to react to.
struct ClientHandler {
    host: String,
    port: u16,
    tx: mpsc::Sender<ConnectEvent>,
    /// Set by `check_server_key` when it rejects the connection, so
    /// [`connect`] can build a precise error message afterward —
    /// `client::connect` itself only bubbles up a generic "key rejected"-
    /// shaped `russh::Error` with no room for *why*.
    reject_reason: Arc<Mutex<Option<String>>>,
}

impl ClientHandler {
    fn set_reject_reason(&self, reason: String) {
        let mut guard = lock_mutex(&self.reject_reason);
        *guard = Some(reason);
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let host = self.host.clone();
        let port = self.port;
        let key_for_check = server_public_key.clone();
        let known = tokio::task::spawn_blocking(move || {
            russh::keys::known_hosts::check_known_hosts(&host, port, &key_for_check)
        })
        .await;

        match known {
            Ok(Ok(true)) => return Ok(true),
            // Not found at all — first contact, fall through to the
            // prompt below.
            Ok(Ok(false)) => {}
            // A recorded key exists and doesn't match (`KeyChanged`), or
            // the known_hosts file itself couldn't be parsed — either way
            // this is a hard reject with no prompt, per the module doc
            // comment: there is no sane "trust anyway" default here.
            Ok(Err(err)) => {
                self.set_reject_reason(format!("known_hosts check failed: {err}"));
                return Ok(false);
            }
            Err(join_err) => {
                self.set_reject_reason(format!("internal error checking known_hosts: {join_err}"));
                return Ok(false);
            }
        }

        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        let prompt = HostKeyPrompt {
            host: self.host.clone(),
            port: self.port,
            key_type: server_public_key.algorithm().to_string(),
            fingerprint,
        };
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        if !deliver(
            &mut self.tx,
            ConnectEvent::HostKeyPrompt {
                prompt,
                reply: reply_tx,
            },
        )
        .await
        {
            self.set_reject_reason("nobody was listening for the host-key prompt".to_owned());
            return Ok(false);
        }

        match tokio::time::timeout(PROMPT_TIMEOUT, reply_rx.next()).await {
            Ok(Some(true)) => {
                let host = self.host.clone();
                let port = self.port;
                let key_for_learn = server_public_key.clone();
                // Best-effort: a failed write to `known_hosts` doesn't
                // undo the human's decision to trust this key for *this*
                // session — it just means the next connection prompts
                // again, same as tonight's key never having been learned.
                let _ = tokio::task::spawn_blocking(move || {
                    russh::keys::known_hosts::learn_known_hosts(&host, port, &key_for_learn)
                })
                .await;
                Ok(true)
            }
            Ok(Some(false)) | Ok(None) => {
                self.set_reject_reason("the host key was not trusted".to_owned());
                Ok(false)
            }
            Err(_elapsed) => {
                self.set_reject_reason("timed out waiting for host-key confirmation".to_owned());
                Ok(false)
            }
        }
    }
}

/// Delivers `event`, retrying through a full channel rather than dropping
/// it — every `ConnectEvent` is must-deliver (see that type's own doc
/// comment). Mirrors `core::fs::ops::send_event`'s identical shape and
/// reasoning, just for this module's own `ConnectEvent` type.
async fn deliver(tx: &mut mpsc::Sender<ConnectEvent>, event: ConnectEvent) -> bool {
    loop {
        match tx.try_send(event.clone()) {
            Ok(()) => return true,
            Err(err) if err.is_full() => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(_) => return false,
        }
    }
}

/// Parses a [`Location`]'s `authority` (`"user@host:port"`, or any prefix
/// of that) into its three parts, defaulting the port to [`DEFAULT_PORT`]
/// when absent. Pure and hand-testable (no environment/network reads) —
/// see the module doc comment for the disclosed IPv6-literal gap.
pub(crate) fn parse_authority(authority: &str) -> (Option<String>, String, u16) {
    let (user, rest) = match authority.split_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
        // An `@` with nothing before it still splits off the (empty)
        // user — `rest` is what's left, not the untouched original
        // string with its leading `@` still attached.
        Some((_, rest)) => (None, rest),
        None => (None, authority),
    };
    match rest.rsplit_once(':') {
        Some((host, port_str)) if !host.is_empty() => match port_str.parse::<u16>() {
            Ok(port) => (user, host.to_owned(), port),
            // Not a real port — keep the whole remainder as the host
            // rather than guessing where the split should have gone.
            Err(_) => (user, rest.to_owned(), DEFAULT_PORT),
        },
        _ => (user, rest.to_owned(), DEFAULT_PORT),
    }
}

fn default_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "root".to_owned())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn cancelled_error() -> VfsError {
    VfsError::Unavailable {
        message: "connection cancelled".to_owned(),
    }
}

fn connect_error(
    host: &str,
    port: u16,
    err: russh::Error,
    reject_reason: &Arc<Mutex<Option<String>>>,
) -> VfsError {
    match lock_mutex(reject_reason).clone() {
        Some(reason) => VfsError::Unavailable {
            message: format!("{host}:{port}: {reason}"),
        },
        None => VfsError::Unavailable {
            message: format!("could not reach {host}:{port}: {err}"),
        },
    }
}

fn ssh_error(host: &str, err: russh::Error) -> VfsError {
    VfsError::Unavailable {
        message: format!("{host}: {err}"),
    }
}

/// One default key file's whole attempt: decode it (prompting for a
/// passphrase if it's encrypted), then try public-key auth with it.
/// Returns `Ok(true)` only on a genuine `AuthResult::Success`; everything
/// else (decode failure even after a passphrase, a passphrase prompt the
/// human skipped/let time out, or the server simply not accepting this
/// key) is `Ok(false)` — "try the next candidate", not an error, since
/// exhausting every key is an entirely ordinary path to falling through
/// to password auth.
async fn try_key_file(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    path: &Path,
    tx: &mut mpsc::Sender<ConnectEvent>,
) -> bool {
    let key = match russh::keys::load_secret_key(path, None) {
        Ok(key) => key,
        Err(_) => {
            let (reply_tx, mut reply_rx) = mpsc::channel(1);
            let delivered = deliver(
                tx,
                ConnectEvent::AuthPrompt {
                    stage: AuthStage::KeyPassphrase {
                        key_path: path.to_path_buf(),
                    },
                    reply: reply_tx,
                },
            )
            .await;
            if !delivered {
                return false;
            }
            let passphrase = match tokio::time::timeout(PROMPT_TIMEOUT, reply_rx.next()).await {
                Ok(Some(Some(passphrase))) => passphrase,
                _ => return false,
            };
            match russh::keys::load_secret_key(path, Some(&passphrase)) {
                Ok(key) => key,
                Err(_) => return false,
            }
        }
    };

    let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), None);
    match session.authenticate_publickey(user, with_hash).await {
        Ok(result) => result.success(),
        Err(_) => false,
    }
}

/// The whole auth ladder — ssh-agent, then default key files, then
/// password — see the module doc comment for the full reasoning behind
/// the order and each step's prompting behavior.
async fn authenticate(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    host: &str,
    tx: &mut mpsc::Sender<ConnectEvent>,
) -> Result<(), VfsError> {
    if let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for key in identities {
            if let Ok(result) = session
                .authenticate_publickey_with(user, key, None, &mut agent)
                .await
                && result.success()
            {
                return Ok(());
            }
        }
    }

    if let Some(home) = home_dir() {
        for name in DEFAULT_KEY_NAMES {
            let path = home.join(".ssh").join(name);
            if path.is_file() && try_key_file(session, user, &path, tx).await {
                return Ok(());
            }
        }
    }

    let (reply_tx, mut reply_rx) = mpsc::channel(1);
    let delivered = deliver(
        tx,
        ConnectEvent::AuthPrompt {
            stage: AuthStage::Password {
                user: user.to_owned(),
            },
            reply: reply_tx,
        },
    )
    .await;
    if !delivered {
        return Err(VfsError::Unavailable {
            message: "nobody was listening for the password prompt".to_owned(),
        });
    }
    let password = match tokio::time::timeout(PROMPT_TIMEOUT, reply_rx.next()).await {
        Ok(Some(Some(password))) => password,
        _ => {
            return Err(VfsError::PermissionDenied {
                location: host.to_owned(),
            });
        }
    };
    match session.authenticate_password(user, password).await {
        Ok(result) if result.success() => Ok(()),
        _ => Err(VfsError::PermissionDenied {
            location: host.to_owned(),
        }),
    }
}

/// The whole handshake — see the module doc comment. Called only from
/// `core::remote::RemoteManager`'s spawned connect task; `request` is
/// checked for cancellation between each major phase (there's no finer
/// granularity than that to check at — a single SFTP round trip isn't
/// interruptible mid-flight the way `core::fs::ops`'s chunked copy is).
pub(crate) async fn connect(
    location: &Location,
    request: &ConnectRequest,
    tx: &mut mpsc::Sender<ConnectEvent>,
) -> Result<SftpBackend, VfsError> {
    let authority = location.authority.as_deref().unwrap_or_default();
    let (user, host, port) = parse_authority(authority);
    let user = user.unwrap_or_else(default_user);

    if request.is_cancelled() {
        return Err(cancelled_error());
    }

    let reject_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        host: host.clone(),
        port,
        tx: tx.clone(),
        reject_reason: reject_reason.clone(),
    };

    let config = Arc::new(client::Config::default());
    let connect_future = client::connect(config, (host.as_str(), port), handler);
    let mut session = match tokio::time::timeout(CONNECT_TIMEOUT, connect_future).await {
        Ok(Ok(session)) => session,
        Ok(Err(err)) => return Err(connect_error(&host, port, err, &reject_reason)),
        Err(_elapsed) => {
            return Err(VfsError::Unavailable {
                message: format!("timed out connecting to {host}:{port}"),
            });
        }
    };

    if request.is_cancelled() {
        return Err(cancelled_error());
    }

    authenticate(&mut session, &user, &host, tx).await?;

    if request.is_cancelled() {
        return Err(cancelled_error());
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|err| ssh_error(&host, err))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|err| ssh_error(&host, err))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|err| VfsError::Unavailable {
            message: format!("sftp subsystem on {host} failed to start: {err}"),
        })?;

    Ok(SftpBackend {
        _session: Arc::new(session),
        sftp: Arc::new(sftp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `parse_authority` — pure, no environment/network reads ──────────

    #[test]
    fn parses_user_host_and_port() {
        assert_eq!(
            parse_authority("jordan@10.0.0.10:2222"),
            (Some("jordan".to_owned()), "10.0.0.10".to_owned(), 2222)
        );
    }

    #[test]
    fn defaults_to_port_22_with_no_port_given() {
        assert_eq!(
            parse_authority("jordan@10.0.0.10"),
            (Some("jordan".to_owned()), "10.0.0.10".to_owned(), 22)
        );
    }

    #[test]
    fn a_bare_host_has_no_user() {
        assert_eq!(
            parse_authority("example.com"),
            (None, "example.com".to_owned(), 22)
        );
    }

    #[test]
    fn a_bare_host_with_a_port_still_has_no_user() {
        assert_eq!(
            parse_authority("example.com:2200"),
            (None, "example.com".to_owned(), 2200)
        );
    }

    #[test]
    fn a_non_numeric_port_keeps_the_whole_remainder_as_the_host() {
        assert_eq!(
            parse_authority("jordan@host:notaport"),
            (Some("jordan".to_owned()), "host:notaport".to_owned(), 22)
        );
    }

    #[test]
    fn an_empty_user_before_the_at_sign_is_treated_as_no_user() {
        assert_eq!(parse_authority("@host"), (None, "host".to_owned(), 22));
    }

    #[test]
    fn round_trips_through_locations_display_and_parse() {
        // `Location::parse`/`Display` (core::vfs) already own the
        // `scheme://authority/path` grammar — this proves this module's
        // own `parse_authority` agrees with what a human-typed connect
        // dialog URI actually produces for the `authority` half.
        let location = Location::parse("sftp://jordan@10.0.0.10:2222/srv");
        let authority = location.authority.as_deref().unwrap_or_default();
        assert_eq!(
            parse_authority(authority),
            (Some("jordan".to_owned()), "10.0.0.10".to_owned(), 2222)
        );
    }

    // ── `wire_path` ───────────────────────────────────────────────────────

    #[test]
    fn wire_path_renders_the_locations_path_as_a_plain_string() {
        let location = Location {
            scheme: "sftp".to_owned(),
            authority: Some("jordan@host".to_owned()),
            path: PathBuf::from("/srv/data"),
        };
        assert_eq!(wire_path(&location), "/srv/data");
    }

    // ── `sftp_error` ─────────────────────────────────────────────────────

    #[test]
    fn sftp_error_words_no_such_file_as_not_found() {
        use russh_sftp::protocol::Status;
        let location = Location::parse("sftp://jordan@host/missing");
        let err = SftpError::Status(Status {
            id: 1,
            status_code: StatusCode::NoSuchFile,
            error_message: "no such file".to_owned(),
            language_tag: String::new(),
        });
        assert!(matches!(
            sftp_error(&location, err),
            VfsError::NotFound { .. }
        ));
    }

    #[test]
    fn sftp_error_falls_back_to_unavailable_for_a_transport_failure() {
        let location = Location::parse("sftp://jordan@host/x");
        let err = SftpError::IO("connection reset".to_owned());
        assert!(matches!(
            sftp_error(&location, err),
            VfsError::Unavailable { .. }
        ));
    }

    // ── `system_time_to_u32` ────────────────────────────────────────────

    #[test]
    fn system_time_to_u32_converts_a_reasonable_timestamp() {
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(system_time_to_u32(time), Some(1_700_000_000));
    }

    #[test]
    fn system_time_to_u32_is_none_before_the_epoch() {
        let time = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(system_time_to_u32(time), None);
    }
}
