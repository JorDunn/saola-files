//! Context menu (the header's overflow button) and Open-with popover.
//!
//! Both render as an ink popover stacked over the directory view's content
//! via `iced::widget::stack!`, with a full-bleed scrim `mouse_area` behind
//! it that closes the menu on any outside click — the style guide's §6
//! Popover rule ("only one popover open at a time… anchored… must never
//! overlap the control that opened them") adapted from the panel's
//! layer-shell popovers to this ordinary window's overlay stack (this app
//! has no layer-shell surfaces at all — see this crate's top-level docs).
//! An iced button without `.on_press` renders `Disabled` **and does not
//! capture its press** (CLAUDE.md gotcha) — the scrim is a bare
//! `mouse_area`, not a button, specifically so a click on it always
//! registers and closes the menu rather than silently doing nothing.
//!
//! Lives outside `ui::dirview`'s module tree, like `ui::breadcrumbs`/
//! `ui::header`: it only ever reads `DirectoryView`'s public surface and
//! constructs `dirview::Message` values, the same boundary those two
//! files already respect.
//!
//! **Scope, updated Stage 8:** Cut/Copy/Paste/Rename/New Folder/New File
//! joined Open/Open with…/Open in terminal/custom actions this stage, now
//! that `core::fs::ops` and `dirview::rename` exist to actually back them.
//! Paste is worded/gated on whether the clipboard has anything
//! (`clipboard_has_contents`, threaded in at render time the same way
//! `mime_db`/`apps_db` already are — CLAUDE.md's capability-honest
//! posture: an always-enabled Paste that silently does nothing would be a
//! fake affordance).
//!
//! **Scope, updated Stage 9:** Delete joined the menu, worded per
//! `Caps::TRASH` ("Move to Trash" when the backend can, "Delete" — and
//! nothing else, no extra parenthetical — when it can't, since the row
//! itself is the only place that wording needs to live). There is
//! deliberately no second "Delete Permanently" row here: Shift+Delete
//! covers that already, and a menu offering *two* delete rows next to each
//! other reads as more dangerous, not more honest — the capability-honest
//! posture is about not hiding what a control does, not about surfacing
//! every possible variant of it in the menu.
//!
//! **Scope, updated Stage 13:** "Properties" joined the menu, the mouse
//! path to the same dialog Alt+Enter opens (`ui::dialogs::properties`).
//! Shown whenever the selection isn't empty, same gate `Copy`/`Cut` already
//! use.
//!
//! **The context menu is a table.** [`CONTEXT_MENU`] lists every fixed
//! row once, in display order, each with a predicate deciding whether it
//! shows for the current [`MenuContext`]. Adding an entry is adding one
//! line there (plus its `dirview::menu::Message` variant and handler).
//! The config-defined `[[action]]` rows follow the table: there can be any
//! number of them, so they are appended by `context_menu` itself.
//!
//! **Anchoring.** Opened from a right-click or the Menu key, the popovers
//! hang from the pointer or the cursor row (`DirectoryView::
//! menu_anchor_rect`) and are placed by `ui::anchored`, which flips them at
//! the window edge once the menu's real size is known (the rule itself is
//! `ui::menu_placement`). Opened from the header's overflow button they
//! keep the simpler top-right anchoring, roughly under where that button
//! always renders: a precise "grow from the trigger" placement would need
//! the trigger's on-screen rect, which iced's plain `view()` cannot learn.

use iced::widget::{Space, column, container, mouse_area, stack, text};
use iced::{Element, Fill, Rectangle};
use saola_theme::icon::Icon;
use saola_theme::widget::{self as saola_widget, Emphasis};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::config::CustomAction;
use crate::core::apps::{AppsDb, DesktopEntry};
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::mime::MimeDb;
use crate::core::vfs::{Caps, Location};
use crate::{msg, tr_static};

use super::anchored::Anchored;
use super::dirview::{DirectoryView, Message, menu};

/// Wraps `content` (the ordinary directory view) with whichever overlay is
/// currently open, if any — otherwise returns `content` untouched. Called
/// from `DirectoryView::view` once per frame; cheap when nothing is open
/// (a single `bool`/`bool` check, no allocation).
pub fn overlay<'a>(
    t: &'a Theme,
    state: &'a DirectoryView,
    mime_db: &'a MimeDb,
    apps_db: &'a AppsDb,
    clipboard_has_contents: bool,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    // `None` = opened from the header's overflow button (legacy top-right);
    // `Some` = opened by a right-click or the Menu key, anchored there.
    let anchor = state.menu_anchor_rect(&t.sizes);
    if state.open_with_open() {
        return popover_stack(
            t,
            content,
            open_with_popover(t, state, mime_db, apps_db),
            anchor,
        );
    }
    if state.menu_open() {
        return popover_stack(
            t,
            content,
            context_menu(t, state, mime_db, clipboard_has_contents),
            anchor,
        );
    }
    content
}

fn popover_stack<'a>(
    t: &'a Theme,
    content: Element<'a, Message>,
    popover: Element<'a, Message>,
    anchor: Option<Rectangle>,
) -> Element<'a, Message> {
    // The scrim closes on a right press as well: a right-click outside the
    // menu dismisses it rather than falling through to the list behind.
    let scrim = mouse_area(Space::new().width(Fill).height(Fill))
        .on_press(Message::Menu(menu::Message::Close))
        .on_right_press(Message::Menu(menu::Message::Close));
    // A bare `container` does not capture clicks, so a press on the menu's
    // own padding would fall through to the scrim and close it. Wrap it so
    // both buttons are swallowed (button-without-`on_press` gotcha, AGENTS.md).
    let swallow = Message::Menu(menu::Message::Swallow);
    let popover = mouse_area(popover)
        .on_press(swallow.clone())
        .on_right_press(swallow);
    let placed: Element<'a, Message> = match anchor {
        // Beside the click/row; flips at the edges (`ui::anchored`).
        Some(rect) => Anchored::new(popover, rect, t.sizes.gap_tight).into(),
        None => container(popover)
            .width(Fill)
            .height(Fill)
            .padding(iced::Padding {
                top: t.sizes.window_header,
                right: t.sizes.pill_gap,
                bottom: 0.0,
                left: 0.0,
            })
            .align_x(iced::alignment::Horizontal::Right)
            .into(),
    };
    stack![content, scrim, placed].into()
}

/// What a context-menu row's predicate gets to look at: the facts about
/// the current selection and app state that decide which rows show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MenuContext {
    /// How many entries are selected.
    pub(super) selected: usize,
    /// Whether the app's clipboard has anything to paste.
    pub(super) clipboard_has_contents: bool,
    /// Whether the current location's backend can trash (`Caps::TRASH`).
    /// Only resolved when something is selected, since only the delete
    /// rows read it.
    pub(super) can_trash: bool,
}

/// One fixed row of the context menu.
pub(super) struct MenuEntry {
    pub(super) glyph: Icon,
    /// Catalogue id of the label (wrapped in `msg!`); resolved with
    /// `i18n::lookup_static` when the row is drawn.
    pub(super) label_id: &'static str,
    /// Whether this row shows for a given context. A plain function
    /// pointer (`fn`), not a closure, so the whole table can be a `const`.
    pub(super) shown: fn(&MenuContext) -> bool,
    pub(super) message: menu::Message,
}

fn always(_: &MenuContext) -> bool {
    true
}

fn has_selection(ctx: &MenuContext) -> bool {
    ctx.selected > 0
}

fn single_selection(ctx: &MenuContext) -> bool {
    ctx.selected == 1
}

fn has_clipboard(ctx: &MenuContext) -> bool {
    ctx.clipboard_has_contents
}

fn has_selection_and_trash(ctx: &MenuContext) -> bool {
    has_selection(ctx) && ctx.can_trash
}

fn has_selection_without_trash(ctx: &MenuContext) -> bool {
    has_selection(ctx) && !ctx.can_trash
}

/// The context menu's fixed rows, in display order. "Move to Trash" and
/// "Delete" are the same action worded per `Caps::TRASH`; their predicates
/// are mutually exclusive, so exactly one of them shows with a selection.
#[rustfmt::skip]
pub(super) const CONTEXT_MENU: &[MenuEntry] = &[
    MenuEntry { glyph: Icon::ExternalLink,   label_id: msg!("menu-open"),             shown: has_selection,               message: menu::Message::OpenSelected },
    MenuEntry { glyph: Icon::Ellipsis,       label_id: msg!("menu-open-with"),        shown: has_selection,               message: menu::Message::OpenWithRequested },
    MenuEntry { glyph: Icon::Terminal,       label_id: msg!("menu-open-in-terminal"), shown: always,                      message: menu::Message::OpenTerminalRequested },
    MenuEntry { glyph: Icon::Copy,           label_id: msg!("menu-copy"),             shown: has_selection,               message: menu::Message::CopyRequested },
    MenuEntry { glyph: Icon::Scissors,       label_id: msg!("menu-cut"),              shown: has_selection,               message: menu::Message::CutRequested },
    MenuEntry { glyph: Icon::ClipboardPaste, label_id: msg!("menu-paste"),            shown: has_clipboard,               message: menu::Message::PasteRequested },
    MenuEntry { glyph: Icon::Pencil,         label_id: msg!("menu-rename"),           shown: single_selection,            message: menu::Message::RenameRequested },
    MenuEntry { glyph: Icon::Trash2,         label_id: msg!("menu-move-to-trash"),    shown: has_selection_and_trash,     message: menu::Message::DeleteRequested },
    MenuEntry { glyph: Icon::Trash2,         label_id: msg!("menu-delete"),           shown: has_selection_without_trash, message: menu::Message::DeleteRequested },
    MenuEntry { glyph: Icon::FolderPlus,     label_id: msg!("menu-new-folder"),       shown: always,                      message: menu::Message::NewFolderRequested },
    MenuEntry { glyph: Icon::FilePlus,       label_id: msg!("menu-new-file"),         shown: always,                      message: menu::Message::NewFileRequested },
    MenuEntry { glyph: Icon::Info,           label_id: msg!("menu-properties"),       shown: has_selection,               message: menu::Message::PropertiesRequested },
    // --- features: insert one row at the position it should display ---
];

/// The table rows that show for `ctx`, in display order.
pub(super) fn visible_entries(ctx: &MenuContext) -> impl Iterator<Item = &'static MenuEntry> + '_ {
    CONTEXT_MENU.iter().filter(move |entry| (entry.shown)(ctx))
}

fn context_menu<'a>(
    t: &'a Theme,
    state: &'a DirectoryView,
    mime_db: &'a MimeDb,
    clipboard_has_contents: bool,
) -> Element<'a, Message> {
    let selected = state.selected_entries();
    let ctx = MenuContext {
        selected: selected.len(),
        clipboard_has_contents,
        // `caps()` resolves a backend, so only ask when a row needs it.
        can_trash: !selected.is_empty() && state.caps().contains(Caps::TRASH),
    };

    let mut items: Vec<Element<'a, Message>> = visible_entries(&ctx)
        .map(|entry| {
            menu_row(
                t,
                entry.glyph,
                crate::i18n::lookup_static(entry.label_id),
                Message::Menu(entry.message.clone()),
            )
        })
        .collect();

    if !selected.is_empty() {
        let scheme = state.location().scheme.clone();
        let mimetypes: Vec<String> = selected
            .iter()
            .map(|entry| entry_mimetype(state.location(), entry, mime_db))
            .collect();
        for (index, action) in state.actions().iter().enumerate() {
            let applies = mimetypes
                .iter()
                .all(|mime| action_applies(action, mime, &scheme));
            if applies {
                items.push(menu_row(
                    t,
                    Icon::ChevronRight,
                    &action.name,
                    Message::Menu(menu::Message::CustomActionRequested(index)),
                ));
            }
        }
    }

    popover_container(t, column(items).width(t.sizes.menu_width))
}

fn open_with_popover<'a>(
    t: &'a Theme,
    state: &'a DirectoryView,
    mime_db: &'a MimeDb,
    apps_db: &'a AppsDb,
) -> Element<'a, Message> {
    let selected = state.selected_entries();
    let Some(first) = selected.first() else {
        return popover_container(t, empty_note(t, tr_static!("menu-nothing-selected")));
    };
    let mimetype = entry_mimetype(state.location(), first, mime_db);
    let candidates = apps_db.candidates_for(&mimetype);
    let default_id = apps_db.default_for(&mimetype).map(|entry| entry.id.clone());

    if candidates.is_empty() {
        return popover_container(t, empty_note(t, tr_static!("menu-no-known-app")));
    }

    let items: Vec<Element<'a, Message>> = candidates
        .into_iter()
        .map(|entry| {
            let is_default = default_id.as_deref() == Some(entry.id.as_str());
            open_with_row(t, entry, is_default)
        })
        .collect();

    popover_container(t, column(items).width(t.sizes.menu_width))
}

fn entry_mimetype(dir: &Location, entry: &FileEntry, mime_db: &MimeDb) -> String {
    if entry.kind == EntryKind::Directory {
        "inode/directory".to_owned()
    } else {
        mime_db.guess_entry(dir, &entry.name, entry.modified)
    }
}

/// Stage 12: delegates to `saola_theme::widget::menu_row` — the exact
/// upstream promotion of this crate's own local derivation (`style::
/// button::menu_row` + this constructor were "promoted from saola-files
/// `menus.rs`, two near-identical clones", per that helper's own doc
/// comment). Kept as a thin local wrapper (rather than rewriting every call
/// site's argument order) so every `menu_row(t, Icon::X, "label", message)`
/// call below stays unchanged.
fn menu_row<'a>(
    t: &'a Theme,
    glyph: Icon,
    label: &'a str,
    message: Message,
) -> Element<'a, Message> {
    let tint = saola_widget::role(t, Surface::Ink, Emphasis::Quiet);
    saola_widget::menu_row(t, Surface::Ink, Some(glyph), label, tint, Some(message))
}

fn open_with_row<'a>(
    t: &'a Theme,
    entry: &'a DesktopEntry,
    is_default: bool,
) -> Element<'a, Message> {
    let (glyph, tint) = if is_default {
        (Icon::Check, t.palette.accent.into_iced())
    } else {
        (
            Icon::ExternalLink,
            saola_widget::role(t, Surface::Ink, Emphasis::Quiet),
        )
    };
    saola_widget::menu_row(
        t,
        Surface::Ink,
        Some(glyph),
        &entry.name,
        tint,
        Some(Message::Menu(menu::Message::OpenWithChosen(
            entry.id.clone(),
        ))),
    )
}

fn empty_note<'a>(t: &'a Theme, message: &'a str) -> Element<'a, Message> {
    container(
        text(message)
            .size(t.typography.size.secondary)
            .font(convert::ui_font_regular(t))
            .color(t.on_ink.tertiary.into_iced()),
    )
    .padding(t.sizes.popover_padding / 2.0)
    .into()
}

fn popover_container<'a>(
    t: &'a Theme,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(content)
        .style(style::container::popover(t))
        .padding(t.sizes.popover_padding / 2.0)
        .into()
}

/// Whether `action` applies to a selection with resolved `mimetype` at
/// `scheme` — empty `mimetypes`/`schemes` means "applies everywhere" (per
/// `config.rs`'s own documented `[[action]]` semantics). Pure and total,
/// so it's unit-tested directly against `CustomAction` fixtures rather
/// than needing a real `DirectoryView`/`MimeDb`.
pub(super) fn action_applies(action: &CustomAction, mimetype: &str, scheme: &str) -> bool {
    let mime_ok = action.mimetypes.is_empty()
        || action
            .mimetypes
            .iter()
            .any(|pattern| mime_pattern_matches(pattern, mimetype));
    let scheme_ok = action.schemes.is_empty() || action.schemes.iter().any(|s| s == scheme);
    mime_ok && scheme_ok
}

/// Matches a config-file mimetype pattern against a resolved mimetype.
/// Supports the `"text/*"`-style top-level wildcard `config.rs`'s own
/// module docs show as an example; anything else is an exact match.
fn mime_pattern_matches(pattern: &str, mimetype: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(top) => mimetype.split('/').next() == Some(top),
        None => pattern == mimetype,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(selected: usize, clipboard_has_contents: bool, can_trash: bool) -> Vec<&'static str> {
        let ctx = MenuContext {
            selected,
            clipboard_has_contents,
            can_trash,
        };
        visible_entries(&ctx)
            .map(|entry| crate::i18n::lookup_static(entry.label_id))
            .collect()
    }

    // The expected lists below are the rows the pre-table, imperative
    // `context_menu` pushed for each case, in its order.

    #[test]
    fn menu_with_nothing_selected_and_an_empty_clipboard() {
        assert_eq!(
            labels(0, false, false),
            ["Open in terminal", "New Folder", "New File"]
        );
    }

    #[test]
    fn menu_with_nothing_selected_and_a_full_clipboard() {
        assert_eq!(
            labels(0, true, false),
            ["Open in terminal", "Paste", "New Folder", "New File"]
        );
    }

    #[test]
    fn menu_with_one_file_selected_on_a_trash_capable_backend() {
        assert_eq!(
            labels(1, false, true),
            [
                "Open",
                "Open with…",
                "Open in terminal",
                "Copy",
                "Cut",
                "Rename…",
                "Move to Trash",
                "New Folder",
                "New File",
                "Properties",
            ]
        );
    }

    #[test]
    fn menu_with_one_file_selected_without_trash_says_delete() {
        assert_eq!(
            labels(1, true, false),
            [
                "Open",
                "Open with…",
                "Open in terminal",
                "Copy",
                "Cut",
                "Paste",
                "Rename…",
                "Delete",
                "New Folder",
                "New File",
                "Properties",
            ]
        );
    }

    #[test]
    fn menu_with_several_selected_has_no_rename() {
        assert_eq!(
            labels(3, true, true),
            [
                "Open",
                "Open with…",
                "Open in terminal",
                "Copy",
                "Cut",
                "Paste",
                "Move to Trash",
                "New Folder",
                "New File",
                "Properties",
            ]
        );
    }

    #[test]
    fn every_row_sends_its_own_menu_message() {
        let ctx = MenuContext {
            selected: 1,
            clipboard_has_contents: true,
            can_trash: true,
        };
        let messages: Vec<String> = visible_entries(&ctx)
            .map(|entry| format!("{:?}", entry.message))
            .collect();
        assert_eq!(
            messages,
            [
                "OpenSelected",
                "OpenWithRequested",
                "OpenTerminalRequested",
                "CopyRequested",
                "CutRequested",
                "PasteRequested",
                "RenameRequested",
                "DeleteRequested",
                "NewFolderRequested",
                "NewFileRequested",
                "PropertiesRequested",
            ]
        );
    }

    fn action(mimetypes: &[&str], schemes: &[&str]) -> CustomAction {
        CustomAction {
            name: "Test action".to_owned(),
            exec: "true".to_owned(),
            mimetypes: mimetypes.iter().map(|s| s.to_string()).collect(),
            schemes: schemes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_filters_apply_everywhere() {
        assert!(action_applies(&action(&[], &[]), "text/plain", "file"));
        assert!(action_applies(&action(&[], &[]), "image/png", "sftp"));
    }

    #[test]
    fn exact_mimetype_filter_matches_only_that_mimetype() {
        let a = action(&["text/plain"], &[]);
        assert!(action_applies(&a, "text/plain", "file"));
        assert!(!action_applies(&a, "text/markdown", "file"));
    }

    #[test]
    fn wildcard_mimetype_filter_matches_the_whole_top_level_type() {
        let a = action(&["text/*"], &[]);
        assert!(action_applies(&a, "text/plain", "file"));
        assert!(action_applies(&a, "text/markdown", "file"));
        assert!(!action_applies(&a, "image/png", "file"));
    }

    #[test]
    fn scheme_filter_excludes_other_schemes() {
        let a = action(&[], &["sftp"]);
        assert!(action_applies(&a, "text/plain", "sftp"));
        assert!(!action_applies(&a, "text/plain", "file"));
    }

    #[test]
    fn mimetype_and_scheme_filters_both_must_pass() {
        let a = action(&["text/*"], &["file"]);
        assert!(action_applies(&a, "text/plain", "file"));
        assert!(!action_applies(&a, "text/plain", "sftp"));
        assert!(!action_applies(&a, "image/png", "file"));
    }
}
