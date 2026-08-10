//! The properties dialog (Alt+Enter / the context menu's "Properties",
//! Stage 13): name, mime/kind, a live streaming size count, modified date,
//! and (read-only today) permissions for the current selection. Built on
//! the Stage 12 dialog kit exactly like `ui::dialogs::conflict` — see that
//! module's doc comment for the shared conventions (`style::dialog::
//! surface`, the modal scrim, `sizes.dialog_width`) this one repeats
//! rather than re-derives.
//!
//! **Where the row data comes from.** `ui::dirview::DirectoryView::
//! properties_event` snapshots each selected entry's [`FileEntry`] (name,
//! kind, size, modified, permission bits) at the moment Alt+Enter/the menu
//! row fires — no extra `Backend::metadata` round trip, since the view
//! already has this in memory (`entries`). [`Properties`] carries that
//! snapshot untouched; the one thing that *is* fetched fresh is the size
//! row, because a directory's true size isn't `FileEntry::size` (a
//! directory entry's own `size` is the inode's, not its contents) — that's
//! `core::fs::size`'s job, streamed in by `main.rs::App` exactly like
//! `ui::dialogs::progress` streams in `core::fs::ops`'s copy/move events:
//! same bounded-bridge-plus-`AtomicBool` shape, different engine.
//!
//! **Permissions are read-only.** `core::vfs::Caps::SET_PERMISSIONS` is
//! still "a future trait method — no backend sets this bit yet" (see that
//! flag's own doc comment) — there is no `Backend::set_permissions` to call
//! even if this rendered an editable control. [`Properties::caps`] is
//! still threaded through (not just discarded) so a future stage that adds
//! that trait method and flips the bit on a real backend only has to touch
//! this row's rendering, not invent the plumbing to reach it.

use iced::widget::{Space, button, column, container, row, text};
use iced::{Center, Element, Fill, Length, Subscription};
use saola_theme::{ColorExt, Surface, Theme, convert, style, widget};

use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::fs::format::{format_system_time, human_size};
use crate::core::fs::size::{self, SizeEvent, SizeRequest};
use crate::core::mime::MimeDb;
use crate::core::vfs::{Caps, Location};

/// The label column's fixed width — layout-specific to this dialog's own
/// two-column row shape, not a saola-theme design-system size (same
/// distinction `ui::dirview::list`'s `SIZE_COLUMN`/`DATE_COLUMN` draw for
/// their own local layout constants).
const LABEL_COLUMN: f32 = 92.0;

#[derive(Debug, Clone, Copy)]
pub enum Message {
    /// The footer's "Close" button, or a click on the modal scrim
    /// (`main.rs::App::view` wires both to this) — unlike the conflict
    /// dialog, there's a sane default here (just close it: nothing this
    /// dialog shows is a decision that needs an explicit answer), so
    /// dismissing it any of the usual ways is fine.
    CloseRequested,
}

/// The dialog's own render-time snapshot — `main.rs::App` owns the actual
/// state (`items` set once at open time, `size_files`/`size_bytes`/
/// `size_done` mutated live off `core::fs::size::SizeEvent`s, mirroring how
/// `ui::dialogs::progress::Progress` is accumulated by `App::
/// handle_op_event`). This module only ever renders it.
pub struct Properties {
    /// Each selected entry, paired with its own location — see the module
    /// doc comment for where this snapshot comes from. Never empty: `App::
    /// open_properties` doesn't construct one for an empty selection.
    pub items: Vec<(Location, FileEntry)>,
    /// The backend's capabilities for `items`' shared directory — see the
    /// module doc comment on why the permissions row reads this and still
    /// always renders read-only today.
    pub caps: Caps,
    /// The live streaming size count (`core::fs::size::SizeEvent::
    /// Progress`/`Finished`/`Cancelled`'s running totals) — `0`/`0` until
    /// the first event lands, which [`Self::size_text`] reads as "still
    /// calculating" rather than "empty".
    pub size_files: usize,
    pub size_bytes: u64,
    /// Set once a `Finished`/`Cancelled` event lands — see `size_files`'
    /// doc comment for why this, not `size_files == 0`, is what
    /// distinguishes "still calculating" from "genuinely empty".
    pub size_done: bool,
}

impl Properties {
    pub fn new(items: Vec<(Location, FileEntry)>, caps: Caps) -> Self {
        Properties {
            items,
            caps,
            size_files: 0,
            size_bytes: 0,
            size_done: false,
        }
    }

    /// The size row's value text — "Calculating…" until the first event
    /// lands, then "12.3 MB — 4 items" (singular "item" for exactly one),
    /// updated in place as later events arrive. Kept as a method (not a
    /// free function) purely so it reads at the call site as "this
    /// snapshot's own size text", matching `ui::dialogs::progress::
    /// Progress::percent`'s shape.
    fn size_text(&self) -> String {
        if !self.size_done && self.size_files == 0 && self.size_bytes == 0 {
            return "Calculating…".to_owned();
        }
        let noun = if self.size_files == 1 {
            "item"
        } else {
            "items"
        };
        format!(
            "{} — {} {noun}",
            human_size(self.size_bytes),
            self.size_files
        )
    }
}

/// Bridges `core::fs::size::run`'s plain `BoxStream` into an
/// `iced::Subscription`, identified by `request` (its manual `Hash`-by-`id`
/// — see that type's doc comment) — the exact same shape `ui::dialogs::
/// progress::subscription` already draws over `ops::run`, just keyed by a
/// `SizeRequest` instead of an `OpRequest`.
pub fn subscription(request: &SizeRequest) -> Subscription<SizeEvent> {
    Subscription::run_with(request.clone(), size::run)
}

/// Renders the modal: title (the entry's name, or "N items selected"),
/// location/kind/size/modified/permissions rows (the last three only for a
/// single-item selection — see the module doc comment), and a footer strip
/// holding "Close". `main.rs::App::view` supplies the surrounding scrim and
/// centering, the same split `ui::dialogs::conflict::view` already takes.
pub fn view<'a>(
    t: &'a Theme,
    mime_db: &'a MimeDb,
    properties: &'a Properties,
) -> Element<'a, Message> {
    let title_text = match properties.items.as_slice() {
        [(_, entry)] => entry.display_name().into_owned(),
        items => format!("{} items selected", items.len()),
    };
    let title = text(title_text)
        .size(t.typography.size.dialog_title)
        .font(convert::display_font(t))
        .color(t.on_paper.primary.into_iced());

    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    if let Some(parent) = properties.items.first().and_then(|(loc, _)| loc.parent()) {
        rows.push(info_row(t, "Location", parent.to_string(), false));
    }
    if let [(_, entry)] = properties.items.as_slice() {
        rows.push(info_row(t, "Kind", kind_text(entry, mime_db), false));
    }
    rows.push(info_row(t, "Size", properties.size_text(), true));
    if let [(_, entry)] = properties.items.as_slice() {
        if let Some(modified) = entry.modified {
            rows.push(info_row(t, "Modified", format_system_time(modified), true));
        }
        if let Some(mode) = entry.mode {
            rows.push(info_row(t, "Permissions", format_mode(mode), true));
        }
    }

    let content = column![title, column(rows).spacing(t.sizes.gap_tight), footer(t)]
        .spacing(t.sizes.popover_padding / 2.0)
        .width(Length::Fixed(t.sizes.dialog_width));

    // Same recipe `ui::dialogs::conflict::view` uses — see its own comment
    // on why `style::dialog::surface` takes no `Surface` parameter.
    container(content)
        .style(style::dialog::surface(t))
        .padding(t.sizes.popover_padding)
        .into()
}

/// One label/value row. `mono` picks the mono face for the value — the
/// style guide's "tabular numerals on size and date columns" rule, applied
/// to this dialog's Size/Modified rows the same way `ui::dirview::list`
/// applies it to its own size/date columns.
fn info_row<'a>(t: &'a Theme, label: &'a str, value: String, mono: bool) -> Element<'a, Message> {
    let value_font = if mono {
        convert::mono_font(t)
    } else {
        convert::ui_font(t)
    };
    row![
        text(label)
            .size(t.typography.size.secondary)
            .font(convert::ui_font_regular(t))
            .color(t.on_paper.secondary.into_iced())
            .width(Length::Fixed(LABEL_COLUMN)),
        text(value)
            .size(t.typography.size.secondary)
            .font(value_font)
            .color(t.on_paper.primary.into_iced())
            .width(Fill),
    ]
    .spacing(t.sizes.pill_gap)
    .into()
}

fn footer<'a>(t: &'a Theme) -> Element<'a, Message> {
    let close = button(
        text("Close")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper))
    .padding(t.paddings.dialog_button)
    .on_press(Message::CloseRequested);

    widget::footer_strip(
        t,
        Surface::Paper,
        row![Space::new().width(Fill), close]
            .width(Fill)
            .align_y(Center),
    )
}

/// `entry`'s kind, worded for a human: "Folder" for a directory, "Symlink"
/// for anything `symlink_metadata` reported as one (`EntryKind` itself
/// can't tell a symlink's *target* kind — see that type's own doc comment
/// — so this doesn't guess either), otherwise the name-guessed mimetype
/// string, the same resolution `ui::menus`'s private `entry_mimetype`
/// already does for the context menu (not reused directly — that helper
/// isn't `pub(crate)`, and duplicating one `match` is cheaper than widening
/// its visibility for a single other caller).
fn kind_text(entry: &FileEntry, mime_db: &MimeDb) -> String {
    if entry.is_symlink {
        return "Symlink".to_owned();
    }
    match entry.kind {
        EntryKind::Directory => "Folder".to_owned(),
        _ => mime_db.guess(&entry.name, None),
    }
}

/// `st_mode & 0o7777` -> `"rwxr-xr-x (755)"` — the traditional symbolic
/// permission string beside its octal form. Only the low 9 bits
/// (owner/group/other rwx) get a symbolic letter; setuid/setgid/sticky
/// (bits 9-11) still show up in the octal number but not as `s`/`t`
/// characters — a deliberate simplification (this dialog is read-only
/// display, not a `chmod` editor — see the module doc comment) rather than
/// a gap to fill in later.
fn format_mode(mode: u32) -> String {
    let mode = mode & 0o7777;
    let bit = |shift: u32, ch: char| if mode & (1 << shift) != 0 { ch } else { '-' };
    let symbolic: String = [
        bit(8, 'r'),
        bit(7, 'w'),
        bit(6, 'x'),
        bit(5, 'r'),
        bit(4, 'w'),
        bit(3, 'x'),
        bit(2, 'r'),
        bit(1, 'w'),
        bit(0, 'x'),
    ]
    .into_iter()
    .collect();
    format!("{symbolic} ({mode:03o})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn entry(name: &str, kind: EntryKind, size: u64, mode: Option<u32>) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind,
            size,
            modified: None,
            is_symlink: false,
            mode,
        }
    }

    // ── permission-display formatting ───────────────────────────────────

    #[test]
    fn format_mode_renders_owner_group_other_symbolically() {
        assert_eq!(format_mode(0o755), "rwxr-xr-x (755)");
        assert_eq!(format_mode(0o644), "rw-r--r-- (644)");
        assert_eq!(format_mode(0), "--------- (000)");
        assert_eq!(format_mode(0o777), "rwxrwxrwx (777)");
    }

    #[test]
    fn format_mode_masks_off_bits_above_0o7777() {
        // A stray file-type bit above 0o7777 (as `st_mode` carries them,
        // even though `modules::local::entry_from_metadata` already masks
        // them off before this ever sees it) must not leak into the octal
        // form or desync the symbolic one.
        assert_eq!(format_mode(0o100_644), "rw-r--r-- (644)");
    }

    #[test]
    fn format_mode_shows_setuid_bit_only_in_the_octal_form() {
        // 0o4755: setuid + rwxr-xr-x. The symbolic string is deliberately
        // still the plain 9-character rwx form (see the function's doc
        // comment) — only the octal number reflects the setuid bit.
        assert_eq!(format_mode(0o4755), "rwxr-xr-x (4755)");
    }

    // ── kind_text ────────────────────────────────────────────────────────

    #[test]
    fn kind_text_words_a_directory_as_folder() {
        let mime_db = MimeDb::new();
        let dir = entry("docs", EntryKind::Directory, 0, None);
        assert_eq!(kind_text(&dir, &mime_db), "Folder");
    }

    #[test]
    fn kind_text_words_a_symlink_regardless_of_kind() {
        let mime_db = MimeDb::new();
        let mut link = entry("link", EntryKind::Other, 0, None);
        link.is_symlink = true;
        assert_eq!(kind_text(&link, &mime_db), "Symlink");
    }

    // ── Properties::size_text ───────────────────────────────────────────

    #[test]
    fn size_text_reads_calculating_before_the_first_event() {
        let properties = Properties::new(
            vec![(Location::local("/a"), entry("a", EntryKind::File, 0, None))],
            Caps::empty(),
        );
        assert_eq!(properties.size_text(), "Calculating…");
    }

    #[test]
    fn size_text_counts_up_as_progress_events_land() {
        let mut properties = Properties::new(
            vec![(Location::local("/a"), entry("a", EntryKind::File, 0, None))],
            Caps::empty(),
        );
        properties.size_files = 3;
        properties.size_bytes = 1024;
        assert_eq!(properties.size_text(), "1.0 KB — 3 items");
    }

    #[test]
    fn size_text_uses_singular_item_for_exactly_one() {
        let mut properties = Properties::new(
            vec![(Location::local("/a"), entry("a", EntryKind::File, 5, None))],
            Caps::empty(),
        );
        properties.size_files = 1;
        properties.size_bytes = 5;
        properties.size_done = true;
        assert_eq!(properties.size_text(), "5 B — 1 item");
    }

    #[test]
    fn size_text_reads_zero_once_done_rather_than_calculating() {
        // An empty directory finishes at 0 files/0 bytes — `size_done`
        // (not `size_files == 0`) is what must distinguish this from
        // "still calculating".
        let mut properties = Properties::new(
            vec![(
                Location::local("/empty"),
                entry("empty", EntryKind::Directory, 0, None),
            )],
            Caps::empty(),
        );
        properties.size_done = true;
        assert_eq!(properties.size_text(), "0 B — 0 items");
    }
}
