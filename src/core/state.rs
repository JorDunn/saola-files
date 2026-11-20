//! Session state: what the app remembers between runs (window size, last
//! location, per-folder view settings) in `state.toml`.
//!
//! **State is not config.** `files.toml` is written by the user and read by
//! the app; `state.toml` is written by the app and never expected to be
//! hand-edited. The two live in different XDG directories for that reason
//! (`$XDG_CONFIG_HOME/saola/` vs `$XDG_STATE_HOME/saola/files/`), so a user
//! can version-control their config without the app's churn in it.
//!
//! The file is disposable: a missing, unreadable or corrupt file degrades to
//! [`State::default`] with at most one warning, and the app starts normally
//! (the no-panic rule). Nothing here ticks or polls — [`Store::save`] is
//! called by the app in response to a change event, and only then.
//!
//! # Format
//!
//! ```toml
//! last-location = "file:///home/jo/Pictures"
//!
//! [window]
//! width = 1100
//! height = 720
//!
//! [[folder]]            # least recently used first; capped at FOLDER_CAP
//! uri = "file:///home/jo/Pictures"
//! view = "grid"
//! sort = "size"
//! descending = true
//! ```
//!
//! Paths are bytes on Linux, not text, but TOML strings are UTF-8. So a
//! location is stored as a URI whose path is percent-encoded byte by byte
//! ([`location_key`]): a file name that is not valid UTF-8 round-trips
//! exactly instead of being mangled by `to_string_lossy`.
//!
//! # Adding a section (for other features)
//!
//! Two routes, both leave the other features' sections alone:
//!
//! * **Small or loosely-shaped data** (a zoom level, a column list): use
//!   [`State::extra`] / [`State::set_extra`] with your own top-level key.
//!   No change to this file at all.
//! * **A section with its own invariants** (a tag registry): add a typed
//!   field to [`State`] under the `// --- features (alphabetical) ---`
//!   markers, a `read_<feature>(&Table, &mut Vec<String>)` that warns about
//!   its own bad values, and one line each in `State::parse` and
//!   `State::to_table`.
//!
//! Either way, call `Store::save` from the app after the change.
//!
//! Unknown top-level keys are kept verbatim across a load/save, so an older
//! binary does not erase a newer binary's sections.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::config::{SortKey, View};
use crate::core::vfs::Location;

/// How many folders keep their own view settings. Past this, the least
/// recently used entry is dropped. 256 entries is roughly 25 KiB of TOML.
pub const FOLDER_CAP: usize = 256;

/// Smallest and largest window dimension (logical px) we accept back from
/// the file. Guards against a hand-edited or damaged value producing a
/// zero-sized or absurd window.
const MIN_WINDOW_DIM: u32 = 200;
const MAX_WINDOW_DIM: u32 = 16_384;

/// Top-level keys this module owns. [`State::set_extra`] refuses them so a
/// feature cannot overwrite window/folder state through the side door.
const RESERVED_KEYS: &[&str] = &["folder", "last-location", "version", "window"];

/// Window size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

impl WindowSize {
    /// From a logical size as the windowing layer reports it (floats).
    /// `None` outside the range we would accept back from the file, so
    /// nothing is saved that loading would then reject.
    pub fn from_logical(width: f32, height: f32) -> Option<Self> {
        // `as` from float to integer saturates (and maps NaN to 0), so
        // this cannot wrap; the range check then rejects the extremes.
        let (width, height) = (width.round() as u32, height.round() as u32);
        let range = MIN_WINDOW_DIM..=MAX_WINDOW_DIM;
        (range.contains(&width) && range.contains(&height)).then_some(WindowSize { width, height })
    }
}

/// The view settings remembered per folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderPrefs {
    pub view: View,
    pub sort: SortKey,
    pub descending: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub window: Option<WindowSize>,
    /// A [`location_key`] of the last local folder shown.
    pub last_location: Option<String>,
    /// `(location_key, prefs)`, least recently used first.
    folders: Vec<(String, FolderPrefs)>,
    /// Top-level keys this version does not know, kept for round-tripping
    /// and used by [`State::extra`].
    other: Table,
    // --- features (alphabetical) ---
}

impl State {
    /// The saved prefs for `key`, marking the entry most recently used
    /// (in memory; it reaches disk with the next save).
    pub fn folder_prefs(&mut self, key: &str) -> Option<FolderPrefs> {
        let index = self.folders.iter().position(|(k, _)| k == key)?;
        let entry = self.folders.remove(index);
        let prefs = entry.1;
        self.folders.push(entry);
        Some(prefs)
    }

    /// Records `prefs` for `key` as the most recently used entry, evicting
    /// the oldest past [`FOLDER_CAP`]. Returns whether anything changed
    /// (callers skip the disk write when it did not).
    pub fn set_folder_prefs(&mut self, key: &str, prefs: FolderPrefs) -> bool {
        if self
            .folders
            .last()
            .is_some_and(|(k, p)| k == key && *p == prefs)
        {
            return false;
        }
        self.folders.retain(|(k, _)| k != key);
        self.folders.push((key.to_owned(), prefs));
        self.enforce_cap();
        true
    }

    /// Forgets `key`'s prefs (the folder is back to the defaults). Returns
    /// whether an entry existed.
    pub fn clear_folder_prefs(&mut self, key: &str) -> bool {
        let before = self.folders.len();
        self.folders.retain(|(k, _)| k != key);
        self.folders.len() != before
    }

    /// Number of folders with saved prefs.
    pub fn folder_count(&self) -> usize {
        self.folders.len()
    }

    /// A feature-owned top-level value, e.g. `state.extra("zoom")`.
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.other.get(key)
    }

    /// Stores a feature-owned top-level value. Returns whether it changed;
    /// a reserved key (see [`RESERVED_KEYS`]) is refused (`false`).
    pub fn set_extra(&mut self, key: &str, value: Value) -> bool {
        if RESERVED_KEYS.contains(&key) || self.other.get(key) == Some(&value) {
            return false;
        }
        self.other.insert(key.to_owned(), value);
        true
    }

    fn enforce_cap(&mut self) {
        if self.folders.len() > FOLDER_CAP {
            let excess = self.folders.len() - FOLDER_CAP;
            self.folders.drain(..excess);
        }
    }

    /// Parses a `state.toml`. `Err` only for invalid TOML; a bad value
    /// inside a valid document is dropped on its own and described in the
    /// returned warnings (the same per-knob resilience as `files.toml`).
    pub fn parse(contents: &str) -> Result<(Self, Vec<String>), toml::de::Error> {
        // Hand-walked rather than `#[derive(Deserialize)]`: one damaged
        // entry must not discard the rest of the file.
        let mut body: Table = contents.parse()?;
        let mut warnings = Vec::new();

        let window = body
            .remove("window")
            .and_then(|value| read_window(&value, &mut warnings));
        let last_location = match body.remove("last-location") {
            None => None,
            Some(Value::String(key)) => Some(key),
            Some(_) => {
                warnings.push("last-location is not a string — ignored".to_owned());
                None
            }
        };
        let folders = read_folders(body.remove("folder"), &mut warnings);
        // --- features (alphabetical) ---

        // `version` is ours and rewritten on save; everything left is some
        // other feature's (or a newer binary's) and travels through.
        body.remove("version");

        let mut state = State {
            window,
            last_location,
            folders,
            other: body,
        };
        state.enforce_cap();
        Ok((state, warnings))
    }

    /// The document `Store::save` writes.
    pub fn to_table(&self) -> Table {
        let mut table = self.other.clone();
        table.insert("version".to_owned(), Value::Integer(1));
        if let Some(window) = self.window {
            let mut t = Table::new();
            t.insert("width".to_owned(), Value::Integer(window.width.into()));
            t.insert("height".to_owned(), Value::Integer(window.height.into()));
            table.insert("window".to_owned(), Value::Table(t));
        }
        if let Some(key) = &self.last_location {
            table.insert("last-location".to_owned(), Value::String(key.clone()));
        }
        if !self.folders.is_empty() {
            let rows = self
                .folders
                .iter()
                .map(|(key, prefs)| {
                    let mut t = Table::new();
                    t.insert("uri".to_owned(), Value::String(key.clone()));
                    t.insert(
                        "view".to_owned(),
                        Value::String(view_str(prefs.view).into()),
                    );
                    t.insert(
                        "sort".to_owned(),
                        Value::String(sort_str(prefs.sort).into()),
                    );
                    t.insert("descending".to_owned(), Value::Boolean(prefs.descending));
                    Value::Table(t)
                })
                .collect();
            table.insert("folder".to_owned(), Value::Array(rows));
        }
        // --- features (alphabetical) ---
        table
    }

    /// Serialised `state.toml` text. `None` only if the TOML writer
    /// rejects the document, which our own tables never do; callers treat
    /// that like a failed write rather than panicking.
    pub fn to_toml_string(&self) -> Option<String> {
        toml::to_string(&self.to_table()).ok()
    }
}

fn read_window(value: &Value, warnings: &mut Vec<String>) -> Option<WindowSize> {
    let dim = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (MIN_WINDOW_DIM..=MAX_WINDOW_DIM).contains(n))
    };
    match (dim("width"), dim("height")) {
        (Some(width), Some(height)) => Some(WindowSize { width, height }),
        _ => {
            warnings.push("window size is missing or out of range — using the default".to_owned());
            None
        }
    }
}

fn read_folders(value: Option<Value>, warnings: &mut Vec<String>) -> Vec<(String, FolderPrefs)> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Value::Array(rows) = value else {
        warnings.push("folder is not an array of tables — ignored".to_owned());
        return Vec::new();
    };
    let mut folders: Vec<(String, FolderPrefs)> = Vec::new();
    for row in &rows {
        let parsed = (|| {
            let uri = row.get("uri")?.as_str()?;
            let view = parse_view(row.get("view")?.as_str()?)?;
            let sort = parse_sort(row.get("sort")?.as_str()?)?;
            let descending = row.get("descending")?.as_bool()?;
            Some((
                uri.to_owned(),
                FolderPrefs {
                    view,
                    sort,
                    descending,
                },
            ))
        })();
        match parsed {
            // A duplicate uri means a hand edit; the later row wins.
            Some((uri, prefs)) => {
                folders.retain(|(k, _)| *k != uri);
                folders.push((uri, prefs));
            }
            None => warnings.push("a folder entry is malformed — skipped".to_owned()),
        }
    }
    folders
}

fn view_str(view: View) -> &'static str {
    match view {
        View::List => "list",
        View::Grid => "grid",
    }
}

fn parse_view(value: &str) -> Option<View> {
    match value {
        "list" => Some(View::List),
        "grid" => Some(View::Grid),
        _ => None,
    }
}

fn sort_str(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Type => "type",
    }
}

fn parse_sort(value: &str) -> Option<SortKey> {
    match value {
        "name" => Some(SortKey::Name),
        "size" => Some(SortKey::Size),
        "modified" => Some(SortKey::Modified),
        "type" => Some(SortKey::Type),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Location keys
// ---------------------------------------------------------------------

/// A stable, text-safe key for `location`: `scheme://authority/path` with
/// the path percent-encoded from its raw bytes. Everything except
/// `A-Za-z0-9-._~/` is `%XX`, so a non-UTF-8 name survives TOML.
pub fn location_key(location: &Location) -> String {
    let mut key = format!("{}://", location.scheme);
    if let Some(authority) = &location.authority {
        key.push_str(authority);
    }
    for &byte in location.path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            key.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail; `let _` states that.
            let _ = write!(key, "%{byte:02X}");
        }
    }
    key
}

/// The inverse of [`location_key`]. `None` for a key that is not in that
/// shape (a hand-edited or damaged file).
pub fn location_from_key(key: &str) -> Option<Location> {
    let (scheme, rest) = key.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let (authority, encoded_path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    Some(Location {
        scheme: scheme.to_owned(),
        authority: (!authority.is_empty()).then(|| authority.to_owned()),
        path: PathBuf::from(OsString::from_vec(percent_decode(encoded_path)?)),
    })
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = hex_value(bytes.next()?)?;
            let lo = hex_value(bytes.next()?)?;
            out.push(hi << 4 | lo);
        } else {
            out.push(byte);
        }
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Where the file lives, and reading/writing it
// ---------------------------------------------------------------------

/// Where `state.toml` lives, resolving the environment at this one thin
/// wrapper; the logic is [`state_path_from`]. `None` means no home to
/// keep state in (a bare CI sandbox): state is simply not remembered.
pub fn resolve_path() -> Option<PathBuf> {
    state_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// `$XDG_STATE_HOME/saola/files/state.toml`, else
/// `~/.local/state/saola/files/state.toml`. An empty value counts as unset
/// (the repo-wide env rule), and so does a relative `XDG_STATE_HOME`, which
/// the XDG spec says to ignore.
pub fn state_path_from(xdg_state: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg_state
        .filter(|dir| Path::new(dir).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("saola/files/state.toml"))
}

/// The state plus the file it came from and goes back to.
#[derive(Debug, Clone, Default)]
pub struct Store {
    path: Option<PathBuf>,
    pub state: State,
}

impl Store {
    /// Loads the state at `path`. Never fails: a missing file is silent
    /// defaults (first run), anything else wrong warns once and uses
    /// defaults. The next save then overwrites a corrupt file with a good
    /// one, which is the intended repair.
    pub fn load(path: Option<PathBuf>) -> Self {
        let state = path.as_deref().map(read_state).unwrap_or_default();
        Store { path, state }
    }

    /// Where this store writes, if anywhere.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Writes the state atomically: a temp file in the same directory,
    /// then `rename`, so a crash mid-write leaves the old file intact
    /// rather than a half-written one. No `fsync`: this is a convenience
    /// cache, and losing the last change on power failure is fine.
    /// No path (no `$HOME`) is a quiet `Ok`.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = self
            .state
            .to_toml_string()
            .ok_or_else(|| io::Error::other("state could not be serialised"))?;
        write_atomic(path, text.as_bytes())
    }
}

fn read_state(path: &Path) -> State {
    let contents = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Missing is the normal first run. Anything else (EACCES, a
        // directory in the way) is worth one line but never fatal.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return State::default(),
        Err(err) => {
            eprintln!(
                "saola-files: cannot read {} ({err}) — using defaults",
                path.display()
            );
            return State::default();
        }
    };
    let parsed = String::from_utf8(contents)
        .map_err(|err| err.to_string())
        .and_then(|text| State::parse(&text).map_err(|err| err.to_string()));
    match parsed {
        Ok((state, warnings)) => {
            for warning in warnings {
                eprintln!("saola-files: {}: {warning}", path.display());
            }
            state
        }
        Err(err) => {
            eprintln!(
                "saola-files: {} is not valid state ({err}) — using defaults",
                path.display()
            );
            State::default()
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // The pid keeps two running instances from sharing a temp file.
    let mut tmp_name: OsString = path.file_name().unwrap_or(OsStr::new("state.toml")).into();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // Best effort; a leftover temp file is harmless but untidy.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory under the system temp dir, removed on drop.
    /// (No `tempfile` dependency for this one use; `set_var` is never
    /// needed because every path is passed in.)
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "saola-files-state-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn prefs(view: View, sort: SortKey, descending: bool) -> FolderPrefs {
        FolderPrefs {
            view,
            sort,
            descending,
        }
    }

    fn grid_by_size() -> FolderPrefs {
        prefs(View::Grid, SortKey::Size, true)
    }

    #[test]
    fn round_trips_every_field() {
        let mut state = State {
            window: Some(WindowSize {
                width: 900,
                height: 600,
            }),
            last_location: Some("file:///home/jo".to_owned()),
            ..State::default()
        };
        state.set_folder_prefs("file:///a", grid_by_size());
        state.set_folder_prefs("file:///b", prefs(View::List, SortKey::Modified, false));

        let text = state.to_toml_string().expect("serialises");
        let (back, warnings) = State::parse(&text).expect("parses");
        assert_eq!(back, state);
        assert!(warnings.is_empty());
    }

    #[test]
    fn window_size_from_logical_rounds_and_rejects_extremes() {
        assert_eq!(
            WindowSize::from_logical(1100.4, 719.6),
            Some(WindowSize {
                width: 1100,
                height: 720
            })
        );
        assert_eq!(WindowSize::from_logical(0.0, 720.0), None);
        assert_eq!(WindowSize::from_logical(f32::NAN, 720.0), None);
        assert_eq!(WindowSize::from_logical(1e9, 720.0), None);
    }

    #[test]
    fn empty_document_is_default() {
        let (state, warnings) = State::parse("").expect("parses");
        assert_eq!(state, State::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_toml_is_an_error_for_the_loader_to_downgrade() {
        assert!(State::parse("window = [").is_err());
    }

    #[test]
    fn one_bad_folder_row_is_skipped_and_the_rest_survive() {
        let text = r#"
            [[folder]]
            uri = "file:///good"
            view = "grid"
            sort = "name"
            descending = false

            [[folder]]
            uri = "file:///bad"
            view = "carousel"
            sort = "name"
            descending = false
        "#;
        let (mut state, warnings) = State::parse(text).expect("parses");
        assert_eq!(state.folder_count(), 1);
        assert!(state.folder_prefs("file:///good").is_some());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn out_of_range_window_is_dropped_with_a_warning() {
        let (state, warnings) =
            State::parse("[window]\nwidth = 0\nheight = 720\n").expect("parses");
        assert_eq!(state.window, None);
        assert_eq!(warnings.len(), 1);
        let (state, _) = State::parse("[window]\nwidth = 99999\nheight = 720\n").expect("parses");
        assert_eq!(state.window, None);
    }

    #[test]
    fn wrong_typed_values_degrade_each_on_its_own() {
        let (state, warnings) = State::parse(
            "last-location = 5\nfolder = \"x\"\n[window]\nwidth = 800\nheight = 600\n",
        )
        .expect("parses");
        assert_eq!(
            state.window,
            Some(WindowSize {
                width: 800,
                height: 600
            })
        );
        assert_eq!(state.last_location, None);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn lru_evicts_the_oldest_past_the_cap() {
        let mut state = State::default();
        for n in 0..FOLDER_CAP + 10 {
            state.set_folder_prefs(&format!("file:///d{n}"), grid_by_size());
        }
        assert_eq!(state.folder_count(), FOLDER_CAP);
        assert_eq!(state.folder_prefs("file:///d0"), None);
        assert_eq!(state.folder_prefs("file:///d9"), None);
        assert!(state.folder_prefs("file:///d10").is_some());
    }

    #[test]
    fn looking_a_folder_up_protects_it_from_eviction() {
        let mut state = State::default();
        for n in 0..FOLDER_CAP {
            state.set_folder_prefs(&format!("file:///d{n}"), grid_by_size());
        }
        // d0 is the oldest; touching it makes d1 the oldest instead.
        assert!(state.folder_prefs("file:///d0").is_some());
        state.set_folder_prefs("file:///new", grid_by_size());
        assert!(state.folder_prefs("file:///d0").is_some());
        assert_eq!(state.folder_prefs("file:///d1"), None);
    }

    #[test]
    fn an_oversized_file_is_capped_on_load_keeping_the_newest() {
        let mut text = String::new();
        for n in 0..FOLDER_CAP + 5 {
            text.push_str(&format!(
                "[[folder]]\nuri = \"file:///d{n}\"\nview = \"grid\"\nsort = \"name\"\ndescending = false\n"
            ));
        }
        let (mut state, _) = State::parse(&text).expect("parses");
        assert_eq!(state.folder_count(), FOLDER_CAP);
        assert_eq!(state.folder_prefs("file:///d0"), None);
        assert!(
            state
                .folder_prefs(&format!("file:///d{}", FOLDER_CAP + 4))
                .is_some()
        );
    }

    #[test]
    fn set_reports_whether_anything_changed() {
        let mut state = State::default();
        assert!(state.set_folder_prefs("file:///a", grid_by_size()));
        assert!(!state.set_folder_prefs("file:///a", grid_by_size()));
        assert!(state.set_folder_prefs("file:///a", prefs(View::List, SortKey::Name, false)));
        assert!(state.clear_folder_prefs("file:///a"));
        assert!(!state.clear_folder_prefs("file:///a"));
    }

    #[test]
    fn unknown_sections_survive_a_round_trip_and_extras_work() {
        let (mut state, _) = State::parse("[zoom]\nlevel = 3\n").expect("parses");
        assert_eq!(
            state
                .extra("zoom")
                .and_then(|z| z.get("level"))
                .and_then(Value::as_integer),
            Some(3)
        );
        assert!(state.set_extra("columns", Value::Array(vec![Value::String("size".into())])));
        assert!(!state.set_extra("window", Value::Integer(1)), "reserved");

        let text = state.to_toml_string().expect("serialises");
        let (back, _) = State::parse(&text).expect("parses");
        assert_eq!(back, state);
    }

    #[test]
    fn location_keys_round_trip_including_non_utf8_paths() {
        let plain = Location::local("/home/jo/My Pictures/100%");
        assert_eq!(location_key(&plain), "file:///home/jo/My%20Pictures/100%25");
        assert_eq!(location_from_key(&location_key(&plain)), Some(plain));

        let raw = PathBuf::from(OsString::from_vec(b"/data/caf\xE9/\xFF".to_vec()));
        let odd = Location::local(raw);
        assert_eq!(location_from_key(&location_key(&odd)), Some(odd));

        let remote = Location {
            scheme: "sftp".to_owned(),
            authority: Some("jo@host:22".to_owned()),
            path: PathBuf::from("/srv"),
        };
        assert_eq!(location_from_key(&location_key(&remote)), Some(remote));
    }

    #[test]
    fn damaged_location_keys_decode_to_none() {
        assert_eq!(location_from_key("no scheme"), None);
        assert_eq!(location_from_key("file:///bad%2"), None);
        assert_eq!(location_from_key("file:///bad%ZZ"), None);
        assert_eq!(location_from_key("://x"), None);
    }

    #[test]
    fn path_resolution_chain_with_explicit_arguments() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            state_path_from(os("/xdg"), os("/home/jo")),
            Some(PathBuf::from("/xdg/saola/files/state.toml"))
        );
        assert_eq!(
            state_path_from(None, os("/home/jo")),
            Some(PathBuf::from(
                "/home/jo/.local/state/saola/files/state.toml"
            ))
        );
        // Empty and relative XDG values count as unset.
        assert_eq!(
            state_path_from(os(""), os("/home/jo")),
            Some(PathBuf::from(
                "/home/jo/.local/state/saola/files/state.toml"
            ))
        );
        assert_eq!(
            state_path_from(os("rel/dir"), os("/home/jo")),
            Some(PathBuf::from(
                "/home/jo/.local/state/saola/files/state.toml"
            ))
        );
        assert_eq!(state_path_from(None, os("")), None);
        assert_eq!(state_path_from(None, None), None);
    }

    #[test]
    fn save_then_load_restores_and_leaves_no_temp_file() {
        let scratch = Scratch::new();
        let path = scratch.0.join("saola/files/state.toml");
        let mut store = Store::load(Some(path.clone()));
        store.state.window = Some(WindowSize {
            width: 1234,
            height: 765,
        });
        store.state.set_folder_prefs("file:///a", grid_by_size());
        store.save().expect("saves (creating the directories)");

        let again = Store::load(Some(path.clone()));
        assert_eq!(again.state, store.state);

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("has parent"))
            .expect("lists")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, vec![OsString::from("state.toml")]);
    }

    #[test]
    fn missing_file_is_defaults_and_a_deleted_file_is_harmless() {
        let scratch = Scratch::new();
        let path = scratch.0.join("state.toml");
        let mut store = Store::load(Some(path.clone()));
        assert_eq!(store.state, State::default());
        store.state.last_location = Some("file:///x".to_owned());
        store.save().expect("saves");
        std::fs::remove_file(&path).expect("delete");
        assert_eq!(Store::load(Some(path)).state, State::default());
    }

    #[test]
    fn corrupt_and_non_utf8_files_fall_back_to_defaults_and_are_repaired_by_a_save() {
        let scratch = Scratch::new();
        let path = scratch.0.join("state.toml");

        std::fs::write(&path, "this is = = not toml").expect("write");
        let mut store = Store::load(Some(path.clone()));
        assert_eq!(store.state, State::default());

        std::fs::write(&path, [0xFF, 0xFE, 0x00]).expect("write");
        assert_eq!(Store::load(Some(path.clone())).state, State::default());

        store.state.window = Some(WindowSize {
            width: 800,
            height: 600,
        });
        store.save().expect("saves over the corrupt file");
        assert_eq!(Store::load(Some(path)).state, store.state);
    }

    #[test]
    fn an_unreadable_path_such_as_a_directory_degrades_to_defaults() {
        let scratch = Scratch::new();
        // A directory where the file should be: read fails with something
        // other than NotFound.
        assert_eq!(Store::load(Some(scratch.0.clone())).state, State::default());
    }

    #[test]
    fn a_store_without_a_path_never_writes() {
        let store = Store::load(None);
        assert!(store.save().is_ok());
        assert!(store.path().is_none());
    }
}
