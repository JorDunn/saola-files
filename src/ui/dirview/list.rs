//! The virtualized row list: iced 0.14 has no virtualized list widget, so
//! this renders only the `visible[first..last]` slice actually on screen,
//! between two `Space` spacers sized `rows_outside × sizes.list_row`.
//! Every row is the fixed `sizes.list_row` height, which is what makes the
//! spacer math exact. Never build one `Element` per entry — a 100k-entry
//! directory must still only ever construct a screenful.
//!
//! Column headers (name/size/date) set sort on click; size/date use the
//! mono face, which is tabular by construction (see saola-theme's
//! `ui_font` docs) — the style guide's "tabular numerals on size and date
//! columns" rule.

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, text, text_input,
};
use iced::{Element, Fill, Length};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::config::SortKey;
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::fs::format::{format_system_time, human_size};
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;

use super::rename::RENAME_INPUT_ID;
use super::{DirectoryView, Message, row_icon, thumbnail_for};

/// Extra rows rendered above/below the on-screen slice so a fast scroll
/// flick doesn't show a blank frame before the next `Scrolled` message
/// updates the range.
const OVERSCAN_ROWS: usize = 4;

/// How many rows to render before the first `Scrolled` event arrives
/// (nothing has reported a viewport height yet).
const INITIAL_ROWS: usize = 64;

/// Column widths. Layout-specific to this file manager's row shape, not a
/// saola-theme design-system size — same distinction `window.rs` draws
/// for `RESIZE_EDGE`/`RESIZE_CORNER`. The name column has no fixed width;
/// it's the `Fill` column.
const SIZE_COLUMN: f32 = 84.0;
const DATE_COLUMN: f32 = 168.0;

pub(super) fn view<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
) -> Element<'a, Message> {
    if let Some(err) = &state.error {
        // `err.to_string()` is owned, so `empty_state` gets a `String` it
        // can consume into a `text` widget directly — a `&str` borrowing
        // this temporary can't outlive this function, but the returned
        // `Element<'a, _>` must.
        return empty_state_owned(t, err.to_string());
    }
    if state.entries.is_empty() {
        let message = if state.loading {
            "Loading…"
        } else {
            "This folder is empty"
        };
        return empty_state(t, message);
    }

    let row_height = t.sizes.list_row;
    let total = state.visible.len();
    let (first, last) = visible_range(state, row_height, total);

    let before = Space::new().height(row_height * first as f32);
    let after = Space::new().height(row_height * total.saturating_sub(last) as f32);

    let rows = state
        .visible
        .get(first..last)
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .filter_map(|(offset, &entry_index)| {
            state
                .entries
                .get(entry_index)
                .map(|entry| entry_row(state, t, mime_db, thumb_cache, first + offset, entry))
        });

    let body_column = column(
        std::iter::once(before.into())
            .chain(rows)
            .chain(std::iter::once(after.into())),
    )
    .width(Fill);

    let body = scrollable(body_column)
        .on_scroll(Message::Scrolled)
        .style(style::scrollable::rest(t, Surface::Paper))
        .width(Fill)
        .height(Fill);

    column![header_row(state, t), body]
        .width(Fill)
        .height(Fill)
        .into()
}

/// Which slice of `visible` is on (or near) screen, from the last known
/// scroll viewport. Before any `Scrolled` event has landed, renders a
/// first screenful so the initial frame isn't blank.
fn visible_range(state: &DirectoryView, row_height: f32, total: usize) -> (usize, usize) {
    if row_height <= 0.0 {
        return (0, total.min(INITIAL_ROWS));
    }
    let Some(viewport) = state.scroll else {
        return (0, total.min(INITIAL_ROWS));
    };
    let offset = viewport.absolute_offset().y.max(0.0);
    let visible_rows = (viewport.bounds().height / row_height).ceil().max(0.0) as usize;
    let first = ((offset / row_height).floor() as usize).saturating_sub(OVERSCAN_ROWS);
    let last = (first
        .saturating_add(visible_rows)
        .saturating_add(OVERSCAN_ROWS * 2))
    .min(total);
    (first.min(last), last)
}

fn header_row<'a>(state: &'a DirectoryView, t: &'a Theme) -> Element<'a, Message> {
    use iced::alignment::Horizontal;

    // Each header is aligned the way its own column's values are (see
    // `entry_row`): Name and Date read left-to-right, Size is right-aligned
    // so the digits line up under each other.
    let name = header_cell(t, "Name", SortKey::Name, state, Fill, Horizontal::Left);
    let size = header_cell(
        t,
        "Size",
        SortKey::Size,
        state,
        Length::Fixed(SIZE_COLUMN),
        Horizontal::Right,
    );
    let date = header_cell(
        t,
        "Date modified",
        SortKey::Modified,
        state,
        Length::Fixed(DATE_COLUMN),
        Horizontal::Left,
    );

    // `align_y` on the container, *not* only on the row: a `container`
    // hands its child loose limits and then aligns the resulting node, so
    // this is the one place in this row that can actually centre the
    // header cells inside the `list_row`-tall band. Without it they sit
    // flush against the band's top edge.
    container(row![name, size, date].align_y(iced::Center))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding([0.0, t.sizes.pill_gap])
        .align_y(iced::Center)
        .into()
}

fn header_cell<'a>(
    t: &'a Theme,
    label: &'a str,
    key: SortKey,
    state: &DirectoryView,
    width: Length,
    align: iced::alignment::Horizontal,
) -> Element<'a, Message> {
    let name = text(label)
        .size(t.typography.size.label)
        .font(convert::mono_font_medium(t));

    // `arrow-down-a-z`/`arrow-up-a-z` (style guide's sort-direction pair)
    // stand in for direction on every sortable column, not just Name —
    // the shape (ascending vs descending), not the literal "A-Z" reading,
    // is what a Size/Date column borrows from them.
    let content: Element<'a, Message> = if state.sort == key {
        let glyph = if state.sort_descending {
            Icon::ArrowDownAZ
        } else {
            Icon::ArrowUpAZ
        };
        row![
            name,
            icon::icon(glyph, t.sizes.icon_row, t.on_paper.secondary.into_iced()),
        ]
        .spacing(t.sizes.gap_tight)
        .align_y(iced::Center)
        .into()
    } else {
        name.into()
    };

    // A `button` never aligns its own content — it places it at the
    // padding's top-left corner (see `entry_row`'s note) — so a header
    // cell with an explicit column width would always render its label
    // hard against the column's left edge. The `container` fills the
    // button's inner width and does the aligning instead.
    button(container(content).width(Fill).align_x(align))
        .style(style::button::bare(t, Surface::Paper))
        .on_press(Message::HeaderClicked(key))
        .width(width)
        .into()
}

fn entry_row<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
    visible_index: usize,
    entry: &'a FileEntry,
) -> Element<'a, Message> {
    // Stage 8: a row mid-inline-rename swaps its label for a `text_input`
    // and stops being a clickable button entirely (there is nothing sane
    // for a click on a field mid-edit to do) — checked by name, matching
    // `RenameState::original`'s own doc comment on why the target is fixed
    // by name rather than by (possibly now-stale) row position.
    if let Some(rename) = state.rename_state()
        && rename.original == entry.name
    {
        return renaming_row(t, mime_db, entry, rename);
    }

    let selected = state.selection.is_selected(&entry.name);
    let has_cursor = state.selection.cursor() == Some(visible_index);

    // Glyph shape carries type, never hue (style guide §1) — the icon's
    // tint only ever follows selected/not-selected (the same two-value
    // split `row_style` below draws the row background/text from); unlike
    // the row's own button chrome it can't also brighten on hover, since
    // an `svg::Style` closure is fixed at build time, not re-evaluated
    // per `button::Status`.
    let icon_color = if selected {
        t.palette.paper
    } else {
        t.on_paper.primary
    };
    // Stage 11: a cached thumbnail (regular files only — see
    // `DirectoryView::thumbnail_candidates`, the only producer of what
    // fills `thumb_cache`) replaces the glyph for this one row; everything
    // else (directories, symlinks, unsupported mimetypes, a cache miss
    // still in flight) falls back to the glyph exactly as before this
    // stage. Scaled to the same `sizes.icon_row` box the glyph already
    // draws in — no separate thumbnail-specific size token needed here.
    let icon: Element<'a, Message> = match thumbnail_for(state, thumb_cache, entry) {
        Some(handle) => iced::widget::image(handle.handle())
            .width(t.sizes.icon_row)
            .height(t.sizes.icon_row)
            .into(),
        None => icon::icon(
            row_icon(entry, mime_db),
            t.sizes.icon_row,
            icon_color.into_iced(),
        )
        .into(),
    };

    let name = text(entry.display_name().into_owned())
        .size(t.typography.size.body)
        .font(convert::ui_font(t));

    let name_row = row![icon, name]
        .spacing(t.sizes.pill_gap)
        .align_y(iced::Center);

    let size = text(size_text(entry))
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t));

    let date = text(date_text(entry))
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t));

    // `.height(Fill)` is what actually centres this row's cells inside the
    // `list_row`-tall button below. An iced `button` lays its content out
    // at the padding's top-left corner and never aligns it
    // (`layout::padded` -> `layout::positioned`), so a `Shrink`-height row
    // ends up pinned to the top of the 38px row with `align_y(Center)`
    // only centring the cells against each other. Filling the height gives
    // `align_y` the button's full height to centre within.
    let content = row![
        container(name_row).width(Fill),
        container(size)
            .width(Length::Fixed(SIZE_COLUMN))
            .align_x(iced::alignment::Horizontal::Right),
        container(date).width(Length::Fixed(DATE_COLUMN)),
    ]
    .height(Fill)
    .align_y(iced::Center)
    .padding([0.0, t.sizes.pill_gap]);

    let styled = button(content)
        .style(style::button::list_row(
            t,
            Surface::Paper,
            selected,
            has_cursor,
        ))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding(0)
        .on_press(Message::RowClicked(visible_index));

    mouse_area(styled)
        .on_double_click(Message::RowDoubleClicked(visible_index))
        .into()
}

/// The inline-rename presentation of one row: the ordinary glyph, plus a
/// `text_input` in place of the name label. Size/date columns stay put so
/// the row's width/height don't jump while editing; the date column shows
/// `rename.error` instead of the entry's date when a previous submit
/// failed (CLAUDE.md's capability-honest wording — a rejected rename is
/// worded inline, never a color change or a modal).
fn renaming_row<'a>(
    t: &'a Theme,
    mime_db: &'a MimeDb,
    entry: &'a FileEntry,
    rename: &'a super::rename::RenameState,
) -> Element<'a, Message> {
    let icon = icon::icon(
        row_icon(entry, mime_db),
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    );

    let field = text_input("Name", &rename.buffer)
        .id(RENAME_INPUT_ID)
        .on_input(Message::RenameChanged)
        .on_submit(Message::RenameSubmitted)
        .style(style::text_input::rest(t, Surface::Paper))
        .font(convert::ui_font(t))
        .size(t.typography.size.body)
        .width(Fill);

    let name_row = row![icon, field]
        .spacing(t.sizes.pill_gap)
        .align_y(iced::Center);

    let trailing_text = rename.error.clone().unwrap_or_else(|| date_text(entry));
    let trailing = text(trailing_text)
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t));
    let size = text(size_text(entry))
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t));

    let content = row![
        container(name_row).width(Fill),
        container(size)
            .width(Length::Fixed(SIZE_COLUMN))
            .align_x(iced::alignment::Horizontal::Right),
        container(trailing).width(Length::Fixed(DATE_COLUMN)),
    ]
    .align_y(iced::Center)
    .padding([0.0, t.sizes.pill_gap]);

    // `align_y` so a row mid-rename sits on the same centre line as the
    // ordinary rows around it (a `container` — unlike a `button` — does
    // align its child, so this is all that's needed here).
    container(content)
        .width(Fill)
        .height(t.sizes.list_row)
        .align_y(iced::Center)
        .into()
}

fn empty_state<'a>(t: &'a Theme, message: &'a str) -> Element<'a, Message> {
    saola_theme::widget::empty_state(t, Surface::Paper, message)
}

fn empty_state_owned<'a>(t: &'a Theme, message: String) -> Element<'a, Message> {
    container(
        text(message)
            .size(t.typography.size.secondary)
            .font(convert::ui_font_regular(t))
            .color(t.on_paper.tertiary.into_iced()),
    )
    .width(Fill)
    .height(Fill)
    .align_x(iced::alignment::Horizontal::Center)
    .align_y(iced::alignment::Vertical::Center)
    .into()
}

fn size_text(entry: &FileEntry) -> String {
    if entry.kind == EntryKind::Directory {
        String::new()
    } else {
        human_size(entry.size)
    }
}

fn date_text(entry: &FileEntry) -> String {
    match entry.modified {
        Some(time) => format_system_time(time),
        None => String::new(),
    }
}
