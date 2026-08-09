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
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::config::SortKey;
use crate::core::fs::entry::{EntryKind, FileEntry};
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::icons::{self, Icon};

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
    let name = header_cell(t, "Name", SortKey::Name, state, Fill);
    let size = header_cell(t, "Size", SortKey::Size, state, Length::Fixed(SIZE_COLUMN));
    let date = header_cell(
        t,
        "Date modified",
        SortKey::Modified,
        state,
        Length::Fixed(DATE_COLUMN),
    );

    container(row![name, size, date].align_y(iced::Center))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding([0.0, t.sizes.pill_gap])
        .into()
}

fn header_cell<'a>(
    t: &'a Theme,
    label: &'a str,
    key: SortKey,
    state: &DirectoryView,
    width: Length,
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
            icons::icon(glyph, t.sizes.icon_row, t.on_paper.secondary.into_iced()),
        ]
        .spacing(4.0)
        .align_y(iced::Center)
        .into()
    } else {
        name.into()
    };

    button(content)
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
        None => icons::icon(
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

    let content = row![
        container(name_row).width(Fill),
        container(size)
            .width(Length::Fixed(SIZE_COLUMN))
            .align_x(iced::alignment::Horizontal::Right),
        container(date).width(Length::Fixed(DATE_COLUMN)),
    ]
    .align_y(iced::Center)
    .padding([0.0, t.sizes.pill_gap]);

    let styled = button(content)
        .style(row_style(t, selected, has_cursor))
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
    let icon = icons::icon(
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

    container(content)
        .width(Fill)
        .height(t.sizes.list_row)
        .into()
}

/// §6 "List row" (`docs/SAOLA-STYLE-GUIDE.md`): height `sizes.list_row`
/// (38px, inside the spec's 36–42px range), `radii.pill`, transparent at
/// rest, `fill_subtle` on hover, terracotta when selected with ivory
/// text. The keyboard cursor (a concept distinct from selection — iced
/// buttons have no `Status::Focused`, so it's app state) draws
/// `style::focus_border` around the row it sits on.
///
/// **Upstream gap** (verified against the pinned `saola-theme-v0.5.0`
/// tag): there is no `style::button::list_row` helper yet, so this is
/// derived locally from the `rest`/`active` recipes in saola-theme's
/// `style/button.rs`. TODO(saola-theme): promote this to
/// `style::button::list_row(t, Surface, selected: bool)` upstream and
/// delete this function once a new tag ships it; bump the pinned tag in
/// this crate's `Cargo.toml` in the same PR that adopts it.
fn row_style(
    t: &Theme,
    selected: bool,
    has_cursor: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    let radius = t.radii.pill;
    let on = t.on_paper;
    let accent = t.palette.accent;
    let paper_text = t.palette.paper;
    let focus = style::focus_border(t, radius);

    move |_, status| {
        let (background, text_color) = if selected {
            let bg = match status {
                button::Status::Hovered => on.fill_subtle.over(accent),
                button::Status::Pressed => on.fill.over(accent),
                _ => accent,
            };
            (Some(bg), paper_text)
        } else {
            let bg = match status {
                button::Status::Hovered => Some(on.fill_subtle),
                button::Status::Pressed => Some(on.fill),
                _ => None,
            };
            (bg, on.primary)
        };

        button::Style {
            background: background.map(|color| iced::Background::Color(color.into_iced())),
            text_color: text_color.into_iced(),
            border: if has_cursor {
                focus
            } else {
                iced::Border {
                    color: iced::Color::TRANSPARENT,
                    width: 0.0,
                    radius: radius.into(),
                }
            },
            ..button::Style::default()
        }
    }
}

fn empty_state<'a>(t: &'a Theme, message: &'a str) -> Element<'a, Message> {
    empty_state_owned(t, message.to_owned())
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

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    let label = UNITS.get(unit).copied().unwrap_or("TB");
    format!("{size:.1} {label}")
}

fn date_text(entry: &FileEntry) -> String {
    match entry.modified {
        Some(time) => format_system_time(time),
        None => String::new(),
    }
}

/// A minimal, dependency-free "YYYY-MM-DD HH:MM" formatter for
/// `SystemTime`. Not a general calendar library — just enough to label a
/// list row; a real date/time crate can replace this if a later stage
/// needs more (relative "2 days ago" phrasing, locale-aware formats, …).
fn format_system_time(time: std::time::SystemTime) -> String {
    let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new(); // times before 1970 aren't worth a crate; blank is honest
    };
    let secs = duration.as_secs();
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (hour, minute) = (time_of_day / 3600, (time_of_day % 3600) / 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`: days-since-1970-01-01 (proleptic
/// Gregorian) -> `(year, month, day)`. A well-known, correct, allocation-
/// and dependency-free algorithm; see
/// <http://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 MB");
    }
}
