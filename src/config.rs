//! `files.toml` — saola-files' configuration.
//!
//! Follows saola-capture's config conventions exactly (the newest precedent
//! in the family, decided with Jordan 2026-08-08): TOML, bare top-level
//! kebab-case keys with no wrapper table, hand-walked with `toml::Table`
//! rather than `#[derive(Deserialize)]` so each bad knob can warn by name
//! and fall back alone.
//!
//! Resilience rules (binding, from CLAUDE.md):
//! - no file → silent defaults;
//! - unparseable file → one `eprintln!` naming the path, then all defaults
//!   — the app still starts;
//! - one bad knob → warn on that knob only, keep every other knob.
//!
//! Directory resolution chain, shared with every sibling:
//! `--config-dir` > `$SAOLA_CONFIG_DIR` > `$XDG_CONFIG_HOME/saola` >
//! `~/.config/saola`, where an env var set to the empty string counts as
//! unset (the XDG spec's own rule, applied uniformly).
//!
//! Two array-of-tables sections extend the flat knobs:
//! - `[[action]]` — user-defined context-menu actions (name + exec, with
//!   optional mimetype/scheme filters). Consumed by the menu layer in
//!   Stage 6.
//! - `[[server]]` — saved remote locations for the places sidebar
//!   (Stage 7) and the SFTP connect flow (Stage 13).
//!
//! ```toml
//! terminal = "alacritty"
//! surface = "ink"
//! view = "grid"
//! sort = "modified"
//! sort-descending = true
//! show-hidden = false
//! thumbnails = true
//! thumbnail-max-mb = 64
//! confirm-empty-trash = true
//!
//! [[action]]
//! name = "Edit as text"
//! exec = "alacritty -e nvim %f"
//! mimetypes = ["text/*"]
//!
//! [[server]]
//! name = "homelab"
//! uri = "sftp://jordan@10.0.0.10/srv"
//! ```

use std::path::{Path, PathBuf};

use toml::Table;

/// Default cap on the size of files we'll generate thumbnails for, in MiB.
const DEFAULT_THUMBNAIL_MAX_MB: u64 = 64;

/// Which of the design language's two grounds the *window* is drawn on:
/// ivory paper (the default) or ink. A local enum rather than
/// `saola_theme::Surface` on purpose — this module stays std+toml-only, so
/// the theme crate never leaks into config parsing; `main.rs` converts it
/// to a `Surface` once at startup.
///
/// The knob moves the window chrome and everything anchored to it (header,
/// sidebar, toolbar, listing). It deliberately does **not** move the
/// popovers/context menus (always ink), the undo toast (always ink) or the
/// four modal dialog cards (always ivory) — those surfaces are pinned by
/// the style guide, not by taste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowSurface {
    #[default]
    Paper,
    Ink,
}

impl WindowSurface {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "paper" => Some(Self::Paper),
            "ink" => Some(Self::Ink),
            _ => None,
        }
    }
}

/// How a directory is presented. Lives here because it's a config knob;
/// the directory view (Stage 3) consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    List,
    Grid,
}

impl View {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "list" => Some(Self::List),
            "grid" => Some(Self::Grid),
            _ => None,
        }
    }
}

/// Which column a directory sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

impl SortKey {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "name" => Some(Self::Name),
            "size" => Some(Self::Size),
            "modified" => Some(Self::Modified),
            "type" => Some(Self::Type),
            _ => None,
        }
    }
}

/// One `[[action]]` entry: a user-defined context-menu command.
///
/// `exec` uses desktop-entry `%f`/`%F` field codes (expanded by the apps
/// layer in Stage 6). Empty `mimetypes`/`schemes` means "applies to all".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAction {
    pub name: String,
    pub exec: String,
    pub mimetypes: Vec<String>,
    pub schemes: Vec<String>,
}

/// One `[[server]]` entry: a saved remote location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedServer {
    pub name: String,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Terminal emulator for "Open in terminal" and `Terminal=true` desktop
    /// entries. `None` falls through to `$TERMINAL`, then `alacritty` — the
    /// chain lives at the point of use (core/apps.rs, Stage 6), not here.
    pub terminal: Option<String>,
    /// Which ground the window draws on. See [`WindowSurface`].
    pub surface: WindowSurface,
    pub view: View,
    pub sort: SortKey,
    pub sort_descending: bool,
    pub show_hidden: bool,
    pub thumbnails: bool,
    pub thumbnail_max_mb: u64,
    pub confirm_empty_trash: bool,
    pub actions: Vec<CustomAction>,
    pub servers: Vec<SavedServer>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal: None,
            surface: WindowSurface::default(),
            view: View::default(),
            sort: SortKey::default(),
            sort_descending: false,
            show_hidden: false,
            thumbnails: true,
            thumbnail_max_mb: DEFAULT_THUMBNAIL_MAX_MB,
            confirm_empty_trash: true,
            actions: Vec::new(),
            servers: Vec::new(),
        }
    }
}

/// The file couldn't be parsed as TOML at all. Carries toml's own message so
/// the one warning we print can say *why*.
#[derive(Debug)]
pub struct ConfigError(toml::de::Error);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Config {
    /// Where `files.toml` lives, resolving the environment at this one thin
    /// wrapper (never inside the logic — the no-`set_var`-in-tests rule).
    /// `None` means "no config is possible here" (no `$HOME`, e.g. CI),
    /// which loads pure defaults rather than being an error.
    pub fn resolve_path(cli_config_dir: Option<&Path>) -> Option<PathBuf> {
        config_dir_from(
            cli_config_dir,
            std::env::var_os("SAOLA_CONFIG_DIR"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .map(|dir| dir.join("files.toml"))
    }

    /// Load the config at boot. Never fails: every error path warns via
    /// `eprintln!` and returns a value.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let contents = match std::fs::read_to_string(path) {
            // Missing file (the common case) and unreadable file both
            // degrade silently to defaults.
            Err(_) => return Self::default(),
            Ok(contents) => contents,
        };
        match Self::parse(&contents) {
            Ok(config) => config,
            Err(err) => {
                eprintln!(
                    "saola-files: {} is not valid TOML ({err}) — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Parse a `files.toml` document. Returns `Err` **only** for invalid
    /// TOML; every lesser problem warns and defaults per knob. This is the
    /// function the unit tests exercise directly.
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let body: Table = contents.parse().map_err(ConfigError)?;
        let defaults = Self::default();

        let terminal = read_str(&body, "terminal").map(str::to_owned);

        let surface = read_str(&body, "surface")
            .and_then(|value| match_or_warn(value, "surface", WindowSurface::parse))
            .unwrap_or_default();

        let view = read_str(&body, "view")
            .and_then(|value| match_or_warn(value, "view", View::parse))
            .unwrap_or_default();

        let sort = read_str(&body, "sort")
            .and_then(|value| match_or_warn(value, "sort", SortKey::parse))
            .unwrap_or_default();

        let sort_descending = read_bool(&body, "sort-descending").unwrap_or(false);
        let show_hidden = read_bool(&body, "show-hidden").unwrap_or(false);
        let thumbnails = read_bool(&body, "thumbnails").unwrap_or(true);
        let thumbnail_max_mb =
            read_u64(&body, "thumbnail-max-mb").unwrap_or(defaults.thumbnail_max_mb);
        let confirm_empty_trash = read_bool(&body, "confirm-empty-trash").unwrap_or(true);

        let actions = read_actions(&body);
        let servers = read_servers(&body);

        Ok(Config {
            terminal,
            surface,
            view,
            sort,
            sort_descending,
            show_hidden,
            thumbnails,
            thumbnail_max_mb,
            confirm_empty_trash,
            actions,
            servers,
        })
    }
}

/// The testable core of [`Config::resolve_path`]'s directory chain: every
/// environment variable arrives as a plain argument, so precedence is
/// unit-testable without touching the process environment. An env var set
/// to the **empty string** is treated as unset and falls through.
fn config_dir_from(
    cli: Option<&Path>,
    saola: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = cli {
        return Some(dir.to_path_buf());
    }
    if let Some(saola) = saola
        && !saola.is_empty()
    {
        return Some(PathBuf::from(saola));
    }
    if let Some(xdg) = xdg
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("saola"));
    }
    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/saola"))
}

/// Applies `parser` to `value`; on `None`, warns (naming the knob and the
/// value that didn't match) and returns `None` so the caller falls back to
/// that knob's default.
fn match_or_warn<T>(value: &str, knob: &str, parser: fn(&str) -> Option<T>) -> Option<T> {
    let parsed = parser(value);
    if parsed.is_none() {
        eprintln!("saola-files: files.toml: unrecognized {knob} \"{value}\" — using default");
    }
    parsed
}

/// `table.get(name)` as a string. A key present but holding a non-string
/// value falls through to `None` — same fallback path as a missing key.
fn read_str<'a>(table: &'a Table, name: &str) -> Option<&'a str> {
    table.get(name)?.as_str()
}

/// `table.get(name)` as a bool. Present-but-non-boolean warns and defaults.
fn read_bool(table: &Table, name: &str) -> Option<bool> {
    let value = table.get(name)?;
    let parsed = value.as_bool();
    if parsed.is_none() {
        eprintln!("saola-files: files.toml: {name} must be true or false — using default");
    }
    parsed
}

/// `table.get(name)` as a non-negative integer. Present-but-wrong-shape
/// (a string, a float, a negative) warns and defaults.
fn read_u64(table: &Table, name: &str) -> Option<u64> {
    let value = table.get(name)?;
    let parsed = value.as_integer().and_then(|n| u64::try_from(n).ok());
    if parsed.is_none() {
        eprintln!("saola-files: files.toml: {name} must be a non-negative integer — using default");
    }
    parsed
}

/// A `[[section]]` array of tables. TOML guarantees `[[x]]` parses as an
/// array of tables, but a scalar `x = …` also occupies the same key — that
/// shape warns once and yields nothing.
fn read_entries<'a>(table: &'a Table, section: &str) -> Vec<&'a Table> {
    let Some(value) = table.get(section) else {
        return Vec::new();
    };
    let Some(array) = value.as_array() else {
        eprintln!(
            "saola-files: files.toml: {section} must be written as [[{section}]] sections — ignoring"
        );
        return Vec::new();
    };
    // Non-table members can't actually be produced by [[section]] syntax
    // (that would be `section = [1, 2]`), so silently skipping them is the
    // same "absent knob" path as a missing key.
    array.iter().filter_map(|entry| entry.as_table()).collect()
}

/// A list-of-strings field inside an entry (`mimetypes = ["text/*"]`).
/// Wrong-shaped members warn by (section, name) and are skipped.
fn read_str_list(entry: &Table, section: &str, name: &str, field: &str) -> Vec<String> {
    let Some(value) = entry.get(field) else {
        return Vec::new();
    };
    let Some(array) = value.as_array() else {
        eprintln!(
            "saola-files: files.toml: [[{section}]] \"{name}\": {field} must be an array of strings — ignoring it"
        );
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|member| {
            let member = member.as_str();
            if member.is_none() {
                eprintln!(
                    "saola-files: files.toml: [[{section}]] \"{name}\": {field} entries must be strings — skipping one"
                );
            }
            member.map(str::to_owned)
        })
        .collect()
}

/// Every valid `[[action]]` entry. An entry missing `name` or `exec` is
/// skipped with a warning naming what's missing; the rest survive.
fn read_actions(body: &Table) -> Vec<CustomAction> {
    read_entries(body, "action")
        .into_iter()
        .filter_map(|entry| {
            let name = match read_str(entry, "name") {
                Some(name) => name.to_owned(),
                None => {
                    eprintln!(
                        "saola-files: files.toml: an [[action]] is missing its name — skipping it"
                    );
                    return None;
                }
            };
            let Some(exec) = read_str(entry, "exec") else {
                eprintln!(
                    "saola-files: files.toml: [[action]] \"{name}\" has no exec — skipping it"
                );
                return None;
            };
            Some(CustomAction {
                mimetypes: read_str_list(entry, "action", &name, "mimetypes"),
                schemes: read_str_list(entry, "action", &name, "schemes"),
                exec: exec.to_owned(),
                name,
            })
        })
        .collect()
}

/// Every valid `[[server]]` entry; missing `name` or `uri` skips that entry.
fn read_servers(body: &Table) -> Vec<SavedServer> {
    read_entries(body, "server")
        .into_iter()
        .filter_map(|entry| {
            let name = match read_str(entry, "name") {
                Some(name) => name.to_owned(),
                None => {
                    eprintln!(
                        "saola-files: files.toml: a [[server]] is missing its name — skipping it"
                    );
                    return None;
                }
            };
            let Some(uri) = read_str(entry, "uri") else {
                eprintln!(
                    "saola-files: files.toml: [[server]] \"{name}\" has no uri — skipping it"
                );
                return None;
            };
            Some(SavedServer {
                uri: uri.to_owned(),
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    // ── directory chain ─────────────────────────────────────────────────

    #[test]
    fn cli_dir_wins_over_everything() {
        let dir = config_dir_from(
            Some(Path::new("/cli")),
            os("/saola"),
            os("/xdg"),
            os("/home/j"),
        );
        assert_eq!(dir, Some(PathBuf::from("/cli")));
    }

    #[test]
    fn saola_config_dir_beats_xdg() {
        let dir = config_dir_from(None, os("/saola"), os("/xdg"), os("/home/j"));
        assert_eq!(dir, Some(PathBuf::from("/saola")));
    }

    #[test]
    fn xdg_gets_saola_suffix() {
        let dir = config_dir_from(None, None, os("/xdg"), os("/home/j"));
        assert_eq!(dir, Some(PathBuf::from("/xdg/saola")));
    }

    #[test]
    fn home_fallback_and_empty_vars_count_as_unset() {
        let dir = config_dir_from(None, os(""), os(""), os("/home/j"));
        assert_eq!(dir, Some(PathBuf::from("/home/j/.config/saola")));
    }

    #[test]
    fn no_home_means_no_config() {
        assert_eq!(config_dir_from(None, None, None, os("")), None);
    }

    // ── whole-file resilience ───────────────────────────────────────────

    #[test]
    fn empty_document_is_all_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn invalid_toml_is_the_only_hard_error() {
        assert!(Config::parse("view = ").is_err());
    }

    #[test]
    fn missing_file_loads_defaults() {
        let config = Config::load(Some(Path::new("/nonexistent/files.toml")));
        assert_eq!(config, Config::default());
    }

    #[test]
    fn no_possible_path_loads_defaults() {
        assert_eq!(Config::load(None), Config::default());
    }

    // ── per-knob degradation: every knob, good and bad ──────────────────

    #[test]
    fn terminal_knob() {
        let config = Config::parse("terminal = \"foot\"").unwrap();
        assert_eq!(config.terminal.as_deref(), Some("foot"));
        // Wrong type falls through like an absent knob.
        assert_eq!(Config::parse("terminal = 3").unwrap().terminal, None);
    }

    #[test]
    fn surface_knob() {
        assert_eq!(
            Config::parse("surface = \"ink\"").unwrap().surface,
            WindowSurface::Ink
        );
        // Unrecognized value warns and keeps the default; the rest of the
        // document still parses.
        let config = Config::parse("surface = \"vellum\"\nshow-hidden = true").unwrap();
        assert_eq!(config.surface, WindowSurface::Paper);
        assert!(config.show_hidden);
    }

    #[test]
    fn view_knob() {
        assert_eq!(Config::parse("view = \"grid\"").unwrap().view, View::Grid);
        // Unrecognized value warns and keeps the default; the rest of the
        // document still parses.
        let config = Config::parse("view = \"mosaic\"\nshow-hidden = true").unwrap();
        assert_eq!(config.view, View::List);
        assert!(config.show_hidden);
    }

    #[test]
    fn sort_knobs() {
        let config = Config::parse("sort = \"modified\"\nsort-descending = true").unwrap();
        assert_eq!(config.sort, SortKey::Modified);
        assert!(config.sort_descending);
        assert_eq!(
            Config::parse("sort = \"bogosort\"").unwrap().sort,
            SortKey::Name
        );
        // Non-boolean warns and defaults.
        assert!(
            !Config::parse("sort-descending = \"yes\"")
                .unwrap()
                .sort_descending
        );
    }

    #[test]
    fn thumbnail_knobs() {
        let config = Config::parse("thumbnails = false\nthumbnail-max-mb = 128").unwrap();
        assert!(!config.thumbnails);
        assert_eq!(config.thumbnail_max_mb, 128);
        // A negative cap is wrong-shaped, not "zero": warn and default.
        assert_eq!(
            Config::parse("thumbnail-max-mb = -1")
                .unwrap()
                .thumbnail_max_mb,
            DEFAULT_THUMBNAIL_MAX_MB
        );
    }

    #[test]
    fn confirm_empty_trash_knob() {
        assert!(
            !Config::parse("confirm-empty-trash = false")
                .unwrap()
                .confirm_empty_trash
        );
    }

    // ── [[action]] ──────────────────────────────────────────────────────

    #[test]
    fn actions_parse_with_filters() {
        let config = Config::parse(
            r#"
            [[action]]
            name = "Edit as text"
            exec = "alacritty -e nvim %f"
            mimetypes = ["text/*", "application/json"]

            [[action]]
            name = "Checksum"
            exec = "sha256sum %F"
            "#,
        )
        .unwrap();
        assert_eq!(config.actions.len(), 2);
        assert_eq!(config.actions[0].mimetypes.len(), 2);
        // Absent filters mean "applies everywhere".
        assert!(config.actions[1].mimetypes.is_empty());
        assert!(config.actions[1].schemes.is_empty());
    }

    #[test]
    fn broken_action_entries_are_skipped_not_fatal() {
        let config = Config::parse(
            r#"
            [[action]]
            name = "No exec here"

            [[action]]
            name = "Survivor"
            exec = "true"
            mimetypes = "text/*"
            "#,
        )
        .unwrap();
        // First entry lacks exec (skipped); second survives, with its
        // wrong-shaped mimetypes (a bare string, not an array) ignored.
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.actions[0].name, "Survivor");
        assert!(config.actions[0].mimetypes.is_empty());
    }

    #[test]
    fn action_as_scalar_is_ignored() {
        let config = Config::parse("action = \"not a section\"").unwrap();
        assert!(config.actions.is_empty());
    }

    // ── [[server]] ──────────────────────────────────────────────────────

    #[test]
    fn servers_parse_and_broken_entries_skip() {
        let config = Config::parse(
            r#"
            [[server]]
            name = "homelab"
            uri = "sftp://jordan@10.0.0.10/srv"

            [[server]]
            name = "no uri"
            "#,
        )
        .unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].uri, "sftp://jordan@10.0.0.10/srv");
    }
}
