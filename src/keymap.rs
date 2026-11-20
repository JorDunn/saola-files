//! `(Key, Modifiers) -> Action` resolution.
//!
//! AGENTS.md's messages rule: "keyboard input resolves through `keymap.rs`
//! to an `Action` enum — `update` consumes Actions, never raw key events."
//! [`Keymap::resolve`] is the one place raw `iced::keyboard::Key`/
//! `Modifiers` values get matched on; everything downstream
//! (`ui::dirview`'s `update`) only ever sees an [`Action`].
//!
//! The mapping is data, not code: [`BINDINGS`] has one row per action
//! (stable config name, default chords, catalogue label). `files.toml`'s
//! `[keys]` section overrides rows by name ([`Keymap::from_config`]).
//!
//! Deliberately *outside* the table, and not rebindable: text editing inside
//! inputs (the focused `text_input` consumes those keys itself), Escape
//! closing the popover/rename/path editor in front, and type-ahead letters
//! (any bare printable key not claimed by a row).

use std::collections::HashMap;

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

/// A keyboard command the active directory view can act on. Deliberately
/// UI-shaped ("move the cursor down"), not "what key was pressed" — no
/// caller past [`Keymap::resolve`] sees a `Key`/`Modifiers` again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MoveCursorUp,
    MoveCursorDown,
    /// Grid view's column stepping (Stage 4: `ui::dirview::grid`); a no-op
    /// in list view, which has no notion of "the next column over".
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorHome,
    MoveCursorEnd,
    MoveCursorPageUp,
    MoveCursorPageDown,
    /// Same keys with Shift held: extend the selection to the new cursor
    /// position (anchored at the last plain click/move) instead of moving
    /// a bare cursor.
    ExtendSelectionUp,
    ExtendSelectionDown,
    ExtendSelectionLeft,
    ExtendSelectionRight,
    ExtendSelectionHome,
    ExtendSelectionEnd,
    ExtendSelectionPageUp,
    ExtendSelectionPageDown,
    /// Ctrl+Space: toggle the cursor row's membership without touching the
    /// rest of the selection. Kept distinct from Ctrl+click, which
    /// `ui::dirview::selection` handles directly from the click message.
    ToggleCursorSelected,
    SelectAll,
    /// Enter: open the selection (descend into a lone selected directory,
    /// or activate the selected file(s)).
    Descend,
    /// Alt+Up or Backspace: navigate to the parent directory.
    Ascend,
    ToggleHidden,
    /// Alt+Left: step back through this view's own history — the
    /// browser-style back button (`DirectoryView`'s back/forward stacks,
    /// Stage 4). Handled entirely inside the view (unlike `Ascend`, which
    /// bubbles an `Event::OpenDirectory` for the owner to act on): back/
    /// forward is inherently per-view, never a "maybe open in a new tab"
    /// situation the way descending into a row is.
    HistoryBack,
    /// Alt+Right: the redo side of [`Action::HistoryBack`].
    HistoryForward,
    /// Ctrl+L: swap the breadcrumb trail for an editable path/URI
    /// `text_input`, focused with its text selected.
    EditPath,
    /// Ctrl+1: switch this view to the list presentation.
    SetViewList,
    /// Ctrl+2: switch this view to the grid presentation.
    SetViewGrid,
    /// F5: re-list the current directory — the manual fallback for
    /// backends that can't signal changes themselves (`Caps::WATCH`
    /// unset).
    Refresh,

    // ── Stage 8: clipboard, rename, new folder ──────────────────────────
    /// Ctrl+C: copy the current selection to the internal clipboard.
    Copy,
    /// Ctrl+X: cut the current selection to the internal clipboard.
    Cut,
    /// Ctrl+V: paste the internal clipboard into the current directory.
    Paste,
    /// F2: start inline-renaming the cursor row (only meaningful with
    /// exactly one entry selected — `DirectoryView::apply_action` decides
    /// that, not this module).
    Rename,
    /// Ctrl+Shift+N: create a new folder in the current directory.
    NewFolder,

    // ── Stage 9: trash / permanent delete ───────────────────────────────
    /// Delete: trash the selection where the backend supports it
    /// (`Caps::TRASH`), permanent delete worded as such otherwise —
    /// `DirectoryView`/`App` decide which, not this module (see
    /// `core::fs::trash`).
    Delete,
    /// Shift+Delete: always a permanent delete, regardless of
    /// `Caps::TRASH`.
    PermanentDelete,

    // ── Stage 10: undo ────────────────────────────────────────────────────
    /// Ctrl+Z: pop and invert the most recent invertible op — see
    /// `core::fs::undo`'s module doc comment for exactly what's invertible.
    Undo,

    // ── Stage 13: properties ────────────────────────────────────────────
    /// Alt+Enter: open the properties dialog for the current selection —
    /// `DirectoryView`/`App` decide what that shows (name, mime, size,
    /// dates, permissions), not this module. Deliberately Alt+Enter, not
    /// plain Enter (already `Action::Descend`) or F2 (already
    /// `Action::Rename`) — matches the mainstream file-manager convention
    /// this style guide follows for "get info".
    Properties,

    /// An app-level command: something the window (not one directory
    /// view) carries out, such as a future "new tab". `DirectoryView`
    /// never handles these itself; it bubbles them unchanged as
    /// `dirview::Event::App` so `App` can act on them. Nesting them here
    /// keeps `Action` from growing a variant per app feature — a feature
    /// adds its variant to [`AppAction`] instead.
    App(AppAction),
    // --- features (alphabetical) ---
    /// Menu key or Shift+F10: open the context menu at the cursor row.
    OpenContextMenu,
}

/// Keyboard commands that belong to the whole app rather than to one
/// directory view (see [`Action::App`]).
///
/// Empty for now: every shortcut that exists today acts on a directory
/// view. An enum with no variants is a type with no values, so nothing can
/// construct an `Action::App` yet, and `App`'s handler is the empty match
/// `match action {}` — the compiler accepts that as exhaustive because
/// there is no value it could ever be called with. The first feature that
/// adds a variant here turns that match into an ordinary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
//
// When a variant is added: give it a `BINDINGS` row and extend the exhaustive
// `match` in `coverage` (keymap tests), or `Keymap::resolve` can never
// produce it.
pub enum AppAction {
    // --- features (alphabetical) ---
}

// ── The binding table ───────────────────────────────────────────────────

/// One row of the keymap: an [`Action`], its stable config name, its
/// default chord(s) and the catalogue id of its human label.
///
/// This table is the single source of truth for shortcuts. The `[keys]`
/// section of `files.toml` reads it (name -> chords), [`Keymap::resolve`]
/// reads it (chord -> action), and a future shortcut editor/preferences
/// dialog reads it (label + current chords). A feature adds a shortcut by
/// adding one row to [`BINDINGS`] and one `keys-<name>` message to the
/// catalogue; nothing else in this module changes.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// What a matching key press resolves to.
    pub action: Action,
    /// Stable snake_case name used as the key in `[keys]`. Never rename one
    /// once released: it is part of users' config files.
    pub name: &'static str,
    /// Id of the human-readable label in `i18n/en-US/files.ftl`.
    pub label: &'static str,
    /// Default chords, in the `[keys]` grammar (see [`Chord::parse`]). A
    /// test checks that every one parses, round-trips, and that no two rows
    /// share a chord.
    pub defaults: &'static [&'static str],
}

impl Binding {
    /// The translated label, for a shortcut editor or help overlay.
    pub fn label_text(&self) -> String {
        crate::i18n::lookup(self.label, None)
    }
}

/// Shorthand so a table row stays on a line or two.
const fn row(
    action: Action,
    name: &'static str,
    label: &'static str,
    defaults: &'static [&'static str],
) -> Binding {
    Binding {
        action,
        name,
        label,
        defaults,
    }
}

/// Every rebindable shortcut. Order is display order for a future editor;
/// it has no effect on resolution.
pub const BINDINGS: &[Binding] = &[
    // Cursor movement.
    row(
        Action::MoveCursorUp,
        "move_cursor_up",
        "keys-move-cursor-up",
        &["up"],
    ),
    row(
        Action::MoveCursorDown,
        "move_cursor_down",
        "keys-move-cursor-down",
        &["down"],
    ),
    row(
        Action::MoveCursorLeft,
        "move_cursor_left",
        "keys-move-cursor-left",
        &["left"],
    ),
    row(
        Action::MoveCursorRight,
        "move_cursor_right",
        "keys-move-cursor-right",
        &["right"],
    ),
    row(
        Action::MoveCursorHome,
        "move_cursor_home",
        "keys-move-cursor-home",
        &["home"],
    ),
    row(
        Action::MoveCursorEnd,
        "move_cursor_end",
        "keys-move-cursor-end",
        &["end"],
    ),
    row(
        Action::MoveCursorPageUp,
        "move_cursor_page_up",
        "keys-move-cursor-page-up",
        &["pageup"],
    ),
    row(
        Action::MoveCursorPageDown,
        "move_cursor_page_down",
        "keys-move-cursor-page-down",
        &["pagedown"],
    ),
    // Selection.
    row(
        Action::ExtendSelectionUp,
        "extend_selection_up",
        "keys-extend-selection-up",
        &["shift+up"],
    ),
    row(
        Action::ExtendSelectionDown,
        "extend_selection_down",
        "keys-extend-selection-down",
        &["shift+down"],
    ),
    row(
        Action::ExtendSelectionLeft,
        "extend_selection_left",
        "keys-extend-selection-left",
        &["shift+left"],
    ),
    row(
        Action::ExtendSelectionRight,
        "extend_selection_right",
        "keys-extend-selection-right",
        &["shift+right"],
    ),
    row(
        Action::ExtendSelectionHome,
        "extend_selection_home",
        "keys-extend-selection-home",
        &["shift+home"],
    ),
    row(
        Action::ExtendSelectionEnd,
        "extend_selection_end",
        "keys-extend-selection-end",
        &["shift+end"],
    ),
    row(
        Action::ExtendSelectionPageUp,
        "extend_selection_page_up",
        "keys-extend-selection-page-up",
        &["shift+pageup"],
    ),
    row(
        Action::ExtendSelectionPageDown,
        "extend_selection_page_down",
        "keys-extend-selection-page-down",
        &["shift+pagedown"],
    ),
    row(
        Action::ToggleCursorSelected,
        "toggle_cursor_selected",
        "keys-toggle-cursor-selected",
        &["ctrl+space"],
    ),
    row(
        Action::SelectAll,
        "select_all",
        "keys-select-all",
        &["ctrl+a"],
    ),
    // Navigation.
    row(Action::Descend, "descend", "keys-descend", &["enter"]),
    row(
        Action::Ascend,
        "ascend",
        "keys-ascend",
        &["backspace", "alt+up"],
    ),
    row(
        Action::HistoryBack,
        "history_back",
        "keys-history-back",
        &["alt+left"],
    ),
    row(
        Action::HistoryForward,
        "history_forward",
        "keys-history-forward",
        &["alt+right"],
    ),
    row(Action::EditPath, "edit_path", "keys-edit-path", &["ctrl+l"]),
    row(Action::Refresh, "refresh", "keys-refresh", &["f5"]),
    // View.
    row(
        Action::ToggleHidden,
        "toggle_hidden",
        "keys-toggle-hidden",
        &["ctrl+h"],
    ),
    row(
        Action::SetViewList,
        "set_view_list",
        "keys-set-view-list",
        &["ctrl+1"],
    ),
    row(
        Action::SetViewGrid,
        "set_view_grid",
        "keys-set-view-grid",
        &["ctrl+2"],
    ),
    // File operations.
    row(Action::Copy, "copy", "keys-copy", &["ctrl+c"]),
    row(Action::Cut, "cut", "keys-cut", &["ctrl+x"]),
    row(Action::Paste, "paste", "keys-paste", &["ctrl+v"]),
    row(Action::Rename, "rename", "keys-rename", &["f2"]),
    row(
        Action::NewFolder,
        "new_folder",
        "keys-new-folder",
        &["ctrl+shift+n"],
    ),
    row(Action::Delete, "delete", "keys-delete", &["delete"]),
    row(
        Action::PermanentDelete,
        "permanent_delete",
        "keys-permanent-delete",
        &["shift+delete"],
    ),
    row(Action::Undo, "undo", "keys-undo", &["ctrl+z"]),
    row(
        Action::Properties,
        "properties",
        "keys-properties",
        &["alt+enter"],
    ),
    // --- features (alphabetical) ---
    row(
        Action::OpenContextMenu,
        "open_context_menu",
        "keys-open-context-menu",
        &["menu", "shift+menu", "shift+f10"],
    ),
];

// ── Chords ──────────────────────────────────────────────────────────────

/// The non-modifier half of a [`Chord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ChordKey {
    /// A printable key, as the *current layout* names it, lowercased.
    Char(char),
    Named(Named),
}

/// A key plus the modifiers that must be held with it, and only those.
///
/// **Logical, not physical.** iced 0.14 delivers both: `KeyPressed.key` is
/// winit's `key_without_modifiers()` (the character the layout puts on
/// that key, ignoring Shift/Ctrl, so Shift+N still reports `"n"`), and
/// `physical_key` is the key's position. We match on the logical key:
/// Ctrl+C must stay "C" on Dvorak or AZERTY, and the defaults must behave
/// exactly as they always did. The cost is that a binding to a letter
/// follows the layout, and a non-Latin layout cannot type `ctrl+n` (users
/// on such layouts bind a named key or a digit instead).
///
/// Matching is exact on Ctrl, Shift and Alt: `ctrl+a` does not fire for
/// Ctrl+Shift+A. The Logo/Super modifier is ignored, as it always was
/// (compositors usually grab it before an app sees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    key: ChordKey,
    ctrl: bool,
    shift: bool,
    alt: bool,
}

/// Canonical names for the named keys a chord can use. `Display` writes
/// these; `parse` accepts these plus [`KEY_ALIASES`].
const NAMED_KEYS: &[(&str, Named)] = &[
    ("up", Named::ArrowUp),
    ("down", Named::ArrowDown),
    ("left", Named::ArrowLeft),
    ("right", Named::ArrowRight),
    ("home", Named::Home),
    ("end", Named::End),
    ("pageup", Named::PageUp),
    ("pagedown", Named::PageDown),
    ("enter", Named::Enter),
    ("backspace", Named::Backspace),
    ("delete", Named::Delete),
    ("insert", Named::Insert),
    ("tab", Named::Tab),
    ("space", Named::Space),
    ("menu", Named::ContextMenu),
    ("escape", Named::Escape),
    ("f1", Named::F1),
    ("f2", Named::F2),
    ("f3", Named::F3),
    ("f4", Named::F4),
    ("f5", Named::F5),
    ("f6", Named::F6),
    ("f7", Named::F7),
    ("f8", Named::F8),
    ("f9", Named::F9),
    ("f10", Named::F10),
    ("f11", Named::F11),
    ("f12", Named::F12),
];

/// Alternative spellings, applied after lowercasing and removing `-`/`_`.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("arrowup", "up"),
    ("arrowdown", "down"),
    ("arrowleft", "left"),
    ("arrowright", "right"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("pgdown", "pagedown"),
    ("return", "enter"),
    ("del", "delete"),
    ("ins", "insert"),
    ("contextmenu", "menu"),
    ("esc", "escape"),
];

impl Chord {
    /// Parses the `[keys]` chord grammar: modifiers and one key joined by
    /// `+`, case-insensitive, spaces around parts ignored.
    ///
    /// * Modifiers: `ctrl` (or `control`), `shift`, `alt`, each at most once.
    /// * Key: one printable character (`n`, `1`, `,`), `plus` for `+`, or a
    ///   name from [`NAMED_KEYS`] (`up`, `pageup`, `f2`, `menu`, `space`).
    /// * A printable character needs `ctrl` or `alt`: bare letters (and
    ///   Shift+letter) belong to type-ahead.
    /// * `escape` is reserved: it always closes the popover/editor in front.
    ///
    /// The error is a plain-English reason for the warning line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lowered = text.trim().to_lowercase();
        if lowered.is_empty() {
            return Err("empty chord".to_owned());
        }
        let parts: Vec<&str> = lowered.split('+').map(str::trim).collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err("empty part (write the plus key as \"plus\")".to_owned());
        }
        // `split` always yields at least one item, but the no-panic rule
        // says to prove that to the compiler rather than index.
        let Some((key_text, modifier_texts)) = parts.split_last() else {
            return Err("empty chord".to_owned());
        };

        let (mut ctrl, mut shift, mut alt) = (false, false, false);
        for modifier in modifier_texts {
            // `slot` is a mutable borrow of whichever flag the name selects.
            let slot = match *modifier {
                "ctrl" | "control" => &mut ctrl,
                "shift" => &mut shift,
                "alt" => &mut alt,
                other => return Err(format!("unknown modifier \"{other}\"")),
            };
            if *slot {
                return Err(format!("modifier \"{modifier}\" given twice"));
            }
            *slot = true;
        }

        let key = parse_key(key_text)?;
        match key {
            ChordKey::Named(Named::Escape) => {
                return Err("escape is reserved for closing popovers and editors".to_owned());
            }
            ChordKey::Char(_) if !ctrl && !alt => {
                return Err(
                    "a character key needs ctrl or alt (bare letters are type-ahead)".to_owned(),
                );
            }
            _ => {}
        }
        Ok(Chord {
            key,
            ctrl,
            shift,
            alt,
        })
    }

    /// The chord a raw key event represents, or `None` for keys that cannot
    /// be part of a chord (unidentified, or a multi-character dead-key
    /// result).
    fn from_event(key: &Key, modifiers: Modifiers) -> Option<Self> {
        let key = match key {
            Key::Named(named) => ChordKey::Named(*named),
            Key::Character(text) => {
                let mut chars = text.as_str().chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // Lowercase so "N" (what some layouts report with Shift
                // held) and "n" are the same key. `to_lowercase` is an
                // iterator because a few characters lowercase to several;
                // such a key is simply not bindable.
                let mut lower = first.to_lowercase();
                let only = lower.next()?;
                if lower.next().is_some() {
                    return None;
                }
                ChordKey::Char(only)
            }
            Key::Unidentified => return None,
        };
        Some(Chord {
            key,
            ctrl: modifiers.control(),
            shift: modifiers.shift(),
            alt: modifiers.alt(),
        })
    }
}

fn parse_key(text: &str) -> Result<ChordKey, String> {
    let squashed: String = text.chars().filter(|c| *c != '-' && *c != '_').collect();
    let canonical = KEY_ALIASES
        .iter()
        .find(|(alias, _)| *alias == squashed)
        .map_or(squashed.as_str(), |(_, canonical)| *canonical);
    if let Some((_, named)) = NAMED_KEYS.iter().find(|(name, _)| *name == canonical) {
        return Ok(ChordKey::Named(*named));
    }
    if canonical == "plus" {
        return Ok(ChordKey::Char('+'));
    }
    // A single printable character. Uses `text` (not `squashed`) so that
    // `-` and `_` themselves are valid keys.
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Ok(ChordKey::Char(c)),
        _ => Err(format!("unknown key \"{text}\"")),
    }
}

/// Writes the canonical spelling, which `parse` reads back to an equal
/// chord: `ctrl+shift+n`, `alt+up`, `f2`.
impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.shift {
            f.write_str("shift+")?;
        }
        if self.alt {
            f.write_str("alt+")?;
        }
        match self.key {
            ChordKey::Char('+') => f.write_str("plus"),
            ChordKey::Char(c) => write!(f, "{c}"),
            ChordKey::Named(named) => {
                let name = NAMED_KEYS
                    .iter()
                    .find(|(_, candidate)| *candidate == named)
                    .map_or("?", |(name, _)| *name);
                f.write_str(name)
            }
        }
    }
}

// ── The keymap ──────────────────────────────────────────────────────────

/// The resolved chord -> action table the app runs with: the defaults from
/// [`BINDINGS`] with the user's `[keys]` overrides applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// Chord -> index into [`BINDINGS`]. The lookup `resolve` uses.
    lookup: HashMap<Chord, usize>,
    /// For each [`BINDINGS`] row (same indices), the chords now bound to it
    /// (empty when unbound). What an editor shows.
    chords: Vec<Vec<Chord>>,
}

impl Default for Keymap {
    /// The built-in bindings: what you get with no `[keys]` table.
    fn default() -> Self {
        let (keymap, warnings) = Keymap::from_config(None);
        // A test proves the defaults are clean, so this loop never runs;
        // it exists so a bad row could never be silent.
        for warning in warnings {
            eprintln!("saola-files: built-in keymap: {warning}");
        }
        keymap
    }
}

impl Keymap {
    /// Resolve one key press into an [`Action`], or `None` if no binding
    /// owns that combination: the caller falls through to its own handling
    /// (type-ahead, or nothing).
    pub fn resolve(&self, key: &Key, modifiers: Modifiers) -> Option<Action> {
        let chord = Chord::from_event(key, modifiers)?;
        let index = *self.lookup.get(&chord)?;
        BINDINGS.get(index).map(|binding| binding.action)
    }

    /// Every row with the chords currently bound to it. The data a
    /// shortcut editor or help overlay renders.
    pub fn rows(&self) -> impl Iterator<Item = (&'static Binding, &[Chord])> {
        BINDINGS
            .iter()
            .zip(self.chords.iter())
            .map(|(binding, chords)| (binding, chords.as_slice()))
    }

    /// The chords bound to the action named `name` (`None` for an unknown
    /// name).
    pub fn chords_for(&self, name: &str) -> Option<&[Chord]> {
        let index = BINDINGS.iter().position(|binding| binding.name == name)?;
        self.chords.get(index).map(Vec::as_slice)
    }

    /// Builds the keymap from the optional `[keys]` value of `files.toml`,
    /// returning it with one warning string per problem.
    ///
    /// Rules (documented for users in the README):
    /// * A user value for an action **replaces** that action's defaults.
    ///   A string is one chord, an array is several, `""` or `[]` unbinds.
    /// * An unknown action name, a value of the wrong type, or an
    ///   unparseable chord warns about that entry only. If an action's
    ///   value had entries but none parsed, its defaults are kept.
    /// * A chord given to two actions by the user goes to the one whose
    ///   name sorts first; the other loses that chord.
    /// * A user chord that equals another action's *default* takes it
    ///   over: that default is dropped.
    ///
    /// Every rule produces a warning naming the actions and chord, so a
    /// config that loses a shortcut always says so. Pure: the caller
    /// (`config::read_keys`) prints the warnings.
    pub fn from_config(section: Option<&toml::Value>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();

        // 1. Parse the user's overrides: `user[i]` is `Some(chords)` when
        // row `i` was overridden (an empty Vec means "unbound").
        let mut user: Vec<Option<Vec<Chord>>> = vec![None; BINDINGS.len()];
        match section {
            None => {}
            Some(toml::Value::Table(table)) => {
                for (name, value) in table {
                    let Some(index) = BINDINGS.iter().position(|b| b.name == name.as_str()) else {
                        warnings.push(format!("[keys] unknown action \"{name}\" — ignoring it"));
                        continue;
                    };
                    if let Some(chords) = read_override(name, value, &mut warnings)
                        && let Some(slot) = user.get_mut(index)
                    {
                        *slot = Some(chords);
                    }
                }
            }
            Some(_) => warnings.push("[keys] must be a table — using the default keys".to_owned()),
        }

        // 2. Defaults for every row the user did not override. A default
        // that fails to parse is a bug in the table (a test forbids it);
        // it is skipped with a warning rather than panicking.
        let mut effective: Vec<Vec<Chord>> = Vec::with_capacity(BINDINGS.len());
        for (binding, overridden) in BINDINGS.iter().zip(user.iter()) {
            match overridden {
                Some(chords) => effective.push(chords.clone()),
                None => effective.push(
                    binding
                        .defaults
                        .iter()
                        .filter_map(|text| match Chord::parse(text) {
                            Ok(chord) => Some(chord),
                            Err(reason) => {
                                warnings.push(format!(
                                    "default chord \"{text}\" for {} is invalid: {reason}",
                                    binding.name
                                ));
                                None
                            }
                        })
                        .collect(),
                ),
            }
        }

        // 3. Resolve conflicts. User bindings claim chords first, in the
        // order of their action names (a stable order a user can predict,
        // unlike table order, which grows with the app).
        let mut order: Vec<usize> = (0..BINDINGS.len()).collect();
        order.sort_by_key(|&i| BINDINGS.get(i).map(|b| b.name));
        let name_of = |i: usize| BINDINGS.get(i).map_or("?", |b| b.name);
        let is_overridden = |i: usize| user.get(i).is_some_and(Option::is_some);

        let mut owner: HashMap<Chord, usize> = HashMap::new();
        for &index in order.iter().filter(|&&i| is_overridden(i)) {
            let Some(chords) = effective.get_mut(index) else {
                continue;
            };
            chords.retain(|chord| match owner.get(chord) {
                Some(&winner) => {
                    warnings.push(format!(
                        "[keys] {chord} is bound to both {} and {} — {} keeps it",
                        name_of(winner),
                        name_of(index),
                        name_of(winner),
                    ));
                    false
                }
                None => {
                    owner.insert(*chord, index);
                    true
                }
            });
        }
        for &index in order.iter().filter(|&&i| !is_overridden(i)) {
            let Some(chords) = effective.get_mut(index) else {
                continue;
            };
            chords.retain(|chord| match owner.get(chord) {
                Some(&winner) => {
                    warnings.push(format!(
                        "[keys] {chord} is the default for {} but {} is bound to it — \
                         unbinding the default",
                        name_of(index),
                        name_of(winner),
                    ));
                    false
                }
                None => {
                    owner.insert(*chord, index);
                    true
                }
            });
        }

        (
            Keymap {
                lookup: owner,
                chords: effective,
            },
            warnings,
        )
    }
}

/// Reads one `[keys]` value into the chords it names, or `None` when the
/// value is unusable and the action should keep its defaults.
fn read_override(
    name: &str,
    value: &toml::Value,
    warnings: &mut Vec<String>,
) -> Option<Vec<Chord>> {
    let entries: Vec<&toml::Value> = match value {
        toml::Value::String(text) if text.trim().is_empty() => return Some(Vec::new()),
        toml::Value::String(_) => vec![value],
        toml::Value::Array(items) if items.is_empty() => return Some(Vec::new()),
        toml::Value::Array(items) => items.iter().collect(),
        _ => {
            warnings.push(format!(
                "[keys] {name}: expected a chord string or an array of them — keeping the default"
            ));
            return None;
        }
    };

    let mut chords: Vec<Chord> = Vec::new();
    for entry in entries {
        let Some(text) = entry.as_str() else {
            warnings.push(format!(
                "[keys] {name}: chords must be strings — ignoring this entry"
            ));
            continue;
        };
        match Chord::parse(text) {
            Ok(chord) if !chords.contains(&chord) => chords.push(chord),
            Ok(_) => {} // the same chord twice: harmless, keep one
            Err(reason) => warnings.push(format!(
                "[keys] {name}: cannot use \"{text}\" ({reason}) — ignoring this entry"
            )),
        }
    }
    if chords.is_empty() {
        warnings.push(format!(
            "[keys] {name}: no usable chord — keeping the default"
        ));
        return None;
    }
    Some(chords)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Table-driven keymap (issue #18) ─────────────────────────────────

    /// Free-function shim so the pre-table tests below keep reading the
    /// way they always did, now through the default [`Keymap`].
    fn resolve(key: &Key, modifiers: Modifiers) -> Option<Action> {
        Keymap::default().resolve(key, modifiers)
    }

    fn parse_section(text: &str) -> (Keymap, Vec<String>) {
        let table: toml::Table = text.parse().expect("test TOML parses");
        Keymap::from_config(table.get("keys"))
    }

    fn press(keymap: &Keymap, key: &Key, modifiers: Modifiers) -> Option<Action> {
        keymap.resolve(key, modifiers)
    }

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    // The old hard-coded `resolve`, kept verbatim as the oracle the table
    // must reproduce. Deleted from production code; lives on only here.
    #[allow(clippy::too_many_lines)]
    mod oracle {
        use super::super::*;

        /// Resolve one key press into an [`Action`], or `None` if this module
        /// doesn't own that combination — the caller falls through to its own
        /// handling (or does nothing).
        pub(super) fn legacy_resolve(key: &Key, modifiers: Modifiers) -> Option<Action> {
            let shift = modifiers.shift();
            let ctrl = modifiers.control();
            let alt = modifiers.alt();

            // Ctrl+<letter>/Space combinations, no other modifier riding along.
            if ctrl && !shift && !alt {
                if let Key::Character(c) = key {
                    match c.as_str() {
                        "a" => return Some(Action::SelectAll),
                        "h" => return Some(Action::ToggleHidden),
                        "l" => return Some(Action::EditPath),
                        "1" => return Some(Action::SetViewList),
                        "2" => return Some(Action::SetViewGrid),
                        "c" => return Some(Action::Copy),
                        "x" => return Some(Action::Cut),
                        "v" => return Some(Action::Paste),
                        "z" => return Some(Action::Undo),
                        _ => {}
                    }
                }
                if matches!(key, Key::Named(Named::Space)) {
                    return Some(Action::ToggleCursorSelected);
                }
                return None;
            }

            // Alt+<arrow>, no other modifier riding along: browser-style history
            // navigation, plus the pre-existing Alt+Up ascend synonym for plain
            // Backspace (handled below). Alt+Enter (Stage 13) rides along in the
            // same "Alt + one named key" bucket rather than the arrows' own match
            // arm, since it isn't a history/ascend action.
            if alt && !ctrl && !shift {
                return match key {
                    Key::Named(Named::ArrowUp) => Some(Action::Ascend),
                    Key::Named(Named::ArrowLeft) => Some(Action::HistoryBack),
                    Key::Named(Named::ArrowRight) => Some(Action::HistoryForward),
                    Key::Named(Named::Enter) => Some(Action::Properties),
                    _ => None,
                };
            }

            // Ctrl+Shift+<letter>, no other modifier: currently only New Folder.
            // Checked with `eq_ignore_ascii_case` rather than a bare `"n"` match:
            // Shift held can change which case `Key::Character` reports depending
            // on the compositor/layout, and this binding must fire either way.
            if ctrl && shift && !alt {
                if let Key::Character(c) = key
                    && c.eq_ignore_ascii_case("n")
                {
                    return Some(Action::NewFolder);
                }
                return None;
            }

            // Every remaining action is a named key with at most Shift held.
            if ctrl || alt {
                return None;
            }
            let Key::Named(named) = key else {
                return None;
            };
            match (named, shift) {
                (Named::ArrowUp, false) => Some(Action::MoveCursorUp),
                (Named::ArrowUp, true) => Some(Action::ExtendSelectionUp),
                (Named::ArrowDown, false) => Some(Action::MoveCursorDown),
                (Named::ArrowDown, true) => Some(Action::ExtendSelectionDown),
                (Named::ArrowLeft, false) => Some(Action::MoveCursorLeft),
                (Named::ArrowLeft, true) => Some(Action::ExtendSelectionLeft),
                (Named::ArrowRight, false) => Some(Action::MoveCursorRight),
                (Named::ArrowRight, true) => Some(Action::ExtendSelectionRight),
                (Named::Home, false) => Some(Action::MoveCursorHome),
                (Named::Home, true) => Some(Action::ExtendSelectionHome),
                (Named::End, false) => Some(Action::MoveCursorEnd),
                (Named::End, true) => Some(Action::ExtendSelectionEnd),
                (Named::PageUp, false) => Some(Action::MoveCursorPageUp),
                (Named::PageUp, true) => Some(Action::ExtendSelectionPageUp),
                (Named::PageDown, false) => Some(Action::MoveCursorPageDown),
                (Named::PageDown, true) => Some(Action::ExtendSelectionPageDown),
                (Named::Enter, false) => Some(Action::Descend),
                (Named::Backspace, false) => Some(Action::Ascend),
                (Named::F5, false) => Some(Action::Refresh),
                (Named::F2, false) => Some(Action::Rename),
                (Named::Delete, false) => Some(Action::Delete),
                (Named::Delete, true) => Some(Action::PermanentDelete),
                // The dedicated Menu key (winit's `ContextMenu`, iced_core
                // `key::Named::ContextMenu`) and its keyboard-only equivalent.
                (Named::ContextMenu, _) | (Named::F10, true) => Some(Action::OpenContextMenu),
                _ => None,
            }
        }
    }

    #[test]
    fn default_table_equals_the_old_hard_coded_mapping() {
        // Every key a person could plausibly press, with every Ctrl/Shift/
        // Alt combination (Logo is ignored by both implementations).
        let mut keys: Vec<Key> = ('a'..='z')
            .chain('0'..='9')
            .chain([' ', ',', '.', '-', '=', '/'])
            .map(|c| Key::Character(c.to_string().as_str().into()))
            .collect();
        // Uppercase is only reported with Shift on some layouts; the old
        // code accepted it for Ctrl+Shift+N alone, and so does the table.
        keys.push(ch("N"));
        keys.push(Key::Unidentified);
        keys.extend(
            [
                Named::ArrowUp,
                Named::ArrowDown,
                Named::ArrowLeft,
                Named::ArrowRight,
                Named::Home,
                Named::End,
                Named::PageUp,
                Named::PageDown,
                Named::Enter,
                Named::Backspace,
                Named::Delete,
                Named::Insert,
                Named::Tab,
                Named::Space,
                Named::Escape,
                Named::ContextMenu,
                Named::F1,
                Named::F2,
                Named::F5,
                Named::F10,
                Named::F12,
            ]
            .map(Key::Named),
        );

        let keymap = Keymap::default();
        let mut compared = 0;
        for key in &keys {
            for bits in 0..16u8 {
                let mut modifiers = Modifiers::empty();
                if bits & 1 != 0 {
                    modifiers |= Modifiers::CTRL;
                }
                if bits & 2 != 0 {
                    modifiers |= Modifiers::SHIFT;
                }
                if bits & 4 != 0 {
                    modifiers |= Modifiers::ALT;
                }
                if bits & 8 != 0 {
                    modifiers |= Modifiers::LOGO;
                }
                // Uppercase with Ctrl but no Shift never happens in a
                // real event stream (`key` is the unshifted key).
                if matches!(key, Key::Character(c) if c.as_str() == "N")
                    && !(modifiers.control() && modifiers.shift())
                {
                    continue;
                }
                assert_eq!(
                    keymap.resolve(key, modifiers),
                    oracle::legacy_resolve(key, modifiers),
                    "{key:?} with {modifiers:?}"
                );
                compared += 1;
            }
        }
        assert!(compared > 1000, "grid unexpectedly small: {compared}");
    }

    #[test]
    fn default_table_is_clean_and_unambiguous() {
        let (keymap, warnings) = Keymap::from_config(None);
        assert!(warnings.is_empty(), "{warnings:?}");
        // Every default parsed (none dropped) and no chord is shared.
        let total: usize = BINDINGS.iter().map(|b| b.defaults.len()).sum();
        assert_eq!(keymap.lookup.len(), total);
        let mut names: Vec<&str> = BINDINGS.iter().map(|b| b.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BINDINGS.len(), "duplicate action name");
        let mut actions: Vec<String> = BINDINGS.iter().map(|b| format!("{:?}", b.action)).collect();
        actions.sort_unstable();
        actions.dedup();
        assert_eq!(actions.len(), BINDINGS.len(), "action in two rows");
    }

    #[test]
    fn every_label_exists_in_the_catalogue() {
        for binding in BINDINGS {
            assert_ne!(
                crate::i18n::lookup(binding.label, None),
                binding.label,
                "{} has no catalogue message {}",
                binding.name,
                binding.label
            );
            assert!(!binding.label_text().is_empty());
        }
    }

    #[test]
    fn names_are_snake_case() {
        for binding in BINDINGS {
            assert!(
                binding
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{}",
                binding.name
            );
        }
    }

    // ── Chord parsing ───────────────────────────────────────────────────

    #[test]
    fn chords_round_trip_through_display() {
        let samples = [
            "ctrl+shift+n",
            "alt+up",
            "f2",
            "menu",
            "shift+delete",
            "ctrl+alt+plus",
            "ctrl+,",
            "ctrl+-",
            "ctrl+space",
            "pagedown",
        ];
        for text in samples {
            let chord = Chord::parse(text).expect(text);
            assert_eq!(chord.to_string(), text);
            assert_eq!(Chord::parse(&chord.to_string()), Ok(chord));
        }
        for binding in BINDINGS {
            for text in binding.defaults {
                let chord = Chord::parse(text).expect(text);
                assert_eq!(&chord.to_string(), text, "defaults are written canonically");
            }
        }
    }

    #[test]
    fn parsing_ignores_case_spacing_and_aliases() {
        let canonical = Chord::parse("ctrl+shift+n").unwrap();
        assert_eq!(Chord::parse("Ctrl + Shift + N"), Ok(canonical));
        assert_eq!(Chord::parse("shift+control+n"), Ok(canonical));
        assert_eq!(Chord::parse("PgUp"), Chord::parse("pageup"));
        assert_eq!(Chord::parse("page_up"), Chord::parse("pageup"));
        assert_eq!(Chord::parse("alt+ArrowUp"), Chord::parse("alt+up"));
        assert_eq!(Chord::parse("ContextMenu"), Chord::parse("menu"));
    }

    #[test]
    fn invalid_chords_are_rejected_with_a_reason() {
        for bad in [
            "",
            "   ",
            "ctrl+",
            "+n",
            "ctrl++n",
            "ctrl+ctrl+n",
            "hyper+n",
            "ctrl+nonsense",
            "ctrl+ab",
            "n",
            "shift+n",
            "escape",
            "ctrl+escape",
        ] {
            let result = Chord::parse(bad);
            assert!(result.is_err(), "{bad:?} should not parse: {result:?}");
            assert!(!result.unwrap_err().is_empty());
        }
    }

    #[test]
    fn matching_is_exact_on_modifiers_and_case_insensitive_on_letters() {
        let km = Keymap::default();
        assert_eq!(
            press(&km, &ch("N"), Modifiers::CTRL | Modifiers::SHIFT),
            Some(Action::NewFolder)
        );
        assert_eq!(press(&km, &ch("n"), Modifiers::CTRL), None);
        assert_eq!(
            press(&km, &ch("a"), Modifiers::CTRL | Modifiers::LOGO),
            Some(Action::SelectAll),
            "Logo is ignored, as before"
        );
    }

    // ── [keys] overrides ────────────────────────────────────────────────

    #[test]
    fn rebinding_replaces_the_old_chord() {
        let (km, warnings) = parse_section("[keys]\nnew_folder = \"ctrl+alt+n\"");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            press(&km, &ch("n"), Modifiers::CTRL | Modifiers::ALT),
            Some(Action::NewFolder)
        );
        assert_eq!(
            press(&km, &ch("n"), Modifiers::CTRL | Modifiers::SHIFT),
            None
        );
        // Everything else is untouched.
        assert_eq!(press(&km, &ch("c"), Modifiers::CTRL), Some(Action::Copy));
    }

    #[test]
    fn an_array_binds_several_chords() {
        let (km, warnings) = parse_section("[keys]\nrename = [\"f2\", \"ctrl+r\"]");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            press(&km, &named(Named::F2), Modifiers::empty()),
            Some(Action::Rename)
        );
        assert_eq!(press(&km, &ch("r"), Modifiers::CTRL), Some(Action::Rename));
        assert_eq!(km.chords_for("rename").map(<[Chord]>::len), Some(2));
    }

    #[test]
    fn empty_string_or_array_unbinds() {
        for value in ["\"\"", "[]"] {
            let (km, warnings) = parse_section(&format!("[keys]\nundo = {value}"));
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(press(&km, &ch("z"), Modifiers::CTRL), None);
            assert_eq!(km.chords_for("undo"), Some(&[][..]));
        }
    }

    #[test]
    fn a_user_chord_takes_over_another_actions_default_with_a_warning() {
        let (km, warnings) = parse_section("[keys]\nselect_all = \"ctrl+c\"");
        assert_eq!(
            press(&km, &ch("c"), Modifiers::CTRL),
            Some(Action::SelectAll)
        );
        // Copy lost its only default; select_all's old chord is gone too.
        assert_eq!(km.chords_for("copy"), Some(&[][..]));
        assert_eq!(press(&km, &ch("a"), Modifiers::CTRL), None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("copy") && warnings[0].contains("select_all"));
        assert!(warnings[0].contains("ctrl+c"));
    }

    #[test]
    fn two_user_bindings_on_one_chord_go_to_the_first_name_alphabetically() {
        let (km, warnings) = parse_section("[keys]\ncut = \"ctrl+k\"\ncopy = \"ctrl+k\"");
        assert_eq!(press(&km, &ch("k"), Modifiers::CTRL), Some(Action::Copy));
        assert_eq!(km.chords_for("cut"), Some(&[][..]));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("copy keeps it"));
        // Deterministic: the same input gives the same keymap every time.
        let (again, _) = parse_section("[keys]\ncopy = \"ctrl+k\"\ncut = \"ctrl+k\"");
        assert_eq!(km, again);
    }

    #[test]
    fn an_unknown_action_warns_by_name_and_changes_nothing_else() {
        let (km, warnings) = parse_section("[keys]\nlevitate = \"ctrl+l\"\nundo = \"ctrl+u\"");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("levitate"));
        assert_eq!(press(&km, &ch("u"), Modifiers::CTRL), Some(Action::Undo));
    }

    #[test]
    fn a_bad_chord_warns_for_that_entry_only_and_keeps_the_default() {
        let (km, warnings) = parse_section(
            "[keys]\nundo = \"ctrl+nonsense\"\npaste = [\"ctrl+nonsense\", \"ctrl+p\"]\ncopy = 7",
        );
        // undo: nothing usable -> default kept.
        assert_eq!(press(&km, &ch("z"), Modifiers::CTRL), Some(Action::Undo));
        // paste: the good entry applies, the bad one is dropped.
        assert_eq!(press(&km, &ch("p"), Modifiers::CTRL), Some(Action::Paste));
        assert_eq!(press(&km, &ch("v"), Modifiers::CTRL), None);
        // copy: wrong type -> default kept.
        assert_eq!(press(&km, &ch("c"), Modifiers::CTRL), Some(Action::Copy));
        assert!(warnings.iter().any(|w| w.contains("undo")));
        assert!(warnings.iter().any(|w| w.contains("paste")));
        assert!(warnings.iter().any(|w| w.contains("copy")));
        assert!(
            warnings
                .iter()
                .all(|w| w.contains("ctrl+nonsense") || w.contains("copy") || w.contains("undo"))
        );
    }

    #[test]
    fn a_non_table_keys_value_warns_and_uses_defaults() {
        let (km, warnings) = parse_section("keys = \"oops\"");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(km, Keymap::default());
    }

    #[test]
    fn rebound_keys_never_reach_type_ahead_or_escape() {
        let (_, warnings) = parse_section("[keys]\nrefresh = \"r\"\nundo = \"escape\"");
        assert_eq!(
            warnings.iter().filter(|w| w.contains("ignoring")).count(),
            2
        );
    }

    /// How a variant is covered by [`BINDINGS`].
    enum Coverage {
        /// Has a row with this name.
        Row(&'static str),
        // `Exempt(&'static str)` (the reason) goes here the day an action
        // is deliberately unbound by default: add the variant and its arm.
    }

    /// Exhaustive on purpose (no wildcard arm): adding an `Action` variant
    /// breaks the build here until it is given a row name, so a new action
    /// cannot be silently unreachable from the keyboard. `Keymap::resolve`
    /// is table-only.
    fn coverage(action: Action) -> Coverage {
        match action {
            Action::MoveCursorUp => Coverage::Row("move_cursor_up"),
            Action::MoveCursorDown => Coverage::Row("move_cursor_down"),
            Action::MoveCursorLeft => Coverage::Row("move_cursor_left"),
            Action::MoveCursorRight => Coverage::Row("move_cursor_right"),
            Action::MoveCursorHome => Coverage::Row("move_cursor_home"),
            Action::MoveCursorEnd => Coverage::Row("move_cursor_end"),
            Action::MoveCursorPageUp => Coverage::Row("move_cursor_page_up"),
            Action::MoveCursorPageDown => Coverage::Row("move_cursor_page_down"),
            Action::ExtendSelectionUp => Coverage::Row("extend_selection_up"),
            Action::ExtendSelectionDown => Coverage::Row("extend_selection_down"),
            Action::ExtendSelectionLeft => Coverage::Row("extend_selection_left"),
            Action::ExtendSelectionRight => Coverage::Row("extend_selection_right"),
            Action::ExtendSelectionHome => Coverage::Row("extend_selection_home"),
            Action::ExtendSelectionEnd => Coverage::Row("extend_selection_end"),
            Action::ExtendSelectionPageUp => Coverage::Row("extend_selection_page_up"),
            Action::ExtendSelectionPageDown => Coverage::Row("extend_selection_page_down"),
            Action::ToggleCursorSelected => Coverage::Row("toggle_cursor_selected"),
            Action::SelectAll => Coverage::Row("select_all"),
            Action::Descend => Coverage::Row("descend"),
            Action::Ascend => Coverage::Row("ascend"),
            Action::ToggleHidden => Coverage::Row("toggle_hidden"),
            Action::HistoryBack => Coverage::Row("history_back"),
            Action::HistoryForward => Coverage::Row("history_forward"),
            Action::EditPath => Coverage::Row("edit_path"),
            Action::SetViewList => Coverage::Row("set_view_list"),
            Action::SetViewGrid => Coverage::Row("set_view_grid"),
            Action::Refresh => Coverage::Row("refresh"),
            Action::Copy => Coverage::Row("copy"),
            Action::Cut => Coverage::Row("cut"),
            Action::Paste => Coverage::Row("paste"),
            Action::Rename => Coverage::Row("rename"),
            Action::NewFolder => Coverage::Row("new_folder"),
            Action::Delete => Coverage::Row("delete"),
            Action::PermanentDelete => Coverage::Row("permanent_delete"),
            Action::Undo => Coverage::Row("undo"),
            Action::Properties => Coverage::Row("properties"),
            Action::OpenContextMenu => Coverage::Row("open_context_menu"),
            // `AppAction` has no variants yet, so this match is empty. When
            // one is added, extend it with one arm per variant.
            Action::App(app) => match app {},
        }
    }

    /// One of each variant. The exhaustive `match` above sends an author
    /// to this spot; add the new variant here too.
    const ALL_ACTIONS: &[Action] = &[
        Action::MoveCursorUp,
        Action::MoveCursorDown,
        Action::MoveCursorLeft,
        Action::MoveCursorRight,
        Action::MoveCursorHome,
        Action::MoveCursorEnd,
        Action::MoveCursorPageUp,
        Action::MoveCursorPageDown,
        Action::ExtendSelectionUp,
        Action::ExtendSelectionDown,
        Action::ExtendSelectionLeft,
        Action::ExtendSelectionRight,
        Action::ExtendSelectionHome,
        Action::ExtendSelectionEnd,
        Action::ExtendSelectionPageUp,
        Action::ExtendSelectionPageDown,
        Action::ToggleCursorSelected,
        Action::SelectAll,
        Action::Descend,
        Action::Ascend,
        Action::ToggleHidden,
        Action::HistoryBack,
        Action::HistoryForward,
        Action::EditPath,
        Action::SetViewList,
        Action::SetViewGrid,
        Action::Refresh,
        Action::Copy,
        Action::Cut,
        Action::Paste,
        Action::Rename,
        Action::NewFolder,
        Action::Delete,
        Action::PermanentDelete,
        Action::Undo,
        Action::Properties,
        Action::OpenContextMenu,
    ];

    #[test]
    fn every_action_variant_has_a_binding_row() {
        for action in ALL_ACTIONS {
            match coverage(*action) {
                Coverage::Row(name) => assert!(
                    BINDINGS
                        .iter()
                        .any(|b| b.name == name && b.action == *action),
                    "{action:?} promises a row named {name} that does not exist"
                ),
            }
        }
        // The list is not missing a row's action either.
        for binding in BINDINGS {
            assert!(
                ALL_ACTIONS.contains(&binding.action),
                "{:?} missing from ALL_ACTIONS",
                binding.action
            );
        }
    }

    #[test]
    fn rows_expose_names_labels_and_chords_for_an_editor() {
        let km = Keymap::default();
        let (binding, chords) = km.rows().find(|(b, _)| b.name == "ascend").unwrap();
        assert_eq!(binding.label_text(), "Go to parent folder");
        let shown: Vec<String> = chords.iter().map(Chord::to_string).collect();
        assert_eq!(shown, ["backspace", "alt+up"]);
        assert_eq!(km.rows().count(), BINDINGS.len());
    }

    fn named(key: Named) -> Key {
        Key::Named(key)
    }

    #[test]
    fn plain_arrows_move_the_cursor() {
        assert_eq!(
            resolve(&named(Named::ArrowDown), Modifiers::empty()),
            Some(Action::MoveCursorDown)
        );
        assert_eq!(
            resolve(&named(Named::ArrowUp), Modifiers::empty()),
            Some(Action::MoveCursorUp)
        );
    }

    #[test]
    fn shift_arrows_extend_selection() {
        assert_eq!(
            resolve(&named(Named::ArrowDown), Modifiers::SHIFT),
            Some(Action::ExtendSelectionDown)
        );
    }

    #[test]
    fn enter_descends_backspace_ascends() {
        assert_eq!(
            resolve(&named(Named::Enter), Modifiers::empty()),
            Some(Action::Descend)
        );
        assert_eq!(
            resolve(&named(Named::Backspace), Modifiers::empty()),
            Some(Action::Ascend)
        );
    }

    #[test]
    fn alt_up_also_ascends() {
        assert_eq!(
            resolve(&named(Named::ArrowUp), Modifiers::ALT),
            Some(Action::Ascend)
        );
    }

    #[test]
    fn ctrl_a_selects_all_ctrl_h_toggles_hidden() {
        assert_eq!(
            resolve(&Key::Character("a".into()), Modifiers::CTRL),
            Some(Action::SelectAll)
        );
        assert_eq!(
            resolve(&Key::Character("h".into()), Modifiers::CTRL),
            Some(Action::ToggleHidden)
        );
    }

    #[test]
    fn ctrl_space_toggles_cursor_row() {
        assert_eq!(
            resolve(&named(Named::Space), Modifiers::CTRL),
            Some(Action::ToggleCursorSelected)
        );
    }

    #[test]
    fn unmapped_keys_resolve_to_none() {
        assert_eq!(resolve(&named(Named::Tab), Modifiers::empty()), None);
        // "x"/"z" are Ctrl+X (Cut)/Ctrl+Z (Undo) as of Stage 8/10 — "q" is
        // still unmapped, and still proves the same thing this test always
        // has (an ordinary Ctrl+<letter> with no binding falls through to
        // `None`).
        assert_eq!(resolve(&Key::Character("q".into()), Modifiers::CTRL), None);
    }

    #[test]
    fn alt_left_right_step_through_history() {
        assert_eq!(
            resolve(&named(Named::ArrowLeft), Modifiers::ALT),
            Some(Action::HistoryBack)
        );
        assert_eq!(
            resolve(&named(Named::ArrowRight), Modifiers::ALT),
            Some(Action::HistoryForward)
        );
    }

    #[test]
    fn ctrl_l_edits_the_path_ctrl_1_2_switch_view() {
        assert_eq!(
            resolve(&Key::Character("l".into()), Modifiers::CTRL),
            Some(Action::EditPath)
        );
        assert_eq!(
            resolve(&Key::Character("1".into()), Modifiers::CTRL),
            Some(Action::SetViewList)
        );
        assert_eq!(
            resolve(&Key::Character("2".into()), Modifiers::CTRL),
            Some(Action::SetViewGrid)
        );
    }

    #[test]
    fn menu_key_and_shift_f10_open_the_context_menu() {
        assert_eq!(
            resolve(&named(Named::ContextMenu), Modifiers::empty()),
            Some(Action::OpenContextMenu)
        );
        assert_eq!(
            resolve(&named(Named::F10), Modifiers::SHIFT),
            Some(Action::OpenContextMenu)
        );
    }

    #[test]
    fn plain_f10_is_not_the_context_menu() {
        assert_eq!(resolve(&named(Named::F10), Modifiers::empty()), None);
    }

    #[test]
    fn f5_refreshes() {
        assert_eq!(
            resolve(&named(Named::F5), Modifiers::empty()),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn alt_shift_arrow_is_unmapped() {
        // The alt-only block above must not swallow Alt+Shift combos —
        // they fall through to the generic "ctrl || alt => None" guard.
        assert_eq!(
            resolve(&named(Named::ArrowLeft), Modifiers::ALT | Modifiers::SHIFT),
            None
        );
    }

    // ── Stage 8: clipboard, rename, new folder ──────────────────────────

    #[test]
    fn ctrl_c_x_v_are_copy_cut_paste() {
        assert_eq!(
            resolve(&Key::Character("c".into()), Modifiers::CTRL),
            Some(Action::Copy)
        );
        assert_eq!(
            resolve(&Key::Character("x".into()), Modifiers::CTRL),
            Some(Action::Cut)
        );
        assert_eq!(
            resolve(&Key::Character("v".into()), Modifiers::CTRL),
            Some(Action::Paste)
        );
    }

    #[test]
    fn f2_renames() {
        assert_eq!(
            resolve(&named(Named::F2), Modifiers::empty()),
            Some(Action::Rename)
        );
    }

    #[test]
    fn ctrl_shift_n_creates_a_new_folder_regardless_of_reported_case() {
        assert_eq!(
            resolve(
                &Key::Character("n".into()),
                Modifiers::CTRL | Modifiers::SHIFT
            ),
            Some(Action::NewFolder)
        );
        assert_eq!(
            resolve(
                &Key::Character("N".into()),
                Modifiers::CTRL | Modifiers::SHIFT
            ),
            Some(Action::NewFolder)
        );
    }

    #[test]
    fn ctrl_shift_other_letters_are_unmapped() {
        assert_eq!(
            resolve(
                &Key::Character("x".into()),
                Modifiers::CTRL | Modifiers::SHIFT
            ),
            None
        );
    }

    // ── Stage 9: trash / permanent delete ────────────────────────────────

    #[test]
    fn delete_trashes_shift_delete_is_permanent() {
        assert_eq!(
            resolve(&named(Named::Delete), Modifiers::empty()),
            Some(Action::Delete)
        );
        assert_eq!(
            resolve(&named(Named::Delete), Modifiers::SHIFT),
            Some(Action::PermanentDelete)
        );
    }

    // ── Stage 10: undo ────────────────────────────────────────────────────

    #[test]
    fn ctrl_z_undoes() {
        assert_eq!(
            resolve(&Key::Character("z".into()), Modifiers::CTRL),
            Some(Action::Undo)
        );
    }

    // ── Stage 13: properties ─────────────────────────────────────────────

    #[test]
    fn alt_enter_opens_properties() {
        assert_eq!(
            resolve(&named(Named::Enter), Modifiers::ALT),
            Some(Action::Properties)
        );
    }

    #[test]
    fn plain_enter_still_descends_not_properties() {
        // Alt+Enter must not swallow the plain-Enter binding it sits right
        // next to in the match arm above.
        assert_eq!(
            resolve(&named(Named::Enter), Modifiers::empty()),
            Some(Action::Descend)
        );
    }
}
