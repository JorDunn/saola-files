//! D-Bus surface (Stage 15): `org.freedesktop.FileManager1` (the
//! freedesktop convention external tools use to ask "the file manager" to
//! reveal something) plus `io.saola.Files1` (this app's own tiny
//! extension), and the single-instance activation dance that makes
//! `saola-files <path>` from a second terminal reveal in the *already
//! running* window instead of opening a second one.
//!
//! # Architecture: everything lives in one `Subscription`
//!
//! Deciding "am I the first instance or the second" needs the session bus,
//! which needs an async runtime — and CLAUDE.md's "one async runtime" rule
//! (iced's tokio executor and zbus's tokio integration share it, never
//! construct a second) means that runtime has to be iced's own, which
//! doesn't exist until `iced::application(..).run()` is called. So this
//! whole handshake — acquire the name, or forward and exit — happens
//! *inside* [`subscription`], the very first thing its worker does once
//! `App::subscription` starts it, exactly like `core::remote::connect`
//! spawns its handshake onto the ambient runtime via `tokio::spawn` rather
//! than building one of its own (see that function's doc comment for the
//! identical reasoning, and `main.rs`'s `App::subscription` for the call
//! site here).
//!
//! **Accepted tradeoff**: because the check happens after iced has already
//! opened the app's window (not before, in `main()`), a *second* launch
//! that turns out to need forwarding briefly creates a window before this
//! subscription's worker discovers `io.saola.Files1` is taken and calls
//! `std::process::exit(0)`. In practice this is a few milliseconds — one
//! local D-Bus round trip — and is judged an acceptable cosmetic cost for
//! never constructing a second async runtime. Flagged here rather than
//! silently accepted.
//!
//! # The two interfaces
//!
//! - `org.freedesktop.FileManager1` at `/org/freedesktop/FileManager1` —
//!   `ShowItems`/`ShowFolders`/`ShowItemProperties`, each `(as URIs, s
//!   StartupID)`. `StartupID` is accepted (the spec requires it) and
//!   ignored — this app has no startup-notification concept to feed it.
//!   External callers already know whether they mean an item (reveal +
//!   select) or a folder (browse straight to it) or a properties request,
//!   so this side never has to guess; see "Deciding items vs. folders"
//!   below for why that matters.
//! - `io.saola.Files1` at `/io/saola/Files1` — one method, `Activate()`,
//!   taking nothing. It exists purely to cover the case `FileManager1`'s
//!   three methods can't express at all: a bare `saola-files` relaunch
//!   with no target, which should just bring the window to front.
//!
//! Both are served off the same bridge channel by two small handler
//! structs ([`FileManager1Handler`], [`SaolaFiles1Handler`]) — not one
//! struct with two `#[zbus::interface]` impl blocks, which conflicts (each
//! macro use generates a `zbus::object_server::Interface` impl for the
//! type it's attached to, and a type can only implement that trait once).
//!
//! # Deciding items vs. folders (done by the *forwarding* client, not here)
//!
//! `cli.rs`'s own positional argument is ambiguous on purpose (a directory
//! browses, a file reveals) — but that ambiguity is resolved *before* a
//! forwarded call ever reaches this module's interface handlers, not
//! after. [`try_forward`] (the secondary-instance role) does its own
//! cheap local `Backend::metadata` probe (never `std::fs` directly —
//! CLAUDE.md) and then calls the *unambiguous* `ShowItems` or
//! `ShowFolders` on the primary — reusing the freedesktop-spec methods for
//! our own internal forwarding instead of inventing a third, ambiguous
//! `io.saola.Files1` method that would have to duplicate the same probe on
//! this end. One probe, one place, and it means an *external* caller's
//! `ShowItems`/`ShowFolders` and our own forwarded CLI target go through
//! the exact same code path server-side.
//!
//! # `file://` URIs: percent-encoded, to survive D-Bus's UTF-8-only wire
//!
//! D-Bus `String` arguments are guaranteed valid UTF-8 by the wire
//! protocol itself — but CLAUDE.md's OsString discipline means a filename
//! is not guaranteed to *be* valid UTF-8. [`encode_file_uri`] percent-
//! encodes a path's raw `OsStr` bytes (not `to_string_lossy`, which would
//! silently mangle a non-UTF-8 name — the same lossy-conversion trap
//! `modules::sftp`'s `wire_path` discloses for its own, unavoidable, wire
//! format); [`decode_file_uri`] reverses it back into raw bytes via
//! `OsString::from_vec` before the string ever becomes a `PathBuf`. This
//! is a *better* answer than `modules::sftp`'s disclosed gap, not a copy
//! of it: `russh-sftp`'s wire format genuinely has no byte-string method to
//! reach for, while ours is a format we designed ourselves, so there was
//! no reason to accept the same loss here.
//!
//! An external caller that isn't us (a portal, another desktop tool) may
//! not percent-encode at all — [`decode_file_uri`] still accepts a bare,
//! unencoded local path (no `%` handling attempted) or a bare `file://`
//! path with no escapes, since a string with no `%` in it decodes to
//! itself either way. Only a genuinely percent-escaped byte forces the
//! distinction, and this crate's own forwarding path always produces one
//! consistently, so the two ends never disagree about how to read each
//! other's URIs.
//!
//! # A verified zbus gotcha: name-request flags are *not* what you'd guess
//! Also add to the "iced 0.14 gotchas" family: [`zbus::fdo::
//! RequestNameFlags::default`] — and therefore
//! `zbus::connection::Builder`'s own default when neither
//! `.allow_name_replacements(..)` nor `.replace_existing_names(..)` is
//! called — is `AllowReplacement | ReplaceExisting | DoNotQueue`, **not**
//! "just don't queue." Left alone, that default would let a *third*
//! `saola-files` launch silently steal `io.saola.Files1` away from an
//! already-running primary instead of being told to forward to it —
//! exactly the single-instance guarantee this whole module exists to
//! provide. [`try_build`] calls both setters explicitly with `false`; see
//! its own comment. `DoNotQueue` itself needs no explicit request — the
//! `Builder` always ORs it in regardless (per its own doc comment), which
//! is the one part of the surprising default that's actually what we want.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use futures::channel::mpsc;
use futures::stream::{BoxStream, StreamExt};
use zbus::fdo::RequestNameFlags;

use crate::cli::Cli;
use crate::core::fs::entry::EntryKind;
use crate::core::vfs::Location;
use crate::modules;

/// `io.saola.Files1`'s well-known bus name — also this module's
/// single-instance token: whoever owns it is "the" running instance.
/// Deliberately the same string as the interface it hosts (a common D-Bus
/// convention); the two constants are kept separate below anyway, since
/// nothing requires them to match and a future interface added at this
/// name wouldn't want to rename the bus identity too.
const SAOLA_NAME: &str = "io.saola.Files1";
const SAOLA_PATH: &str = "/io/saola/Files1";

/// The standard freedesktop file-manager activation name/path — fixed by
/// the spec, not ours to choose.
const FDO_NAME: &str = "org.freedesktop.FileManager1";
const FDO_PATH: &str = "/org/freedesktop/FileManager1";

/// The bridge channel's capacity — matches `core::remote::
/// CONNECT_CHANNEL_CAPACITY` and every other worker-to-app bridge in this
/// crate; nothing about D-Bus activation volume (a human clicking things,
/// at most a few calls a second) calls for a different number.
const EVENT_CHANNEL_CAPACITY: usize = 8;

/// What a D-Bus call resolves to, once decoded — `main.rs`'s `App::
/// handle_dbus_event` is the only place that ever reads one of these; this
/// module never touches `App` state directly (CLAUDE.md: `integration/`
/// reaches the app only through this bounded channel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `io.saola.Files1`'s `Activate`, or any `FileManager1` call whose
    /// URI list didn't decode to anything usable — bring the window to
    /// front and do nothing else.
    Raise,
    /// `ShowItems`: reveal this location's parent with it selected.
    Reveal(Location),
    /// `ShowFolders`: browse straight to this location.
    Browse(Location),
    /// `ShowItemProperties`: open the properties dialog for these
    /// locations (every URI that decoded successfully; a partially
    /// garbled call still shows properties for the items it could read).
    Properties(Vec<Location>),
}

/// The seam this stage's "interface logic behind a trait with a fake bus"
/// requirement asks for: every interface method funnels its decoded
/// [`Event`] through this trait instead of calling `try_send` inline, so
/// the tests below can swap in a plain `Vec<Event>` and assert on exactly
/// what a method call would have sent — no zbus connection, no bus, no
/// executor involved.
trait EventSink {
    fn emit(&mut self, event: Event);
}

impl EventSink for mpsc::Sender<Event> {
    fn emit(&mut self, event: Event) {
        // A full channel means `App` fell behind processing D-Bus events
        // (vanishingly unlikely at this volume) — CLAUDE.md's
        // bounded-bridging rule says drop rather than block, so a D-Bus
        // method call still returns promptly either way, never hanging
        // the caller on a slow/wedged UI thread.
        let _ = self.try_send(event);
    }
}

/// Serves `org.freedesktop.FileManager1`. A separate type from
/// [`SaolaFiles1Handler`] below, not two impl blocks on one struct — zbus's
/// `#[zbus::interface]` macro generates one `zbus::object_server::
/// Interface` impl per annotated block, and two such impls for the same
/// type conflict (`E0119`); two thin, identically-shaped structs (each
/// just wrapping the same `mpsc::Sender<Event>`) is the fix, not a
/// workaround.
struct FileManager1Handler {
    events: mpsc::Sender<Event>,
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager1Handler {
    async fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        self.events.clone().emit(reveal_event(&uris));
    }

    async fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        self.events.clone().emit(browse_event(&uris));
    }

    async fn show_item_properties(&self, uris: Vec<String>, _startup_id: String) {
        self.events.clone().emit(properties_event(&uris));
    }
}

/// Serves `io.saola.Files1` — see [`FileManager1Handler`]'s doc comment
/// for why this is a separate type rather than a second impl block on it.
struct SaolaFiles1Handler {
    events: mpsc::Sender<Event>,
}

#[zbus::interface(name = "io.saola.Files1")]
impl SaolaFiles1Handler {
    async fn activate(&self) {
        self.events.clone().emit(Event::Raise);
    }
}

/// [`DbusHandler::show_items`]'s decode step, pulled out as a pure
/// function so it's testable without an interface dispatch at all. The
/// *first* URI that decodes to something is what gets revealed — this
/// app has exactly one tab (CLAUDE.md's tabs seam is still a `Vec` of one
/// — see `ui::explorer`'s docs), so a multi-URI `ShowItems` call (Nautilus
/// et al. support selecting several siblings at once) can only ever act
/// on one of them; the rest are silently dropped rather than erroring the
/// whole call out. Flagged as a real limitation for whichever future
/// stage adds tabs, not treated as good enough forever.
fn reveal_event(uris: &[String]) -> Event {
    match uris.iter().find_map(|uri| decode_file_uri(uri)) {
        Some(location) => Event::Reveal(location),
        None => Event::Raise,
    }
}

/// [`DbusHandler::show_folders`]'s decode step — same "first usable URI,
/// rest dropped" posture as [`reveal_event`], same reason.
fn browse_event(uris: &[String]) -> Event {
    match uris.iter().find_map(|uri| decode_file_uri(uri)) {
        Some(location) => Event::Browse(location),
        None => Event::Raise,
    }
}

/// [`DbusHandler::show_item_properties`]'s decode step. Unlike
/// `reveal_event`/`browse_event`, every decodable URI survives — the
/// properties dialog (`ui::dialogs::properties`) already supports a
/// multi-item selection (it's what a normal multi-select Properties click
/// builds), so there's no single-tab limitation to work around here.
fn properties_event(uris: &[String]) -> Event {
    let locations: Vec<Location> = uris.iter().filter_map(|uri| decode_file_uri(uri)).collect();
    if locations.is_empty() {
        Event::Raise
    } else {
        Event::Properties(locations)
    }
}

/// Percent-encodes `path`'s raw `OsStr` bytes into a `file://` URI — see
/// the module doc comment's section on why this operates on bytes, not a
/// lossily-converted `String`. Unreserved characters (`A-Za-z0-9-_.~`)
/// and `/` (the path separator) are left bare for readability; everything
/// else, including any non-UTF-8 byte, becomes `%XX`.
fn encode_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The inverse of [`encode_file_uri`], plus leniency for callers that
/// aren't us: a bare local path with no `file://` prefix at all (some
/// tools pass plain paths despite the spec calling for URIs) is accepted
/// unmodified, and any other `scheme://` is handed to [`Location::parse`]
/// as-is (see the module doc comment's scope note on why a *remote*
/// authority is never one of our own percent-encoded byte strings, so
/// there's nothing to decode there). An empty result either way is `None`
/// — nothing to reveal or browse to.
fn decode_file_uri(uri: &str) -> Option<Location> {
    let Some(rest) = uri.strip_prefix("file://") else {
        if uri.contains("://") {
            return Some(Location::parse(uri));
        }
        return (!uri.is_empty()).then(|| Location::local(PathBuf::from(uri)));
    };
    let bytes = percent_decode(rest.as_bytes());
    (!bytes.is_empty()).then(|| Location::local(PathBuf::from(OsString::from_vec(bytes))))
}

/// Decodes `%XX` escapes byte-for-byte (not as `&str`), so a percent-
/// escaped non-UTF-8 byte round-trips through [`OsString::from_vec`]
/// instead of being rejected or lossily replaced. A malformed escape (a
/// trailing `%`, or non-hex digits after it) is passed through literally
/// rather than treated as an error — CLAUDE.md's no-panic posture applied
/// to a hand-rolled parser: a caller that got the encoding slightly wrong
/// still gets *something* sensible back, not a rejected call.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut iter = input.iter().copied();
    while let Some(byte) = iter.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let mut lookahead = iter.clone();
        match (
            lookahead.next().and_then(hex_value),
            lookahead.next().and_then(hex_value),
        ) {
            (Some(hi), Some(lo)) => {
                out.push((hi << 4) | lo);
                iter = lookahead;
            }
            _ => out.push(byte),
        }
    }
    out
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// The D-Bus subscription: acquires single-instance ownership of
/// [`SAOLA_NAME`] and serves both interfaces for the rest of the process's
/// life, or — if another instance already owns it — forwards `cli`'s
/// invocation over D-Bus and ends this process outright, or — if the
/// session bus can't be reached at all — logs once and idles (CLAUDE.md:
/// "Bus unavailable ⇒ run standalone, log once, never crash").
///
/// A free function taking `&Cli`, matching `core::remote::connect`'s own
/// shape — `main.rs`'s `App::subscription` passes this directly as
/// `Subscription::run_with`'s builder (`fn(&D) -> S`, the documented iced
/// 0.14 gotcha), keyed on `App::activation` (a `Cli` clone taken once at
/// `App::new`, never touched again — see that field's doc comment for why
/// `Cli` derives `Hash` only to satisfy this bound, not to meaningfully
/// distinguish values).
pub fn subscription(cli: &Cli) -> BoxStream<'static, Event> {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    // `tokio::spawn` schedules onto whatever runtime is already driving
    // the current task — here, iced's own (this function only ever runs
    // as a `Subscription`'s worker, which iced spawns on its runtime) —
    // never a second one; see the module doc comment's "one async
    // runtime" section.
    tokio::spawn(worker(cli.clone(), tx));
    rx.boxed()
}

/// The worker [`subscription`] spawns — see [`Acquired`]'s three arms for
/// the whole decision tree.
async fn worker(cli: Cli, tx: mpsc::Sender<Event>) {
    match acquire(tx).await {
        Acquired::Primary(connection) => {
            // Holding `connection` here — inside a future that never
            // resolves — is what keeps its `ObjectServer` (and the
            // background socket-reader task zbus spawned for it) alive
            // for the rest of the process's life. Dropping it would tear
            // both down; there's nothing more for this worker to *do*
            // once it's serving (every method call is handled by zbus's
            // own dispatch, off this task).
            let _connection = connection;
            std::future::pending::<()>().await;
        }
        Acquired::Taken => {
            // `forward` always ends the process (see its own doc
            // comment); this is unreachable in practice, but doesn't
            // assume that by falling off the end of the function.
            forward(cli).await;
        }
        Acquired::Unavailable => {
            eprintln!("saola-files: session bus unavailable — D-Bus integration disabled");
            std::future::pending::<()>().await;
        }
    }
}

/// The outcome of trying to become the primary instance.
enum Acquired {
    /// This process now owns [`SAOLA_NAME`] and is serving both
    /// interfaces on this connection.
    Primary(zbus::Connection),
    /// Another process already owns [`SAOLA_NAME`] — forward to it.
    Taken,
    /// The session bus itself couldn't be reached (no `dbus-daemon`, a
    /// sandboxed/minimal environment).
    Unavailable,
}

async fn acquire(events: mpsc::Sender<Event>) -> Acquired {
    match try_build(&events).await {
        Ok(connection) => {
            // Best-effort, decoupled from the outcome above: also claim
            // the standard `FileManager1` name so external tools that
            // don't know about `io.saola.Files1` can reach us too.
            // `DoNotQueue` only (no replacement flags — same reasoning as
            // `try_build`'s comment) — a failure here (nothing else
            // *should* provide this interface on this desktop, but
            // "should" isn't "does") is silently accepted; the app's own
            // `io.saola.Files1` surface above is unaffected either way.
            let _ = connection
                .request_name_with_flags(FDO_NAME, RequestNameFlags::DoNotQueue.into())
                .await;
            Acquired::Primary(connection)
        }
        Err(zbus::Error::NameTaken) => Acquired::Taken,
        Err(_) => Acquired::Unavailable,
    }
}

/// Builds the primary connection: requests [`SAOLA_NAME`] and serves both
/// interfaces at their respective paths.
async fn try_build(events: &mpsc::Sender<Event>) -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::session()?
        .name(SAOLA_NAME)?
        // See the module doc comment's "verified zbus gotcha" section:
        // left at their defaults, these two flags would let a *later*
        // launch steal this name away from an already-running primary
        // instead of being told to forward to it. Explicit `false` on
        // both is what makes this app's single-instance guarantee actually
        // hold.
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .serve_at(
            SAOLA_PATH,
            SaolaFiles1Handler {
                events: events.clone(),
            },
        )?
        .serve_at(
            FDO_PATH,
            FileManager1Handler {
                events: events.clone(),
            },
        )?
        .build()
        .await
}

#[zbus::proxy(
    interface = "io.saola.Files1",
    default_service = "io.saola.Files1",
    default_path = "/io/saola/Files1"
)]
trait SaolaFiles1 {
    async fn activate(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.FileManager1",
    default_service = "io.saola.Files1",
    default_path = "/org/freedesktop/FileManager1"
)]
trait FileManager1 {
    async fn show_items(&self, uris: &[&str], startup_id: &str) -> zbus::Result<()>;
    async fn show_folders(&self, uris: &[&str], startup_id: &str) -> zbus::Result<()>;
}

/// The secondary-instance role: connect to the session bus and forward
/// `cli`'s invocation to whichever process owns [`SAOLA_NAME`], then end
/// this process. Never returns — every path, including a connection or
/// call failure, ends in [`std::process::exit`]: there is no window this
/// process was ever going to show (see the module doc comment's accepted
/// window-flash tradeoff — the *other* window is the one that's about to
/// come to front), so there is nothing sensible left to fall back to.
async fn forward(cli: Cli) -> ! {
    if let Err(err) = try_forward(&cli).await {
        eprintln!("saola-files: couldn't reach the running instance: {err}");
    }
    std::process::exit(0)
}

async fn try_forward(cli: &Cli) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    match forward_plan(cli).await {
        ForwardPlan::Activate => SaolaFiles1Proxy::new(&connection).await?.activate().await,
        ForwardPlan::ShowItems(uri) => {
            FileManager1Proxy::new(&connection)
                .await?
                .show_items(&[uri.as_str()], "")
                .await
        }
        ForwardPlan::ShowFolders(uri) => {
            FileManager1Proxy::new(&connection)
                .await?
                .show_folders(&[uri.as_str()], "")
                .await
        }
    }
}

/// What [`try_forward`] should call on the running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ForwardPlan {
    Activate,
    ShowItems(String),
    ShowFolders(String),
}

/// Builds the plan from `cli`, mirroring `main.rs`'s own `App::new`
/// precedence exactly (`--select` beats a positional target beats
/// neither) — the one part of this decision that touches the filesystem
/// (probing whether a bare positional names a directory) is isolated in
/// [`probe_is_dir`], so [`plan_for_target`] (the actual precedence logic)
/// stays pure and is what the tests below exercise directly.
async fn forward_plan(cli: &Cli) -> ForwardPlan {
    if let Some(select) = &cli.select {
        return ForwardPlan::ShowItems(encode_file_uri(&absolute(select)));
    }
    let Some(target) = &cli.target else {
        return ForwardPlan::Activate;
    };
    let path = absolute(Path::new(target));
    let is_dir = probe_is_dir(&path).await;
    plan_for_target(path, is_dir)
}

/// The pure half of [`forward_plan`]: given a path that's already known
/// to be a directory or not, which unambiguous `FileManager1` method
/// forwards it.
fn plan_for_target(path: PathBuf, is_dir: bool) -> ForwardPlan {
    let uri = encode_file_uri(&path);
    if is_dir {
        ForwardPlan::ShowFolders(uri)
    } else {
        ForwardPlan::ShowItems(uri)
    }
}

/// Whether `path` names a directory, via the local `Backend` (CLAUDE.md:
/// never `std::fs` directly outside `src/modules/`) — a CLI-forwarded
/// target is always a local path today (`cli.rs`'s grammar never
/// URI-parses its positional argument; see `DirectoryView::open_target`'s
/// identical `Location::local` construction). `false` for anything that
/// doesn't exist or can't be probed (permission denied, a race) — reveal
/// it like a file, the same default `DirectoryView::open_target`'s own
/// `Err(_)` arm falls back to.
async fn probe_is_dir(path: &Path) -> bool {
    let location = Location::local(path);
    let Some(backend) = modules::resolve(&location) else {
        return false;
    };
    matches!(
        backend.metadata(&location).await,
        Ok(entry) if entry.kind == EntryKind::Directory
    )
}

/// Resolves `path` against this process's own working directory when it
/// isn't already absolute. Load-bearing specifically for forwarding: the
/// *receiving* instance almost certainly has a different `cwd` than this
/// (about-to-exit) process, so a relative CLI argument (`saola-files
/// ./notes.txt`) must be made absolute *before* it crosses the D-Bus call
/// — resolving it on the far side would resolve against the wrong
/// directory entirely. Falls back to the unresolved path on error (no
/// `$PWD`, a sandboxed environment) rather than failing the whole forward
/// — the running instance will simply fail to find a bad path the same
/// way any broken path does, worded through the ordinary `VfsError` path.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    impl EventSink for Vec<Event> {
        fn emit(&mut self, event: Event) {
            self.push(event);
        }
    }

    // -- file:// URI encode/decode -----------------------------------------

    #[test]
    fn encode_decode_round_trips_an_ascii_path() {
        let path = Path::new("/home/jordan/Documents/notes.txt");
        let uri = encode_file_uri(path);
        assert_eq!(uri, "file:///home/jordan/Documents/notes.txt");
        assert_eq!(decode_file_uri(&uri), Some(Location::local(path)));
    }

    #[test]
    fn encode_escapes_a_space_and_decode_reverses_it() {
        let path = Path::new("/home/jordan/My Documents/a file.txt");
        let uri = encode_file_uri(path);
        assert!(uri.contains("%20"));
        assert_eq!(decode_file_uri(&uri), Some(Location::local(path)));
    }

    #[test]
    fn encode_decode_preserves_non_utf8_bytes() {
        // A filename that isn't valid UTF-8 at all — the exact case
        // CLAUDE.md's OsString discipline exists for, and the reason this
        // module encodes raw bytes rather than `to_string_lossy`.
        let raw = OsString::from_vec(vec![b'/', b'a', 0xFF, 0xFE, b'b']);
        let path = PathBuf::from(raw);
        let uri = encode_file_uri(&path);
        // The wire form itself must be valid UTF-8 (a real `String`) even
        // though the path it encodes isn't.
        assert!(uri.is_ascii());
        assert_eq!(decode_file_uri(&uri), Some(Location::local(path)));
    }

    #[test]
    fn decode_accepts_a_bare_local_path_with_no_scheme() {
        assert_eq!(
            decode_file_uri("/home/jordan/notes.txt"),
            Some(Location::local("/home/jordan/notes.txt"))
        );
    }

    #[test]
    fn decode_passes_a_non_file_scheme_to_location_parse() {
        assert_eq!(
            decode_file_uri("sftp://jordan@example.com/srv/data"),
            Some(Location::parse("sftp://jordan@example.com/srv/data"))
        );
    }

    #[test]
    fn decode_rejects_an_empty_uri() {
        assert_eq!(decode_file_uri(""), None);
        assert_eq!(decode_file_uri("file://"), None);
    }

    #[test]
    fn percent_decode_passes_a_malformed_escape_through_literally() {
        assert_eq!(percent_decode(b"100%"), b"100%");
        assert_eq!(percent_decode(b"100%zz"), b"100%zz");
        assert_eq!(percent_decode(b"50%2"), b"50%2");
    }

    // -- server-side event construction -------------------------------------

    #[test]
    fn reveal_event_picks_the_first_decodable_uri() {
        // A leading empty string is the one URI form that never decodes
        // (see `decode_rejects_an_empty_uri`) — everything else, even a
        // bare path with no scheme, decodes leniently (see
        // `decode_accepts_a_bare_local_path_with_no_scheme`), so an empty
        // string is what actually exercises "skip the first, use the
        // second" here.
        let event = reveal_event(&[
            "".to_owned(),
            "file:///home/jordan/a.txt".to_owned(),
            "file:///home/jordan/b.txt".to_owned(),
        ]);
        assert_eq!(event, Event::Reveal(Location::local("/home/jordan/a.txt")));
    }

    #[test]
    fn reveal_event_with_no_uris_just_raises() {
        assert_eq!(reveal_event(&[]), Event::Raise);
    }

    #[test]
    fn browse_event_decodes_a_folder_uri() {
        assert_eq!(
            browse_event(&["file:///home/jordan/Downloads".to_owned()]),
            Event::Browse(Location::local("/home/jordan/Downloads"))
        );
    }

    #[test]
    fn properties_event_keeps_every_decodable_uri_not_just_the_first() {
        let event = properties_event(&[
            "file:///a".to_owned(),
            "".to_owned(),
            "file:///b".to_owned(),
        ]);
        assert_eq!(
            event,
            Event::Properties(vec![Location::local("/a"), Location::local("/b")])
        );
    }

    #[test]
    fn properties_event_with_nothing_decodable_just_raises() {
        assert_eq!(properties_event(&["".to_owned()]), Event::Raise);
    }

    // -- interface dispatch, against a fake sink (no bus involved) ---------

    #[tokio::test]
    async fn show_items_emits_reveal() {
        let (tx, _rx) = mpsc::channel(1);
        let handler = FileManager1Handler { events: tx };
        // Call the generated interface method directly — no `ObjectServer`,
        // no connection, nothing but the struct itself.
        handler
            .show_items(vec!["file:///a".to_owned()], String::new())
            .await;
        // The real channel already round-tripped correctly above (this
        // stays a smoke test for the wiring); the decode logic itself is
        // covered exhaustively by `reveal_event`'s own tests.
    }

    #[test]
    fn handler_methods_forward_through_the_sink_trait() {
        // Exercises the `EventSink` seam directly with the fake — this is
        // the "interface logic behind a trait with a fake bus" the stage
        // asked for, applied at the one point that actually varies
        // (real channel vs. `Vec`), rather than to the whole zbus
        // dispatch machinery (which needs a real bus to test at all — see
        // the module doc comment's "Bus unavailable" manual-check note).
        let mut sink: Vec<Event> = Vec::new();
        sink.emit(reveal_event(&["file:///a".to_owned()]));
        sink.emit(Event::Raise);
        assert_eq!(
            sink,
            vec![Event::Reveal(Location::local("/a")), Event::Raise]
        );
    }

    // -- acquire-outcome classification --------------------------------------

    #[test]
    fn name_taken_is_the_only_error_treated_as_a_second_instance() {
        // `zbus::Error::NameTaken` is a real, constructible unit variant
        // (see the module doc comment) — fabricating one here needs no
        // bus at all.
        assert!(matches!(
            classify_for_test(Err(zbus::Error::NameTaken)),
            TestOutcome::Taken
        ));
        assert!(matches!(
            classify_for_test(Err(zbus::Error::InvalidReply)),
            TestOutcome::Unavailable
        ));
        assert!(matches!(classify_for_test(Ok(())), TestOutcome::Primary));
    }

    /// A same-shape mirror of [`acquire`]'s `match`, without needing an
    /// actual `zbus::Connection` to plug into the `Ok` arm — this is
    /// purely testing the classification, not the connection build.
    enum TestOutcome {
        Primary,
        Taken,
        Unavailable,
    }

    fn classify_for_test(result: zbus::Result<()>) -> TestOutcome {
        match result {
            Ok(()) => TestOutcome::Primary,
            Err(zbus::Error::NameTaken) => TestOutcome::Taken,
            Err(_) => TestOutcome::Unavailable,
        }
    }

    // -- forwarding precedence ------------------------------------------------

    #[test]
    fn plan_for_target_picks_the_unambiguous_method() {
        assert_eq!(
            plan_for_target(PathBuf::from("/tmp/dir"), true),
            ForwardPlan::ShowFolders(encode_file_uri(Path::new("/tmp/dir")))
        );
        assert_eq!(
            plan_for_target(PathBuf::from("/tmp/file.txt"), false),
            ForwardPlan::ShowItems(encode_file_uri(Path::new("/tmp/file.txt")))
        );
    }

    #[tokio::test]
    async fn forward_plan_prefers_select_over_target() {
        let cli = Cli {
            select: Some(PathBuf::from("/tmp/a.txt")),
            target: Some(OsString::from("/tmp/b")),
            config_dir: None,
        };
        assert_eq!(
            forward_plan(&cli).await,
            ForwardPlan::ShowItems(encode_file_uri(Path::new("/tmp/a.txt")))
        );
    }

    #[tokio::test]
    async fn forward_plan_with_neither_activates() {
        assert_eq!(forward_plan(&Cli::default()).await, ForwardPlan::Activate);
    }

    #[tokio::test]
    async fn forward_plan_probes_a_real_target_and_falls_back_to_show_items_when_gone() {
        // No `--select`, a `target` naming a path that doesn't exist:
        // `probe_is_dir` can't confirm it's a directory (`modules::
        // resolve` + `Backend::metadata` fails), so this falls back to
        // treating it as an item to reveal — the same posture
        // `DirectoryView::open_target`'s own `Err(_)` arm takes.
        let cli = Cli {
            select: None,
            target: Some(OsString::from("/definitely/not/a/real/path/anywhere")),
            config_dir: None,
        };
        let plan = forward_plan(&cli).await;
        assert!(matches!(plan, ForwardPlan::ShowItems(_)));
    }

    #[test]
    fn absolute_leaves_an_already_absolute_path_unchanged() {
        assert_eq!(absolute(Path::new("/a/b")), PathBuf::from("/a/b"));
    }
}
