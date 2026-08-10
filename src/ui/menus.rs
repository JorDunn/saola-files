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
//! **Anchoring is simplified too:** a precise "grow from the trigger
//! button" popover (style guide §6) needs the trigger's on-screen rect,
//! which iced 0.14's plain `view()` has no way to learn without threading
//! measured layout state through — out of scope here. Both popovers below
//! anchor to the window's top-right corner instead, roughly under where
//! the header's overflow button always renders.

use iced::widget::{Space, column, container, mouse_area, stack, text};
use iced::{Element, Fill};
use saola_theme::icon::Icon;
use saola_theme::widget::{self as saola_widget, Emphasis};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::config::CustomAction;
use crate::core::apps::{AppsDb, DesktopEntry};
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::mime::MimeDb;
use crate::core::vfs::Caps;

use super::dirview::{DirectoryView, Message};

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
    if state.open_with_open() {
        return popover_stack(t, content, open_with_popover(t, state, mime_db, apps_db));
    }
    if state.menu_open() {
        return popover_stack(
            t,
            content,
            context_menu(t, state, mime_db, clipboard_has_contents),
        );
    }
    content
}

fn popover_stack<'a>(
    t: &'a Theme,
    content: Element<'a, Message>,
    popover: Element<'a, Message>,
) -> Element<'a, Message> {
    let scrim = mouse_area(Space::new().width(Fill).height(Fill)).on_press(Message::CloseMenu);
    let anchored = container(popover)
        .width(Fill)
        .height(Fill)
        .padding(iced::Padding {
            top: t.sizes.window_header,
            right: t.sizes.pill_gap,
            bottom: 0.0,
            left: 0.0,
        })
        .align_x(iced::alignment::Horizontal::Right);
    stack![content, scrim, anchored].into()
}

fn context_menu<'a>(
    t: &'a Theme,
    state: &'a DirectoryView,
    mime_db: &'a MimeDb,
    clipboard_has_contents: bool,
) -> Element<'a, Message> {
    let selected = state.selected_entries();
    let mut items: Vec<Element<'a, Message>> = Vec::new();

    if !selected.is_empty() {
        items.push(menu_row(
            t,
            Icon::ExternalLink,
            "Open",
            Message::MenuOpenSelected,
        ));
        items.push(menu_row(
            t,
            Icon::Ellipsis,
            "Open with…",
            Message::MenuOpenWithRequested,
        ));
    }
    items.push(menu_row(
        t,
        Icon::Terminal,
        "Open in terminal",
        Message::MenuOpenTerminalRequested,
    ));

    // ── Stage 8: clipboard / rename / new ────────────────────────────────
    if !selected.is_empty() {
        items.push(menu_row(t, Icon::Copy, "Copy", Message::MenuCopyRequested));
        items.push(menu_row(
            t,
            Icon::Scissors,
            "Cut",
            Message::MenuCutRequested,
        ));
    }
    if clipboard_has_contents {
        items.push(menu_row(
            t,
            Icon::ClipboardPaste,
            "Paste",
            Message::MenuPasteRequested,
        ));
    }
    if selected.len() == 1 {
        items.push(menu_row(
            t,
            Icon::Pencil,
            "Rename…",
            Message::MenuRenameRequested,
        ));
    }
    // ── Stage 9: delete / trash ──────────────────────────────────────────
    if !selected.is_empty() {
        let label = if state.caps().contains(Caps::TRASH) {
            "Move to Trash"
        } else {
            "Delete"
        };
        items.push(menu_row(
            t,
            Icon::Trash2,
            label,
            Message::MenuDeleteRequested,
        ));
    }
    items.push(menu_row(
        t,
        Icon::FolderPlus,
        "New Folder",
        Message::MenuNewFolderRequested,
    ));
    items.push(menu_row(
        t,
        Icon::FilePlus,
        "New File",
        Message::MenuNewFileRequested,
    ));
    // ── Stage 13: properties ─────────────────────────────────────────────
    if !selected.is_empty() {
        items.push(menu_row(
            t,
            Icon::Info,
            "Properties",
            Message::MenuPropertiesRequested,
        ));
    }

    if !selected.is_empty() {
        let scheme = state.location().scheme.clone();
        let mimetypes: Vec<String> = selected
            .iter()
            .map(|entry| entry_mimetype(entry, mime_db))
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
                    Message::MenuCustomActionRequested(index),
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
        return popover_container(t, empty_note(t, "Nothing selected"));
    };
    let mimetype = entry_mimetype(first, mime_db);
    let candidates = apps_db.candidates_for(&mimetype);
    let default_id = apps_db.default_for(&mimetype).map(|entry| entry.id.clone());

    if candidates.is_empty() {
        return popover_container(t, empty_note(t, "No known app for this file type"));
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

fn entry_mimetype(entry: &FileEntry, mime_db: &MimeDb) -> String {
    if entry.kind == EntryKind::Directory {
        "inode/directory".to_owned()
    } else {
        mime_db.guess(&entry.name, None)
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
        Some(Message::OpenWithChosen(entry.id.clone())),
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
