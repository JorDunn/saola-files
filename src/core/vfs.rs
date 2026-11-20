//! The VFS core: [`Location`], [`Caps`], [`VfsError`], and the [`Backend`]
//! trait every protocol module implements. iced-free by design (see
//! CLAUDE.md's layering rule) — this file only ever sees `std`, `futures`,
//! and `async_trait`, never `iced`.
//!
//! `src/core/fs/` holds the pure data ([`crate::core::fs::entry::FileEntry`])
//! and comparators that travel through this trait; `src/modules/` holds the
//! implementations (`local.rs` this stage, `sftp.rs` later).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::sink::Sink;
use futures::stream::BoxStream;

use crate::core::fs::entry::FileEntry;

/// Where a directory view is pointed: a URI-shaped triple, kept structured
/// (not a pre-formatted `String`) so backends can compare/join paths
/// without reparsing. Local locations have `scheme == "file"` and no
/// authority; the `Display` impl renders them as a bare path (no
/// `file://`), matching what a human typed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    pub scheme: String,
    /// `user@host[:port]` for remote schemes; always `None` for `file`.
    pub authority: Option<String>,
    pub path: PathBuf,
}

impl Location {
    /// A local filesystem location.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Location {
            scheme: "file".to_owned(),
            authority: None,
            path: path.into(),
        }
    }

    pub fn is_local(&self) -> bool {
        self.scheme == "file"
    }

    /// The parent location, same scheme/authority. `None` at the root.
    pub fn parent(&self) -> Option<Location> {
        self.path.parent().map(|parent| Location {
            scheme: self.scheme.clone(),
            authority: self.authority.clone(),
            path: parent.to_path_buf(),
        })
    }

    /// This location with `name` appended as a child path segment.
    pub fn join(&self, name: impl AsRef<Path>) -> Location {
        Location {
            scheme: self.scheme.clone(),
            authority: self.authority.clone(),
            path: self.path.join(name),
        }
    }

    /// Parses `scheme://[authority]/path` (a remote location typed by hand,
    /// or read out of a config/bookmarks file) into a [`Location`]. Anything
    /// without a `"://"` is a bare local path. A scheme nothing recognizes
    /// still parses fine here — it surfaces as an ordinary "no backend"
    /// [`VfsError`] at load time via `modules::resolve`, not a parse error.
    /// Mirrors this type's own `Display` impl, so round-tripping "format,
    /// then parse" reproduces the same `Location`.
    ///
    /// Shared by the breadcrumb path/URI editor (`ui::dirview`) and the
    /// places sidebar's saved-`[[server]]` entries (`core::places`) — one
    /// hand-rolled URI grammar, not two copies that could drift.
    pub fn parse(input: &str) -> Location {
        let Some((scheme, rest)) = input.split_once("://") else {
            return Location::local(PathBuf::from(input));
        };
        if scheme.is_empty() {
            return Location::local(PathBuf::from(input));
        }
        match rest.find('/') {
            Some(path_start) => {
                let authority = &rest[..path_start];
                let path = &rest[path_start..];
                Location {
                    scheme: scheme.to_owned(),
                    authority: (!authority.is_empty()).then(|| authority.to_owned()),
                    path: PathBuf::from(path),
                }
            }
            // No `/` at all after the scheme — the whole remainder is the
            // authority, with an implied root path.
            None => Location {
                scheme: scheme.to_owned(),
                authority: (!rest.is_empty()).then(|| rest.to_owned()),
                path: PathBuf::from("/"),
            },
        }
    }
}

impl fmt::Display for Location {
    /// Human-facing rendering — breadcrumbs, error text, window titles.
    /// This is the one place `Location` touches `to_string_lossy`
    /// (transitively, via `Path::display`); everywhere else the `PathBuf`
    /// travels unconverted, per CLAUDE.md's OsString discipline.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.authority {
            Some(authority) => {
                write!(f, "{}://{}{}", self.scheme, authority, self.path.display())
            }
            None if self.scheme == "file" => write!(f, "{}", self.path.display()),
            None => write!(f, "{}://{}", self.scheme, self.path.display()),
        }
    }
}

bitflags::bitflags! {
    /// What a backend can actually do. The UI reads this to word itself
    /// honestly instead of hiding or disabling controls: no `TRASH` means
    /// permanent-delete copy, not a missing delete button; no `WATCH`
    /// means refresh-on-navigate + F5, not a broken live view; no
    /// `LOCAL_PATH` means "open" downloads to a temp file with a
    /// read-only caveat, not a silent failure.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Caps: u16 {
        /// `watch()` can return a live stream of [`DirEvent`]s.
        const WATCH = 1 << 0;
        /// Deletions go through a recoverable trash, not a permanent
        /// `remove()`.
        const TRASH = 1 << 1;
        /// `rename()` renames in place, without a copy+delete round trip.
        const RENAME_IN_PLACE = 1 << 2;
        /// [`Backend::set_permissions`] works (chmod). Inspecting the mode
        /// needs no cap: [`FileEntry::mode`] is `None` when a backend can't
        /// report it.
        const SET_PERMISSIONS = 1 << 3;
        /// Entries have a real local path, so "open" can hand it straight
        /// to an app instead of downloading to a temp file first.
        const LOCAL_PATH = 1 << 4;
        /// Thumbnails can be generated directly from `read()`.
        const THUMBNAILS = 1 << 5;
        /// [`Backend::symlink`] works.
        const SYMLINK = 1 << 6;
        /// [`Backend::statfs`] works (free-space numbers are available).
        const STATFS = 1 << 7;
    }
}

/// A VFS failure, worded for a human — the UI renders this text directly
/// as an empty-state message, never an error dialog. `EACCES` is a normal
/// Tuesday: it becomes [`VfsError::PermissionDenied`], not a panic or a
/// modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsError {
    NotFound {
        location: String,
    },
    PermissionDenied {
        location: String,
    },
    AlreadyExists {
        location: String,
    },
    NotADirectory {
        location: String,
    },
    IsADirectory {
        location: String,
    },
    /// The backend itself is unreachable (a dropped SFTP session, a D-Bus
    /// service that isn't running) — degrades to an empty state, never a
    /// crash.
    Unavailable {
        message: String,
    },
    /// The backend cannot do what was asked (its matching [`Caps`] bit is
    /// unset). This is the answer from every trait method's default body,
    /// so a UI that forgot to check `caps()` gets a worded error, not a
    /// panic. `operation` is a noun phrase for what was refused, such as
    /// `"creating links"`.
    Unsupported {
        operation: String,
    },
    /// Catch-all for I/O errors that don't map onto a more specific
    /// variant above; still human-worded, never a raw `io::Error` dump.
    Other {
        message: String,
    },
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::NotFound { location } => write!(f, "{location} doesn't exist"),
            VfsError::PermissionDenied { location } => {
                write!(f, "You don't have permission to open {location}")
            }
            VfsError::AlreadyExists { location } => write!(f, "{location} already exists"),
            VfsError::NotADirectory { location } => write!(f, "{location} isn't a folder"),
            VfsError::IsADirectory { location } => write!(f, "{location} is a folder"),
            VfsError::Unavailable { message } => write!(f, "{message}"),
            VfsError::Unsupported { operation } => {
                write!(f, "This location doesn't support {operation}")
            }
            VfsError::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for VfsError {}

/// A single change reported by [`Backend::watch`]. `modules::local` (Stage
/// 5, inotify) is the first backend to actually emit these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirEvent {
    Created(OsString),
    Removed(OsString),
    Renamed {
        from: OsString,
        to: OsString,
    },
    Changed(OsString),
    /// The backend can no longer guarantee it reported every change since
    /// the last event — either the kernel's own inotify queue overflowed
    /// (`IN_Q_OVERFLOW`, a documented inotify(7) gotcha: the reader didn't
    /// drain fast enough and the kernel dropped events rather than block),
    /// or this backend's own bounded bridge channel filled up because the
    /// UI-side consumer fell behind (CLAUDE.md's `try_send`-never-blocks
    /// rule means that channel drops rather than backs up the watcher).
    /// Either way, trying to reconcile from a possibly-incomplete event
    /// history would risk silently diverging from disk; the only correct
    /// recovery is a full re-list, same as a manual F5.
    Overflow,
}

/// Space on the filesystem holding a location, in bytes, from
/// [`Backend::statfs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    /// Size of the whole filesystem.
    pub total: u64,
    /// Unused bytes, including space only privileged users may use
    /// (`f_bfree`). Use `available` for "how much can I write".
    pub free: u64,
    /// Unused bytes an ordinary user may use (`f_bavail`).
    pub available: u64,
}

/// A chunk stream from [`Backend::read`]. Chunks are plain `Vec<u8>` (no
/// `bytes::Bytes` dependency yet — nothing downstream needs zero-copy
/// slicing this stage; revisit if the ops engine wants it).
pub type ReadStream = BoxStream<'static, Result<Vec<u8>, VfsError>>;

/// A chunk sink for [`Backend::write`] and [`Backend::write_with`].
///
/// **Completion contract (added Stage 8, when `core::fs::ops`'s Move
/// became the first caller that deletes a source right after a successful
/// copy):** `Sink::close` must not resolve until every chunk already sent
/// has been written at the destination, not merely handed off to some
/// detached worker, and it must resolve to the first write error if any
/// chunk failed. `modules::local::LocalBackend::write`'s `WriterSink`
/// is the reference implementation — it wraps the raw channel sender so
/// `poll_close` also joins the `spawn_blocking` writer thread, because a
/// bare `mpsc::Sender`'s own `Sink::poll_close` only disconnects the
/// channel and returns `Ready` immediately, which is *not* complete (see
/// that type's doc comment for the full story, including the integration
/// test that caught it). Every backend gives its own `close()` the same
/// guarantee (SFTP awaits the remote handle's close, which also collects
/// every outstanding write acknowledgement).
///
/// **Stable storage** is a separate, stronger promise: the bytes survive a
/// power cut. `close` only makes it when the sink was opened with
/// [`WriteOptions::sync`] set (local: `fsync`; SFTP: `fsync@openssh.com`
/// when the server offers it). It costs a disk flush per file, so the ops
/// engine asks for it only where a crash would otherwise lose data (a Move
/// removing its source, an Overwrite replacing the old file).
pub type WriteSink = Pin<Box<dyn Sink<Vec<u8>, Error = VfsError> + Send>>;

/// How [`Backend::write_with`] opens its file. `Default` is what plain
/// [`Backend::write`] does: create or truncate, no flush to stable storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// Create the file and fail with [`VfsError::AlreadyExists`] if
    /// anything is already at that name, a symlink included, instead of
    /// truncating it (local: `O_CREAT | O_EXCL`; SFTP: `CREATE | EXCLUDE`).
    /// Never writes through a symlink.
    pub create_new: bool,
    /// `close` also flushes the file to stable storage before it reports
    /// success; a failed flush is a failed write. See [`WriteSink`].
    pub sync: bool,
}

/// Every protocol backend implements this. Object-safe (`Box<dyn Backend>`
/// lives in the module registry, `src/modules/mod.rs`) via `async_trait`:
/// native `async fn` in traits isn't dyn-compatible without boxing the
/// returned future by hand, and `async_trait` does that boxing once, here,
/// instead of at every one of these nine methods.
///
/// `list` is deliberately coarse — the full `Vec<FileEntry>` in one call,
/// never a per-entry round trip — so a remote backend (SFTP) pays one
/// network round trip per directory, not one per file.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The URI scheme this backend serves (`"file"`, `"sftp"`, …). Used as
    /// the registry key in `src/modules/mod.rs`.
    fn scheme(&self) -> &'static str;

    /// What this backend instance can do. Not necessarily static per
    /// scheme forever — a real SFTP session might lose a capability a
    /// particular server doesn't support — but every backend built so far
    /// returns a fixed set.
    fn caps(&self) -> Caps;

    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError>;

    async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError>;

    async fn read(&self, location: &Location) -> Result<ReadStream, VfsError>;

    /// Create or truncate `location` and return a sink for its bytes. Same
    /// as [`Self::write_with`] with `WriteOptions::default()`.
    async fn write(&self, location: &Location) -> Result<WriteSink, VfsError>;

    /// Like [`Self::write`], with the choices in [`WriteOptions`]: create
    /// exclusively, and flush to stable storage on close.
    ///
    /// The default body is a *checked* fallback for a backend that has no
    /// native exclusive create: it refuses with `AlreadyExists` when
    /// `metadata` finds something at `location`, then calls `write`. That
    /// check and the create are two steps, so something created in between
    /// would be truncated; and `sync` is only as strong as that backend's
    /// `write`. Both real backends (local, SFTP) override it with the
    /// single-step version.
    async fn write_with(
        &self,
        location: &Location,
        options: WriteOptions,
    ) -> Result<WriteSink, VfsError> {
        if options.create_new && self.metadata(location).await.is_ok() {
            return Err(VfsError::AlreadyExists {
                location: location.to_string(),
            });
        }
        self.write(location).await
    }

    /// Whether `a` and `b` (both on this backend) name the same file, so
    /// that copying one onto the other would copy a file onto itself.
    ///
    /// Always `true` when the locations are equal: `Location` compares
    /// paths component by component, so `/d/./f` and `/d//f` equal `/d/f`.
    /// Beyond that a backend answers what it can tell: local compares
    /// device and inode after following symlinks (hard links, a symlink to
    /// the file, a path through a symlinked folder); SFTP compares the
    /// server's canonical paths (symlinks, not hard links). The default,
    /// and the answer when either side cannot be examined, is the plain
    /// comparison. Not a [`Caps`] bit: every backend can answer, some just
    /// see less.
    ///
    /// A trait method rather than an inode field on `FileEntry` because
    /// identity has to follow symlinks while `metadata` deliberately does
    /// not, and because adding a field touches every `FileEntry` literal in
    /// the tree.
    async fn same_file(&self, a: &Location, b: &Location) -> bool {
        a == b
    }

    async fn mkdir(&self, location: &Location) -> Result<(), VfsError>;

    async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError>;

    async fn remove(&self, location: &Location) -> Result<(), VfsError>;

    async fn set_times(
        &self,
        location: &Location,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), VfsError>;

    /// Create a symbolic link at `link` that points at `target`.
    ///
    /// `target` is stored verbatim, exactly as given (it may be relative
    /// and need not exist), which is why it is a `&Path` and not a
    /// [`Location`]: a link's target is just bytes to the filesystem.
    /// Needs [`Caps::SYMLINK`]; the default says [`VfsError::Unsupported`].
    async fn symlink(&self, _target: &Path, _link: &Location) -> Result<(), VfsError> {
        Err(VfsError::Unsupported {
            operation: "creating links".to_owned(),
        })
    }

    /// Set the permission bits of `location` (chmod). Only the low 12 bits
    /// (`0o7777`) of `mode` are used; file-type bits are ignored. Follows
    /// a symlink to its target, like `chmod(1)`. Needs
    /// [`Caps::SET_PERMISSIONS`]; the default says [`VfsError::Unsupported`].
    async fn set_permissions(&self, _location: &Location, _mode: u32) -> Result<(), VfsError> {
        Err(VfsError::Unsupported {
            operation: "changing permissions".to_owned(),
        })
    }

    /// Total, free and available bytes on the filesystem holding
    /// `location`. Needs [`Caps::STATFS`]; the default says
    /// [`VfsError::Unsupported`].
    async fn statfs(&self, _location: &Location) -> Result<FsStats, VfsError> {
        Err(VfsError::Unsupported {
            operation: "reporting free space".to_owned(),
        })
    }

    /// A live stream of changes under `location`, or `None` if this
    /// backend can't signal (`Caps::WATCH` unset) — the UI falls back to
    /// refresh-on-navigate + F5. Synchronous: a backend either already has
    /// a watch mechanism wired up (an open inotify handle, a live SFTP
    /// session) or it doesn't; there's nothing to `.await` to find out.
    fn watch(&self, location: &Location) -> Option<BoxStream<'static, DirEvent>>;
}

/// Lets a pooled connection (`core::remote::RemoteManager`'s `Arc<dyn
/// Backend>`) stand in anywhere a `Box<dyn Backend>` is expected —
/// `modules::resolve`'s whole Stage-14 remote path is `Box::new(arc) as
/// Box<dyn Backend>`, which needs `Arc<dyn Backend>` to itself implement
/// `Backend` (an unsized coercion needs the pointee to satisfy the trait,
/// not just wrap something that does). Allowed under the orphan rule
/// because `Backend` is a local trait even though `Arc` is foreign — every
/// method just forwards to the shared instance via `Arc::as_ref`, so this
/// is pure delegation, not a second implementation of anything.
#[async_trait]
impl Backend for std::sync::Arc<dyn Backend> {
    fn scheme(&self) -> &'static str {
        self.as_ref().scheme()
    }

    fn caps(&self) -> Caps {
        self.as_ref().caps()
    }

    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError> {
        self.as_ref().list(location).await
    }

    async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
        self.as_ref().metadata(location).await
    }

    async fn read(&self, location: &Location) -> Result<ReadStream, VfsError> {
        self.as_ref().read(location).await
    }

    async fn write(&self, location: &Location) -> Result<WriteSink, VfsError> {
        self.as_ref().write(location).await
    }

    async fn write_with(
        &self,
        location: &Location,
        options: WriteOptions,
    ) -> Result<WriteSink, VfsError> {
        self.as_ref().write_with(location, options).await
    }

    async fn same_file(&self, a: &Location, b: &Location) -> bool {
        self.as_ref().same_file(a, b).await
    }

    async fn mkdir(&self, location: &Location) -> Result<(), VfsError> {
        self.as_ref().mkdir(location).await
    }

    async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError> {
        self.as_ref().rename(from, to).await
    }

    async fn remove(&self, location: &Location) -> Result<(), VfsError> {
        self.as_ref().remove(location).await
    }

    async fn set_times(
        &self,
        location: &Location,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), VfsError> {
        self.as_ref().set_times(location, accessed, modified).await
    }

    async fn symlink(&self, target: &Path, link: &Location) -> Result<(), VfsError> {
        self.as_ref().symlink(target, link).await
    }

    async fn set_permissions(&self, location: &Location, mode: u32) -> Result<(), VfsError> {
        self.as_ref().set_permissions(location, mode).await
    }

    async fn statfs(&self, location: &Location) -> Result<FsStats, VfsError> {
        self.as_ref().statfs(location).await
    }

    fn watch(&self, location: &Location) -> Option<BoxStream<'static, DirEvent>> {
        self.as_ref().watch(location)
    }
}

/// An in-memory [`Backend`] for UI tests (selection, directory-view state
/// machine) that don't want real disk I/O. Directories are seeded up
/// front via [`FakeBackend::with_dir`]; mutating calls (`mkdir`/`rename`/
/// `remove`/`write`/`set_times`) return [`VfsError::Other`] — nothing in
/// Stage 3's UI exercises them yet.
pub struct FakeBackend {
    caps: Caps,
    dirs: HashMap<PathBuf, Vec<FileEntry>>,
    stats: Option<FsStats>,
    /// Calls to the link/permission/space methods, in order. A `Mutex`
    /// because the trait methods take `&self` (interior mutability), and
    /// `Backend` requires `Sync`, which rules out `RefCell`.
    calls: std::sync::Mutex<Vec<FakeCall>>,
}

/// One recorded call on a [`FakeBackend`], so tests can assert what a
/// feature asked the backend to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    Symlink {
        target: PathBuf,
        link: Location,
    },
    SetPermissions {
        location: Location,
        mode: u32,
    },
    Statfs {
        location: Location,
    },
    WriteWith {
        location: Location,
        options: WriteOptions,
    },
    SameFile {
        a: Location,
        b: Location,
    },
}

impl FakeBackend {
    pub fn new() -> Self {
        FakeBackend {
            caps: Caps::empty(),
            dirs: HashMap::new(),
            stats: None,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The numbers `statfs` returns when `Caps::STATFS` is set.
    pub fn with_stats(mut self, stats: FsStats) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Every symlink/permission/statfs call so far, refused ones included.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: FakeCall) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    /// `Unsupported` unless `cap` is set: the same rule a real backend
    /// follows, so UI tests see honest refusals.
    fn require(&self, cap: Caps, operation: &str) -> Result<(), VfsError> {
        if self.caps.contains(cap) {
            Ok(())
        } else {
            Err(VfsError::Unsupported {
                operation: operation.to_owned(),
            })
        }
    }

    pub fn with_caps(mut self, caps: Caps) -> Self {
        self.caps = caps;
        self
    }

    /// Seed a directory's listing. Entries are stored exactly as given —
    /// callers exercise `core::fs::sort` (or the view's own sort) on top
    /// of this separately; `FakeBackend` doesn't sort.
    pub fn with_dir(mut self, path: impl Into<PathBuf>, entries: Vec<FileEntry>) -> Self {
        self.dirs.insert(path.into(), entries);
        self
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn scheme(&self) -> &'static str {
        "fake"
    }

    fn caps(&self) -> Caps {
        self.caps
    }

    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, VfsError> {
        self.dirs
            .get(&location.path)
            .cloned()
            .ok_or_else(|| VfsError::NotFound {
                location: location.to_string(),
            })
    }

    async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
        let name = location.path.file_name().map(OsString::from);
        let parent = location.path.parent().unwrap_or(&location.path);
        name.and_then(|name| {
            self.dirs
                .get(parent)
                .and_then(|entries| entries.iter().find(|entry| entry.name == name))
        })
        .cloned()
        .ok_or_else(|| VfsError::NotFound {
            location: location.to_string(),
        })
    }

    async fn read(&self, location: &Location) -> Result<ReadStream, VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't read {location}"),
        })
    }

    async fn write(&self, location: &Location) -> Result<WriteSink, VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't write {location}"),
        })
    }

    /// Recorded (so the `Arc` forwarder test can see it arrive), then
    /// refused like `write`.
    async fn write_with(
        &self,
        location: &Location,
        options: WriteOptions,
    ) -> Result<WriteSink, VfsError> {
        self.record(FakeCall::WriteWith {
            location: location.clone(),
            options,
        });
        self.write(location).await
    }

    /// Recorded, then the plain location comparison.
    async fn same_file(&self, a: &Location, b: &Location) -> bool {
        self.record(FakeCall::SameFile {
            a: a.clone(),
            b: b.clone(),
        });
        a == b
    }

    async fn mkdir(&self, location: &Location) -> Result<(), VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't mkdir {location}"),
        })
    }

    async fn rename(&self, from: &Location, _to: &Location) -> Result<(), VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't rename {from}"),
        })
    }

    async fn remove(&self, location: &Location) -> Result<(), VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't remove {location}"),
        })
    }

    async fn set_times(
        &self,
        location: &Location,
        _accessed: Option<SystemTime>,
        _modified: Option<SystemTime>,
    ) -> Result<(), VfsError> {
        Err(VfsError::Other {
            message: format!("FakeBackend can't set times on {location}"),
        })
    }

    async fn symlink(&self, target: &Path, link: &Location) -> Result<(), VfsError> {
        self.record(FakeCall::Symlink {
            target: target.to_path_buf(),
            link: link.clone(),
        });
        self.require(Caps::SYMLINK, "creating links")
    }

    async fn set_permissions(&self, location: &Location, mode: u32) -> Result<(), VfsError> {
        self.record(FakeCall::SetPermissions {
            location: location.clone(),
            mode,
        });
        self.require(Caps::SET_PERMISSIONS, "changing permissions")
    }

    async fn statfs(&self, location: &Location) -> Result<FsStats, VfsError> {
        self.record(FakeCall::Statfs {
            location: location.clone(),
        });
        self.require(Caps::STATFS, "reporting free space")?;
        self.stats.ok_or_else(|| VfsError::Other {
            message: format!("FakeBackend has no space numbers for {location}"),
        })
    }

    fn watch(&self, _location: &Location) -> Option<BoxStream<'static, DirEvent>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fs::entry::EntryKind;
    use futures::SinkExt;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind,
            size: 0,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    #[test]
    fn local_location_displays_as_a_bare_path() {
        let location = Location::local("/home/jordan/Downloads");
        assert_eq!(location.to_string(), "/home/jordan/Downloads");
    }

    #[test]
    fn remote_location_displays_with_scheme_and_authority() {
        let location = Location {
            scheme: "sftp".to_owned(),
            authority: Some("jordan@10.0.0.10".to_owned()),
            path: PathBuf::from("/srv"),
        };
        assert_eq!(location.to_string(), "sftp://jordan@10.0.0.10/srv");
    }

    #[test]
    fn parent_and_join_round_trip() {
        let location = Location::local("/a/b");
        let parent = location.parent().unwrap();
        assert_eq!(parent, Location::local("/a"));
        assert_eq!(parent.join("b"), location);
        assert_eq!(Location::local("/").parent(), None);
    }

    #[test]
    fn parse_handles_local_and_remote_forms() {
        assert_eq!(
            Location::parse("/home/jordan"),
            Location::local("/home/jordan")
        );
        assert_eq!(
            Location::parse("sftp://jordan@host/srv"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/srv"),
            }
        );
        // No path at all after the authority: implied root.
        assert_eq!(
            Location::parse("sftp://jordan@host"),
            Location {
                scheme: "sftp".to_owned(),
                authority: Some("jordan@host".to_owned()),
                path: PathBuf::from("/"),
            }
        );
    }

    #[test]
    fn is_local_distinguishes_file_scheme_from_remote() {
        assert!(Location::local("/a").is_local());
        let remote = Location {
            scheme: "sftp".to_owned(),
            authority: Some("h".to_owned()),
            path: PathBuf::from("/"),
        };
        assert!(!remote.is_local());
    }

    #[test]
    fn caps_bits_combine_and_check() {
        let caps = Caps::TRASH | Caps::LOCAL_PATH;
        assert!(caps.contains(Caps::TRASH));
        assert!(caps.contains(Caps::LOCAL_PATH));
        assert!(!caps.contains(Caps::WATCH));
    }

    #[test]
    fn vfs_error_wording_is_human_readable() {
        let err = VfsError::PermissionDenied {
            location: "/root/secret".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "You don't have permission to open /root/secret"
        );
    }

    #[test]
    fn every_vfs_error_variant_words_something_human_readable() {
        // Every variant, not just the common ones — this is the whole
        // point of the type (CLAUDE.md: worded for a human, EACCES is a
        // normal Tuesday), and it doubles as the "constructed somewhere"
        // proof for the variants no backend triggers yet.
        let cases = [
            VfsError::NotFound {
                location: "/x".to_owned(),
            },
            VfsError::AlreadyExists {
                location: "/x".to_owned(),
            },
            VfsError::NotADirectory {
                location: "/x".to_owned(),
            },
            VfsError::IsADirectory {
                location: "/x".to_owned(),
            },
            VfsError::Unavailable {
                message: "the server hung up".to_owned(),
            },
            VfsError::Unsupported {
                operation: "creating links".to_owned(),
            },
            VfsError::Other {
                message: "something else went wrong".to_owned(),
            },
        ];
        for case in cases {
            assert!(!case.to_string().is_empty());
        }
    }

    #[test]
    fn dir_event_variants_are_plain_data() {
        // `FakeBackend::watch()` always returns `None` (only `modules::
        // local`'s real inotify backend produces these); this is the
        // type-level proof the variants themselves are sound
        // (constructible, comparable) independent of a real producer.
        let created = DirEvent::Created(OsString::from("a"));
        let removed = DirEvent::Removed(OsString::from("a"));
        let changed = DirEvent::Changed(OsString::from("a"));
        let renamed = DirEvent::Renamed {
            from: OsString::from("a"),
            to: OsString::from("b"),
        };
        assert_ne!(created, renamed);
        assert_ne!(removed, changed);
        assert_ne!(changed, DirEvent::Overflow);
        assert_eq!(created, DirEvent::Created(OsString::from("a")));
        assert_eq!(DirEvent::Overflow, DirEvent::Overflow);
    }

    #[test]
    fn fake_backend_lists_seeded_directories() {
        let backend = FakeBackend::new().with_dir(
            "/home",
            vec![
                entry("a.txt", EntryKind::File),
                entry("docs", EntryKind::Directory),
            ],
        );
        let result = futures::executor::block_on(backend.list(&Location::local("/home")));
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn fake_backend_reports_configured_caps_and_no_watch_stream() {
        let backend = FakeBackend::new().with_caps(Caps::TRASH);
        assert_eq!(backend.caps(), Caps::TRASH);
        assert!(backend.watch(&Location::local("/home")).is_none());
    }

    #[test]
    fn fake_backend_mutating_calls_are_worded_not_panicking() {
        let backend = FakeBackend::new();
        let loc = Location::local("/x");
        assert!(futures::executor::block_on(backend.read(&loc)).is_err());
        assert!(futures::executor::block_on(backend.write(&loc)).is_err());
        assert!(futures::executor::block_on(backend.mkdir(&loc)).is_err());
        assert!(futures::executor::block_on(backend.rename(&loc, &loc)).is_err());
        assert!(futures::executor::block_on(backend.remove(&loc)).is_err());
        assert!(futures::executor::block_on(backend.set_times(&loc, None, None)).is_err());
    }

    #[test]
    fn fake_backend_errors_on_unseeded_directories() {
        let backend = FakeBackend::new();
        let result = futures::executor::block_on(backend.list(&Location::local("/nowhere")));
        assert!(matches!(result, Err(VfsError::NotFound { .. })));
    }

    #[test]
    fn fake_backend_metadata_finds_seeded_entries() {
        let backend = FakeBackend::new().with_dir("/home", vec![entry("a.txt", EntryKind::File)]);
        let result = futures::executor::block_on(backend.metadata(&Location::local("/home/a.txt")));
        assert_eq!(result.unwrap().name, OsString::from("a.txt"));
    }

    // ── symlink / set_permissions / statfs ───────────────────────────────

    /// A backend that overrides nothing, to prove the trait's default
    /// bodies refuse with `Unsupported` instead of panicking.
    struct BareBackend;

    #[async_trait]
    impl Backend for BareBackend {
        fn scheme(&self) -> &'static str {
            "bare"
        }
        fn caps(&self) -> Caps {
            Caps::empty()
        }
        async fn list(&self, _: &Location) -> Result<Vec<FileEntry>, VfsError> {
            Ok(Vec::new())
        }
        async fn metadata(&self, l: &Location) -> Result<FileEntry, VfsError> {
            Err(VfsError::NotFound {
                location: l.to_string(),
            })
        }
        async fn read(&self, l: &Location) -> Result<ReadStream, VfsError> {
            Err(VfsError::NotFound {
                location: l.to_string(),
            })
        }
        async fn write(&self, l: &Location) -> Result<WriteSink, VfsError> {
            Err(VfsError::NotFound {
                location: l.to_string(),
            })
        }
        async fn mkdir(&self, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn rename(&self, _: &Location, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn remove(&self, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn set_times(
            &self,
            _: &Location,
            _: Option<SystemTime>,
            _: Option<SystemTime>,
        ) -> Result<(), VfsError> {
            Ok(())
        }
        fn watch(&self, _: &Location) -> Option<BoxStream<'static, DirEvent>> {
            None
        }
    }

    fn is_unsupported<T: std::fmt::Debug>(result: Result<T, VfsError>) -> bool {
        matches!(result, Err(VfsError::Unsupported { .. }))
    }

    #[tokio::test]
    async fn default_methods_answer_unsupported() {
        let backend = BareBackend;
        let here = Location::local("/x");
        assert!(is_unsupported(backend.symlink(Path::new("t"), &here).await));
        assert!(is_unsupported(backend.set_permissions(&here, 0o644).await));
        assert!(is_unsupported(backend.statfs(&here).await));
    }

    #[tokio::test]
    async fn fake_backend_records_calls_and_follows_its_caps() {
        let stats = FsStats {
            total: 100,
            free: 40,
            available: 30,
        };
        let fake = FakeBackend::new()
            .with_caps(Caps::SYMLINK | Caps::SET_PERMISSIONS | Caps::STATFS)
            .with_stats(stats);
        let link = Location::local("/d/link");
        let file = Location::local("/d/file");
        assert_eq!(fake.symlink(Path::new("file"), &link).await, Ok(()));
        assert_eq!(fake.set_permissions(&file, 0o600).await, Ok(()));
        assert_eq!(fake.statfs(&file).await, Ok(stats));
        assert_eq!(
            fake.calls(),
            vec![
                FakeCall::Symlink {
                    target: PathBuf::from("file"),
                    link,
                },
                FakeCall::SetPermissions {
                    location: file.clone(),
                    mode: 0o600,
                },
                FakeCall::Statfs { location: file },
            ]
        );
    }

    #[tokio::test]
    async fn fake_backend_without_caps_refuses_but_still_records() {
        let fake = FakeBackend::new();
        let here = Location::local("/x");
        assert!(is_unsupported(fake.symlink(Path::new("t"), &here).await));
        assert!(is_unsupported(fake.set_permissions(&here, 0o644).await));
        assert!(is_unsupported(fake.statfs(&here).await));
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test]
    async fn arc_forwarding_impl_matches_the_inner_backend() {
        let caps = Caps::SYMLINK | Caps::STATFS | Caps::SET_PERMISSIONS;
        let stats = FsStats {
            total: 9,
            free: 5,
            available: 4,
        };
        let fake = std::sync::Arc::new(FakeBackend::new().with_caps(caps).with_stats(stats));
        let inner: std::sync::Arc<dyn Backend> = fake.clone();
        // Calls go through `Backend for Arc<dyn Backend>`, not the inner
        // value directly: a forgotten forwarder would hit the trait's
        // default body and answer `Unsupported` despite the caps.
        assert_eq!(Backend::caps(&inner), caps);
        let here = Location::local("/x");
        assert_eq!(
            Backend::symlink(&inner, Path::new("t"), &here).await,
            Ok(())
        );
        assert_eq!(Backend::statfs(&inner, &here).await, Ok(stats));
        // A missed `set_permissions` forwarder would answer `Unsupported`
        // from the default body and record nothing on the inner fake.
        assert_eq!(Backend::set_permissions(&inner, &here, 0o644).await, Ok(()));
        assert!(fake.calls().contains(&FakeCall::SetPermissions {
            location: here,
            mode: 0o644,
        }));
    }

    #[tokio::test]
    async fn arc_forwarder_forwards_write_with_and_same_file() {
        let fake = std::sync::Arc::new(FakeBackend::new());
        let inner: std::sync::Arc<dyn Backend> = fake.clone();
        let here = Location::local("/d/f");
        let there = Location::local("/d/g");
        let options = WriteOptions {
            create_new: true,
            sync: true,
        };
        // A missing forwarder would run the trait's default bodies, which
        // call `metadata`/`write` (or compare locations) and record nothing.
        let _ = Backend::write_with(&inner, &here, options).await;
        assert!(!Backend::same_file(&inner, &here, &there).await);
        assert_eq!(
            fake.calls(),
            vec![
                FakeCall::WriteWith {
                    location: here.clone(),
                    options,
                },
                FakeCall::SameFile { a: here, b: there },
            ]
        );
    }

    #[tokio::test]
    async fn default_same_file_compares_locations_component_by_component() {
        let backend = BareBackend;
        let plain = Location::local("/d/f");
        assert!(backend.same_file(&plain, &Location::local("/d/./f")).await);
        assert!(backend.same_file(&plain, &Location::local("/d//f")).await);
        assert!(!backend.same_file(&plain, &Location::local("/d/g")).await);
    }

    /// `BareBackend` with one file, to exercise the default `write_with`.
    struct OneFileBackend;

    #[async_trait]
    impl Backend for OneFileBackend {
        fn scheme(&self) -> &'static str {
            "one"
        }
        fn caps(&self) -> Caps {
            Caps::empty()
        }
        async fn list(&self, _: &Location) -> Result<Vec<FileEntry>, VfsError> {
            Ok(Vec::new())
        }
        async fn metadata(&self, l: &Location) -> Result<FileEntry, VfsError> {
            if l.path == Path::new("/taken") {
                Ok(entry("taken", EntryKind::File))
            } else {
                Err(VfsError::NotFound {
                    location: l.to_string(),
                })
            }
        }
        async fn read(&self, l: &Location) -> Result<ReadStream, VfsError> {
            BareBackend.read(l).await
        }
        async fn write(&self, _: &Location) -> Result<WriteSink, VfsError> {
            Ok(Box::pin(
                futures::sink::drain().sink_map_err(|never| match never {}),
            ))
        }
        async fn mkdir(&self, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn rename(&self, _: &Location, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn remove(&self, _: &Location) -> Result<(), VfsError> {
            Ok(())
        }
        async fn set_times(
            &self,
            _: &Location,
            _: Option<SystemTime>,
            _: Option<SystemTime>,
        ) -> Result<(), VfsError> {
            Ok(())
        }
        fn watch(&self, _: &Location) -> Option<BoxStream<'static, DirEvent>> {
            None
        }
    }

    #[tokio::test]
    async fn default_write_with_refuses_an_existing_name_when_creating_new() {
        let backend = OneFileBackend;
        let new = WriteOptions {
            create_new: true,
            sync: false,
        };
        let taken = Location::local("/taken");
        assert!(matches!(
            backend.write_with(&taken, new).await,
            Err(VfsError::AlreadyExists { .. })
        ));
        assert!(
            backend
                .write_with(&Location::local("/free"), new)
                .await
                .is_ok()
        );
        // Without `create_new` it is plain `write`, which truncates.
        assert!(
            backend
                .write_with(&taken, WriteOptions::default())
                .await
                .is_ok()
        );
    }

    #[test]
    fn caps_bits_for_links_and_space_are_distinct() {
        let all = [
            Caps::WATCH,
            Caps::TRASH,
            Caps::RENAME_IN_PLACE,
            Caps::SET_PERMISSIONS,
            Caps::LOCAL_PATH,
            Caps::THUMBNAILS,
            Caps::SYMLINK,
            Caps::STATFS,
        ];
        let combined = all.iter().fold(Caps::empty(), |acc, cap| acc | *cap);
        assert_eq!(combined.bits().count_ones() as usize, all.len());
    }
}
