//! `core/apps.rs` — desktop-entry resolution and opening: the
//! `mimeapps.list` chain (which app is the default for a mimetype),
//! `.desktop` parsing, `Exec=` field-code expansion, "open in terminal",
//! and detached spawning. iced-free (CLAUDE.md's layering rule) — never
//! imports `ui/`.
//!
//! Every directory-walking/parsing function here takes its search
//! directories (or file contents) as plain arguments rather than reading
//! `$XDG_*`/`$HOME` itself — CLAUDE.md's "never `std::env::set_var` in a
//! test" rule, applied the same way `config.rs`'s `config_dir_from` already
//! does: [`AppsDb::load`] is the one thin wrapper that resolves the
//! environment; every test below drives the argument-taking core
//! ([`AppsDb::build`], [`parse_mimeapps_list`], [`parse_desktop_entry`], …)
//! with explicit tempdir fixtures or string literals instead.
//!
//! Scope cut, worth stating plainly: this module only ever *reads* the
//! `.desktop`/`mimeapps.list` ecosystem and spawns processes — it never
//! writes a `mimeapps.list` (no "always open with…" persistence yet) and
//! doesn't resolve the `kde4-foo.desktop` → `kde4/foo.desktop`-style
//! vendor-prefix desktop-id convention beyond one level of subdirectory
//! nesting. Both are real gaps in strict spec compliance, not correctness
//! bugs in what's implemented.
//!
//! **Why `discover_desktop_entries`/`parse_mimeapps_list`'s callers use
//! plain `std::fs` directly** (this file isn't under `src/modules/`):
//! CLAUDE.md's "all file access goes through a `Backend`" rule is about
//! the files a `Location` points at — the ones a directory view browses,
//! which may live on a remote backend. `.desktop`/`mimeapps.list` files
//! are always local system/user configuration, the same category
//! `config.rs` already reads directly via `std::fs::read_to_string` for
//! `files.toml` (and `core::mime::MimeDb::new()` reads indirectly via
//! `xdg_mime::SharedMimeInfo::new()` for the shared-MIME database) — never
//! something a remote backend could serve instead.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `files.toml` `terminal` → `$TERMINAL` → this, in that order (CLAUDE.md's
/// stated chain). Alacritty is Saola's own terminal (style guide §9).
const DEFAULT_TERMINAL: &str = "alacritty";

/// One parsed `.desktop` file's `[Desktop Entry]` group — only the keys
/// this app's opening flow actually reads (no `Icon=`/localized
/// `Name[xx]=` — this stage doesn't render app icons or honor locale).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The desktop-id this entry was discovered under (its file name,
    /// `applications`-relative, `/` replaced with `-` — the same id
    /// `mimeapps.list` names it by).
    pub id: String,
    pub name: String,
    pub exec: String,
    pub terminal: bool,
    pub no_display: bool,
    pub hidden: bool,
    pub mime_types: Vec<String>,
}

/// Parses one `.desktop` file's `[Desktop Entry]` group. Returns `None` if
/// there's no usable `Exec=` — an entry with nothing to run can't be
/// opened with, so it isn't worth keeping around (matches how a listing
/// entry that fails to parse elsewhere in this codebase is just skipped,
/// not surfaced as an error — e.g. `modules::local::list_blocking`).
pub fn parse_desktop_entry(id: &str, contents: &str) -> Option<DesktopEntry> {
    let mut in_entry_section = false;
    let mut name = String::new();
    let mut exec = String::new();
    let mut terminal = false;
    let mut no_display = false;
    let mut hidden = false;
    let mut mime_types = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = value.to_owned(),
            "Exec" => exec = value.to_owned(),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "MimeType" => {
                mime_types = value
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            _ => {}
        }
    }

    if exec.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        id: id.to_owned(),
        name: if name.is_empty() { id.to_owned() } else { name },
        exec,
        terminal,
        no_display,
        hidden,
        mime_types,
    })
}

/// One `mimeapps.list` file's three association sections, each mapping a
/// mimetype to an ordered list of desktop-ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MimeAppsFile {
    default_applications: HashMap<String, Vec<String>>,
    added_associations: HashMap<String, Vec<String>>,
    removed_associations: HashMap<String, Vec<String>>,
}

/// Parses one `mimeapps.list` document. Unknown sections/malformed lines
/// are silently skipped (CLAUDE.md's per-knob degradation posture, applied
/// to a file format this app doesn't own rather than a knob it does) —
/// there's no single "warn and default" story for a system file mixing
/// entries from a dozen packages, so this just takes what parses.
fn parse_mimeapps_list(contents: &str) -> MimeAppsFile {
    let mut file = MimeAppsFile::default();
    let mut section: Option<&mut HashMap<String, Vec<String>>> = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = match line {
                "[Default Applications]" => Some(&mut file.default_applications),
                "[Added Associations]" => Some(&mut file.added_associations),
                "[Removed Associations]" => Some(&mut file.removed_associations),
                _ => None,
            };
            continue;
        }
        let Some(bucket) = section.as_deref_mut() else {
            continue;
        };
        let Some((mimetype, ids)) = line.split_once('=') else {
            continue;
        };
        let ids: Vec<String> = ids
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !ids.is_empty() {
            bucket
                .entry(mimetype.trim().to_owned())
                .or_default()
                .extend(ids);
        }
    }

    file
}

/// Merges a priority-ordered chain of `mimeapps.list` files into one
/// mimetype → ordered-desktop-ids map. Earlier files' `[Default
/// Applications]` entries win first, then earlier files' `[Added
/// Associations]`, then later files fill in mimetypes nothing earlier
/// named — a simplification of the spec's full per-file-priority removal
/// semantics (a `[Removed Associations]` entry anywhere in the chain vetoes
/// that id for that mimetype everywhere, not just at-or-below its own
/// file's priority), documented here rather than chased further: real
/// `mimeapps.list` files essentially never rely on a *lower*-priority file
/// un-vetoing a *higher*-priority file's removal, so this conservative
/// reading matches practice.
fn merge_associations(files: &[MimeAppsFile]) -> HashMap<String, Vec<String>> {
    let mut removed_ever: HashMap<&str, HashSet<&str>> = HashMap::new();
    for file in files {
        for (mimetype, ids) in &file.removed_associations {
            let bucket = removed_ever.entry(mimetype.as_str()).or_default();
            bucket.extend(ids.iter().map(String::as_str));
        }
    }

    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    for file in files {
        for (mimetype, ids) in file
            .default_applications
            .iter()
            .chain(file.added_associations.iter())
        {
            let vetoed = removed_ever.get(mimetype.as_str());
            let bucket = result.entry(mimetype.clone()).or_default();
            for id in ids {
                if vetoed.is_some_and(|set| set.contains(id.as_str())) {
                    continue;
                }
                if !bucket.contains(id) {
                    bucket.push(id.clone());
                }
            }
        }
    }
    result
}

/// Discovers every `.desktop` file under `app_dirs` (each an
/// `applications` directory, highest priority first), one level of
/// subdirectory nesting included (`vendor/name.desktop` → id
/// `vendor-name`, the common real-world layout). An id already claimed by
/// an earlier (higher-priority) directory is never overwritten — the same
/// "earlier in the chain shadows later" rule `$XDG_DATA_DIRS` itself uses.
fn discover_desktop_entries(app_dirs: &[PathBuf]) -> HashMap<String, DesktopEntry> {
    let mut entries = HashMap::new();
    for dir in app_dirs {
        collect_desktop_files_one_dir(dir, &mut entries);
    }
    entries
}

fn collect_desktop_files_one_dir(dir: &Path, out: &mut HashMap<String, DesktopEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return; // Missing/unreadable dir — a normal Tuesday, not an error.
    };
    for item in read_dir.flatten() {
        let path = item.path();
        if path.is_dir() {
            // One level of subdirectory nesting only — see this module's
            // doc comment's scope-cut note.
            let Some(prefix) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let Ok(sub_read_dir) = std::fs::read_dir(&path) else {
                continue;
            };
            for sub_item in sub_read_dir.flatten() {
                let sub_path = sub_item.path();
                let Some(id) = desktop_id(&sub_path, Some(prefix)) else {
                    continue;
                };
                insert_desktop_file(&sub_path, id, out);
            }
            continue;
        }
        let Some(id) = desktop_id(&path, None) else {
            continue;
        };
        insert_desktop_file(&path, id, out);
    }
}

/// A desktop-id keeps its `.desktop` suffix — `mimeapps.list` itself
/// writes ids that way (`text/plain=nvim.desktop`), so the id this
/// function produces has to match that literally for `AppsDb::
/// default_for`'s lookup to ever hit. The subdirectory case follows the
/// same rule the spec uses for `$XDG_DATA_DIRS/applications/vendor/
/// tool.desktop`: prefix, `-`, then the full file name including its own
/// `.desktop` suffix — `vendor-tool.desktop`, not `vendor-tool`.
fn desktop_id(path: &Path, prefix: Option<&str>) -> Option<String> {
    if path.extension().and_then(OsStr::to_str) != Some("desktop") {
        return None;
    }
    let file_name = path.file_name()?.to_str()?;
    Some(match prefix {
        Some(prefix) => format!("{prefix}-{file_name}"),
        None => file_name.to_owned(),
    })
}

fn insert_desktop_file(path: &Path, id: String, out: &mut HashMap<String, DesktopEntry>) {
    if out.contains_key(&id) {
        return; // Higher-priority directory already claimed this id.
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    if let Some(entry) = parse_desktop_entry(&id, &contents) {
        out.insert(id, entry);
    }
}

/// The database this app's opening flow queries: every discovered
/// `.desktop` entry, plus the merged mimetype → default-app associations.
/// Building one walks every applications dir and parses every
/// `mimeapps.list`/`*.desktop` file found — expensive enough that
/// CLAUDE.md's shared-cache rule applies: one instance lives on `App`
/// (`main.rs`), built once at startup.
pub struct AppsDb {
    entries: HashMap<String, DesktopEntry>,
    associations: HashMap<String, Vec<String>>,
}

impl AppsDb {
    /// The testable core: builds a database from explicit directory lists,
    /// each highest-priority first. `config_dirs` are searched for
    /// `mimeapps.list`; `data_dirs` are searched both for their own
    /// `applications/mimeapps.list` (the deprecated-but-still-checked
    /// location the spec carries forward, lower priority than the
    /// config-dir chain) and for `applications/*.desktop`.
    pub fn build(config_dirs: &[PathBuf], data_dirs: &[PathBuf]) -> Self {
        let config_chain = config_dirs
            .iter()
            .filter_map(|dir| std::fs::read_to_string(dir.join("mimeapps.list")).ok())
            .map(|contents| parse_mimeapps_list(&contents));
        let data_chain = data_dirs
            .iter()
            .filter_map(|dir| std::fs::read_to_string(dir.join("applications/mimeapps.list")).ok())
            .map(|contents| parse_mimeapps_list(&contents));
        let all_files: Vec<MimeAppsFile> = config_chain.chain(data_chain).collect();
        let associations = merge_associations(&all_files);

        let app_dirs: Vec<PathBuf> = data_dirs
            .iter()
            .map(|dir| dir.join("applications"))
            .collect();
        let entries = discover_desktop_entries(&app_dirs);

        AppsDb {
            entries,
            associations,
        }
    }

    /// The thin environment-resolving wrapper (CLAUDE.md: never
    /// `std::env::set_var` in a test — this is the one place that reads
    /// `$XDG_*`/`$HOME`; [`Self::build`] above is what every test drives).
    pub fn load() -> Self {
        let (config_dirs, data_dirs) = resolve_xdg_dirs(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("XDG_CONFIG_DIRS"),
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("XDG_DATA_DIRS"),
            std::env::var_os("HOME"),
        );
        Self::build(&config_dirs, &data_dirs)
    }

    /// The desktop entry for a raw desktop-id — used to resolve an
    /// explicit Open-with choice back to something `open` can launch.
    pub fn entry(&self, id: &str) -> Option<&DesktopEntry> {
        self.entries.get(id)
    }

    /// The default app for `mimetype`, or `None` if nothing claims it (or
    /// every id that does names an entry this database never found).
    pub fn default_for(&self, mimetype: &str) -> Option<&DesktopEntry> {
        self.associations
            .get(mimetype)?
            .iter()
            .find_map(|id| self.entries.get(id))
    }

    /// Every known app that can open `mimetype`, for the Open-with popover
    /// — the resolved default first (if any), then every other installed,
    /// visible (`NoDisplay`/`Hidden` both false) app whose own `MimeType=`
    /// list names it, alphabetized by display name.
    pub fn candidates_for(&self, mimetype: &str) -> Vec<&DesktopEntry> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut result = Vec::new();
        if let Some(default) = self.default_for(mimetype) {
            seen.insert(default.id.as_str());
            result.push(default);
        }
        let mut rest: Vec<&DesktopEntry> = self
            .entries
            .values()
            .filter(|entry| {
                !entry.no_display
                    && !entry.hidden
                    && !seen.contains(entry.id.as_str())
                    && entry.mime_types.iter().any(|m| m == mimetype)
            })
            .collect();
        rest.sort_by(|a, b| a.name.cmp(&b.name));
        result.extend(rest);
        result
    }
}

/// The testable core of the `$XDG_CONFIG_HOME`/`$XDG_CONFIG_DIRS`/
/// `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` chain, mirroring `config.rs`'s
/// `config_dir_from`: every environment variable arrives as a plain
/// argument, and an env var set to the empty string counts as unset (the
/// same XDG-spec rule `config.rs` already applies). Returns
/// `(config_dirs, data_dirs)`, each highest-priority first.
fn resolve_xdg_dirs(
    config_home: Option<OsString>,
    config_dirs: Option<OsString>,
    data_home: Option<OsString>,
    data_dirs: Option<OsString>,
    home: Option<OsString>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let non_empty = |v: Option<OsString>| v.filter(|s| !s.is_empty());
    let home = non_empty(home).map(PathBuf::from);

    let mut configs = Vec::new();
    match non_empty(config_home) {
        Some(dir) => configs.push(PathBuf::from(dir)),
        None => {
            if let Some(home) = &home {
                configs.push(home.join(".config"));
            }
        }
    }
    match non_empty(config_dirs) {
        Some(dirs) => configs.extend(std::env::split_paths(&dirs)),
        None => configs.push(PathBuf::from("/etc/xdg")),
    }

    let mut data = Vec::new();
    match non_empty(data_home) {
        Some(dir) => data.push(PathBuf::from(dir)),
        None => {
            if let Some(home) = &home {
                data.push(home.join(".local/share"));
            }
        }
    }
    match non_empty(data_dirs) {
        Some(dirs) => data.extend(std::env::split_paths(&dirs)),
        None => {
            data.push(PathBuf::from("/usr/local/share"));
            data.push(PathBuf::from("/usr/share"));
        }
    }

    (configs, data)
}

// ── `Exec=` field-code expansion ────────────────────────────────────────

/// Tokenizes an `Exec=` value per the Desktop Entry Spec's quoting rules:
/// whitespace-separated outside double quotes; `\"`, `\\`, `` \` ``, `\$`
/// unescape to their bare character inside a quoted token. Simplified
/// versus the full spec grammar (no shell-metacharacter validation), but
/// faithful to every `Exec=` line this app is likely to actually meet.
fn tokenize_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => match chars.peek() {
                Some('"' | '\\' | '`' | '$') => {
                    // `.next()` is safe here: `peek()` just proved a
                    // character is there.
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                }
                _ => current.push('\\'),
            },
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    tokens
}

/// Whether `tokens` uses the list-form (`%F`/`%U`) or singular-form
/// (`%f`/`%u`) file field code — decides [`build_argv`]'s fan-out. List
/// wins if both somehow appear (spec says an entry should only use one).
fn wants_all_files_at_once(tokens: &[String]) -> bool {
    tokens.iter().any(|t| t == "%F" || t == "%U")
}

fn wants_one_file_per_spawn(tokens: &[String]) -> bool {
    tokens.iter().any(|t| t == "%f" || t == "%u")
}

/// Expands `exec`'s field codes into one or more argv vectors to spawn —
/// one spawn per selected target for a `%f`/`%u` (singular) app, one spawn
/// with every target for a `%F`/`%U` (list) app, or one bare spawn
/// (targets ignored) for an `Exec=` naming no file field code at all.
/// `%i`/`%c`/`%k` and the deprecated `%d`/`%D`/`%n`/`%N`/`%v`/`%m` codes
/// are dropped rather than guessed at; a literal `%%` unescapes to `%`.
pub fn build_argv(exec: &str, targets: &[PathBuf]) -> Vec<Vec<OsString>> {
    let tokens = tokenize_exec(exec);
    if tokens.is_empty() {
        return Vec::new();
    }
    if wants_all_files_at_once(&tokens) {
        return vec![expand_tokens(&tokens, targets)];
    }
    if wants_one_file_per_spawn(&tokens) && !targets.is_empty() {
        return targets
            .iter()
            .map(|target| expand_tokens(&tokens, std::slice::from_ref(target)))
            .collect();
    }
    vec![expand_tokens(&tokens, &[])]
}

fn expand_tokens(tokens: &[String], targets: &[PathBuf]) -> Vec<OsString> {
    let mut argv = Vec::new();
    for token in tokens {
        match token.as_str() {
            "%f" | "%F" => argv.extend(targets.iter().map(|t| t.clone().into_os_string())),
            "%u" | "%U" => argv.extend(targets.iter().map(|t| file_uri(t))),
            "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {
                // Dropped — see this function's docs.
            }
            other => argv.push(OsString::from(other.replace("%%", "%"))),
        }
    }
    argv
}

/// A minimal `file://` URI for `%u`/`%U` expansion: percent-encodes only
/// spaces and literal `%` signs, which covers the overwhelming majority of
/// real paths — a general RFC 3986 encoder is more than this stage needs
/// (`%u`/`%U` apps are rare next to `%f`/`%F` ones on a desktop that's
/// mostly local files). Iterates `char`s, not bytes, so multi-byte UTF-8
/// sequences pass through intact rather than being corrupted one byte at a
/// time. Lossily converts a non-UTF-8 path (the one sanctioned
/// `to_string_lossy` in this function).
fn file_uri(path: &Path) -> OsString {
    let display = path.to_string_lossy();
    let mut out = String::from("file://");
    for c in display.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    OsString::from(out)
}

// ── terminal resolution + spawning ──────────────────────────────────────

/// `files.toml` `terminal` → `$TERMINAL` → `alacritty` — the testable
/// core (CLAUDE.md: never `std::env::set_var` in a test). An empty string
/// in either source counts as unset, same rule `config.rs`'s own chain
/// uses.
pub fn resolve_terminal(config_terminal: Option<&str>, env_terminal: Option<&OsStr>) -> OsString {
    if let Some(terminal) = config_terminal.filter(|t| !t.is_empty()) {
        return OsString::from(terminal);
    }
    if let Some(terminal) = env_terminal.filter(|t| !t.is_empty()) {
        return terminal.to_owned();
    }
    OsString::from(DEFAULT_TERMINAL)
}

/// The thin environment-resolving wrapper around [`resolve_terminal`].
pub fn resolve_terminal_from_env(config_terminal: Option<&str>) -> OsString {
    resolve_terminal(config_terminal, std::env::var_os("TERMINAL").as_deref())
}

/// Detached spawn: redirected stdio, dropped `Child` handle — the same
/// shape `saola-panel`/`saola-capture` already use for launching external
/// processes (their `spawn_editor`/launcher-click handlers: `current_exe`
/// or a resolved program, `Stdio::null()` on every stream, `.spawn()` with
/// the `Child` immediately dropped). A spawn failure is worded and
/// returned, never a panic.
pub fn spawn_argv(program: &OsStr, args: &[OsString]) -> io::Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn spawn_argv_in(program: &OsStr, args: &[OsString], cwd: &Path) -> io::Result<()> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Opens `targets` with `entry` — builds argv via [`build_argv`] and spawns
/// once (list-form apps) or once per target (singular/bare-form apps). A
/// `Terminal=true` entry (the `.desktop` file documents itself as
/// expecting to run inside a terminal) is wrapped in `terminal -e ...`,
/// the same convention `open_terminal_here` and Alacritty/foot/xterm/urxvt
/// all share — not every terminal emulator honors `-e` identically, a
/// known simplification worth revisiting if a non-`-e` terminal becomes
/// the configured one. Stops at the first spawn failure rather than
/// attempting the rest of a multi-target batch.
pub fn open(entry: &DesktopEntry, targets: &[PathBuf], terminal: &OsStr) -> io::Result<()> {
    for argv in build_argv(&entry.exec, targets) {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        if entry.terminal {
            let mut wrapped = vec![OsString::from("-e"), program.clone()];
            wrapped.extend(args.iter().cloned());
            spawn_argv(terminal, &wrapped)?;
        } else {
            spawn_argv(program, args)?;
        }
    }
    Ok(())
}

/// "Open in terminal": a directory opens *in* itself; a file opens in its
/// parent — one function for both, per the stage's own wording ("a file
/// opens the terminal in its parent dir"). `is_dir` names which case
/// `path` is (the caller already knows from `EntryKind`/the location it
/// came from, so this never re-`stat`s).
pub fn open_terminal_here(terminal: &OsStr, is_dir: bool, path: &Path) -> io::Result<()> {
    let cwd = if is_dir {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    spawn_argv_in(terminal, &[], cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `.desktop` parsing ───────────────────────────────────────────────

    #[test]
    fn parses_a_minimal_desktop_entry() {
        let contents = "[Desktop Entry]\nType=Application\nName=Alacritty\nExec=alacritty %f\nMimeType=text/plain;\n";
        let entry = parse_desktop_entry("alacritty", contents).unwrap();
        assert_eq!(entry.name, "Alacritty");
        assert_eq!(entry.exec, "alacritty %f");
        assert!(!entry.terminal);
        assert_eq!(entry.mime_types, vec!["text/plain"]);
    }

    #[test]
    fn missing_exec_is_not_launchable() {
        let contents = "[Desktop Entry]\nName=No Exec Here\n";
        assert!(parse_desktop_entry("no-exec", contents).is_none());
    }

    #[test]
    fn missing_name_falls_back_to_the_id() {
        let contents = "[Desktop Entry]\nExec=true\n";
        let entry = parse_desktop_entry("true-app", contents).unwrap();
        assert_eq!(entry.name, "true-app");
    }

    #[test]
    fn only_the_desktop_entry_section_is_read() {
        let contents =
            "[Desktop Action foo]\nExec=should-not-be-seen\n\n[Desktop Entry]\nExec=real-exec\n";
        let entry = parse_desktop_entry("x", contents).unwrap();
        assert_eq!(entry.exec, "real-exec");
    }

    #[test]
    fn terminal_no_display_and_hidden_flags_parse() {
        let contents = "[Desktop Entry]\nExec=vim %f\nTerminal=true\nNoDisplay=true\nHidden=true\n";
        let entry = parse_desktop_entry("vim", contents).unwrap();
        assert!(entry.terminal);
        assert!(entry.no_display);
        assert!(entry.hidden);
    }

    // ── `mimeapps.list` parsing/merging ─────────────────────────────────

    #[test]
    fn parses_default_and_added_associations() {
        let file = parse_mimeapps_list(
            "[Default Applications]\ntext/plain=nvim.desktop\n\n[Added Associations]\ntext/plain=nvim.desktop;vscode.desktop;\n",
        );
        assert_eq!(
            file.default_applications.get("text/plain"),
            Some(&vec!["nvim.desktop".to_owned()])
        );
        assert_eq!(
            file.added_associations.get("text/plain"),
            Some(&vec![
                "nvim.desktop".to_owned(),
                "vscode.desktop".to_owned()
            ])
        );
    }

    #[test]
    fn merge_prefers_earlier_files_default_applications_first() {
        let high = parse_mimeapps_list("[Default Applications]\ntext/plain=nvim.desktop\n");
        let low = parse_mimeapps_list(
            "[Default Applications]\ntext/plain=vscode.desktop\n[Added Associations]\ntext/plain=gedit.desktop\n",
        );
        let merged = merge_associations(&[high, low]);
        assert_eq!(
            merged.get("text/plain"),
            Some(&vec![
                "nvim.desktop".to_owned(),
                "vscode.desktop".to_owned(),
                "gedit.desktop".to_owned(),
            ])
        );
    }

    #[test]
    fn removed_associations_veto_an_id_across_the_chain() {
        let high = parse_mimeapps_list("[Removed Associations]\ntext/plain=vscode.desktop\n");
        let low =
            parse_mimeapps_list("[Default Applications]\ntext/plain=vscode.desktop;nvim.desktop\n");
        let merged = merge_associations(&[high, low]);
        assert_eq!(
            merged.get("text/plain"),
            Some(&vec!["nvim.desktop".to_owned()])
        );
    }

    #[test]
    fn unknown_sections_and_malformed_lines_are_skipped() {
        let file = parse_mimeapps_list(
            "[Some Unknown Section]\ntext/plain=x.desktop\n[Default Applications]\nnot-a-kv-line\ntext/plain=nvim.desktop\n",
        );
        assert!(file.default_applications.contains_key("text/plain"));
        assert_eq!(file.default_applications.len(), 1);
    }

    // ── `AppsDb::build` over a tempdir fixture ──────────────────────────

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "saola-files-apps-test-{}-{}",
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

    #[test]
    fn build_discovers_desktop_files_and_resolves_the_default() {
        let root = tempdir();
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let apps_dir = data_dir.join("applications");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&apps_dir).unwrap();

        std::fs::write(
            config_dir.join("mimeapps.list"),
            "[Default Applications]\ntext/plain=nvim.desktop\n",
        )
        .unwrap();
        std::fs::write(
            apps_dir.join("nvim.desktop"),
            "[Desktop Entry]\nName=Neovim\nExec=nvim %f\nMimeType=text/plain;\n",
        )
        .unwrap();

        let db = AppsDb::build(&[config_dir], &[data_dir]);
        let default = db.default_for("text/plain").expect("should resolve nvim");
        assert_eq!(default.name, "Neovim");
        assert_eq!(db.entry("nvim.desktop").unwrap().name, "Neovim");

        cleanup(root);
    }

    #[test]
    fn build_discovers_one_level_of_vendor_subdirectory() {
        let root = tempdir();
        let data_dir = root.join("data");
        let apps_dir = data_dir.join("applications");
        let vendor_dir = apps_dir.join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        std::fs::write(
            vendor_dir.join("tool.desktop"),
            "[Desktop Entry]\nName=Vendor Tool\nExec=vendortool %f\n",
        )
        .unwrap();

        let db = AppsDb::build(&[], &[data_dir]);
        assert_eq!(db.entry("vendor-tool.desktop").unwrap().name, "Vendor Tool");

        cleanup(root);
    }

    #[test]
    fn higher_priority_data_dir_shadows_a_same_id_entry() {
        let root = tempdir();
        let high = root.join("high/applications");
        let low = root.join("low/applications");
        std::fs::create_dir_all(&high).unwrap();
        std::fs::create_dir_all(&low).unwrap();
        std::fs::write(
            high.join("editor.desktop"),
            "[Desktop Entry]\nName=High Priority Editor\nExec=high-editor %f\n",
        )
        .unwrap();
        std::fs::write(
            low.join("editor.desktop"),
            "[Desktop Entry]\nName=Low Priority Editor\nExec=low-editor %f\n",
        )
        .unwrap();

        let db = AppsDb::build(&[], &[root.join("high"), root.join("low")]);
        assert_eq!(
            db.entry("editor.desktop").unwrap().name,
            "High Priority Editor"
        );

        cleanup(root);
    }

    #[test]
    fn candidates_for_lists_the_default_first_then_other_matching_apps() {
        let root = tempdir();
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let apps_dir = data_dir.join("applications");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&apps_dir).unwrap();
        std::fs::write(
            config_dir.join("mimeapps.list"),
            "[Default Applications]\ntext/plain=nvim.desktop\n",
        )
        .unwrap();
        std::fs::write(
            apps_dir.join("nvim.desktop"),
            "[Desktop Entry]\nName=Neovim\nExec=nvim %f\nMimeType=text/plain;\n",
        )
        .unwrap();
        std::fs::write(
            apps_dir.join("gedit.desktop"),
            "[Desktop Entry]\nName=Gedit\nExec=gedit %f\nMimeType=text/plain;\n",
        )
        .unwrap();
        std::fs::write(
            apps_dir.join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden Editor\nExec=hidden %f\nMimeType=text/plain;\nNoDisplay=true\n",
        )
        .unwrap();

        let db = AppsDb::build(&[config_dir], &[data_dir]);
        let names: Vec<&str> = db
            .candidates_for("text/plain")
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["Neovim", "Gedit"]); // default first, then alphabetized, hidden excluded

        cleanup(root);
    }

    #[test]
    fn missing_directories_degrade_to_an_empty_db_not_an_error() {
        let db = AppsDb::build(
            &[PathBuf::from("/nonexistent-saola-files-config")],
            &[PathBuf::from("/nonexistent-saola-files-data")],
        );
        assert!(db.default_for("text/plain").is_none());
        assert!(db.candidates_for("text/plain").is_empty());
    }

    // ── XDG dir chain ────────────────────────────────────────────────────

    #[test]
    fn xdg_home_vars_take_precedence_over_defaults() {
        let (configs, data) = resolve_xdg_dirs(
            Some(OsString::from("/x/config")),
            None,
            Some(OsString::from("/x/data")),
            None,
            Some(OsString::from("/home/j")),
        );
        assert_eq!(configs[0], PathBuf::from("/x/config"));
        assert_eq!(configs[1], PathBuf::from("/etc/xdg"));
        assert_eq!(data[0], PathBuf::from("/x/data"));
        assert_eq!(data[1], PathBuf::from("/usr/local/share"));
        assert_eq!(data[2], PathBuf::from("/usr/share"));
    }

    #[test]
    fn empty_env_vars_count_as_unset() {
        let (configs, _data) = resolve_xdg_dirs(
            Some(OsString::from("")),
            None,
            None,
            None,
            Some(OsString::from("/home/j")),
        );
        assert_eq!(configs[0], PathBuf::from("/home/j/.config"));
    }

    #[test]
    fn no_home_still_yields_the_system_wide_dirs() {
        let (configs, data) = resolve_xdg_dirs(None, None, None, None, None);
        assert_eq!(configs, vec![PathBuf::from("/etc/xdg")]);
        assert_eq!(
            data,
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
    }

    // ── `Exec=` field-code expansion ────────────────────────────────────

    #[test]
    fn tokenizes_plain_and_quoted_words() {
        assert_eq!(
            tokenize_exec("nvim %f --flag"),
            vec!["nvim", "%f", "--flag"]
        );
        assert_eq!(
            tokenize_exec(r#""/opt/my app/bin" %f"#),
            vec!["/opt/my app/bin", "%f"]
        );
    }

    #[test]
    fn tokenize_unescapes_quoted_backslash_sequences() {
        assert_eq!(
            tokenize_exec(r#"echo "say \"hi\"""#),
            vec!["echo", "say \"hi\""]
        );
    }

    #[test]
    fn singular_field_code_spawns_once_per_target() {
        let targets = vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")];
        let argv = build_argv("nvim %f", &targets);
        assert_eq!(argv.len(), 2);
        assert_eq!(
            argv[0],
            vec![OsString::from("nvim"), OsString::from("/a.txt")]
        );
        assert_eq!(
            argv[1],
            vec![OsString::from("nvim"), OsString::from("/b.txt")]
        );
    }

    #[test]
    fn list_field_code_spawns_once_with_every_target() {
        let targets = vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")];
        let argv = build_argv("code %F", &targets);
        assert_eq!(argv.len(), 1);
        assert_eq!(
            argv[0],
            vec![
                OsString::from("code"),
                OsString::from("/a.txt"),
                OsString::from("/b.txt"),
            ]
        );
    }

    #[test]
    fn no_field_code_spawns_once_with_targets_ignored() {
        let targets = vec![PathBuf::from("/a.txt")];
        let argv = build_argv("htop", &targets);
        assert_eq!(argv, vec![vec![OsString::from("htop")]]);
    }

    #[test]
    fn url_field_code_percent_encodes_spaces() {
        let targets = vec![PathBuf::from("/a dir/b.txt")];
        let argv = build_argv("open %u", &targets);
        assert_eq!(
            argv[0],
            vec![
                OsString::from("open"),
                OsString::from("file:///a%20dir/b.txt"),
            ]
        );
    }

    #[test]
    fn deprecated_and_meta_field_codes_are_dropped() {
        let argv = build_argv("app %i --name %c %k", &[]);
        assert_eq!(
            argv,
            vec![vec![OsString::from("app"), OsString::from("--name")]]
        );
    }

    #[test]
    fn literal_percent_percent_unescapes_to_one_percent() {
        let argv = build_argv("printf 100%%", &[]);
        assert_eq!(
            argv,
            vec![vec![OsString::from("printf"), OsString::from("100%")]]
        );
    }

    #[test]
    fn empty_exec_yields_no_spawns() {
        assert!(build_argv("   ", &[]).is_empty());
    }

    // ── terminal resolution ─────────────────────────────────────────────

    #[test]
    fn config_terminal_wins_over_env_and_default() {
        assert_eq!(
            resolve_terminal(Some("foot"), Some(OsStr::new("xterm"))),
            OsString::from("foot")
        );
    }

    #[test]
    fn env_terminal_wins_over_default() {
        assert_eq!(
            resolve_terminal(None, Some(OsStr::new("xterm"))),
            OsString::from("xterm")
        );
    }

    #[test]
    fn falls_back_to_alacritty() {
        assert_eq!(resolve_terminal(None, None), OsString::from("alacritty"));
    }

    #[test]
    fn empty_config_and_env_values_count_as_unset() {
        assert_eq!(
            resolve_terminal(Some(""), Some(OsStr::new(""))),
            OsString::from("alacritty")
        );
    }
}
