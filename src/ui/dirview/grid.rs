//! Grid view: fixed-size glyph tiles, virtualized by *row of tiles* the
//! same way `list.rs` virtualizes by row of text — only the row band
//! actually on (or near) screen is ever built into `Element`s. CLAUDE.md's
//! "no virtualized list widget, never build 100k Elements" rule applies to
//! every directory view, not just the list.
//!
//! Column count is a fixed placeholder ([`GRID_COLUMNS`]), not derived
//! from the viewport's actual measured width: iced 0.14's `view()` has no
//! way to learn a container's pixel width before layout runs, and wiring
//! real responsive columns means threading `window::resize` events down
//! into `DirectoryView` state — out of scope for this stage. A later
//! stage can replace the constant with that without touching the
//! virtualization math below (it only cares about "how many tiles per
//! row", not where that number comes from).

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, text, text_input,
};
use iced::{Center, Element, Fill, Length};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::entry::FileEntry;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::icons;

use super::rename::RENAME_INPUT_ID;
use super::{DirectoryView, Message, row_icon, thumbnail_for};

/// Tiles per row. See the module docs — this is a placeholder, not a
/// layout measurement. `pub(super)` so `DirectoryView::row_step` (the
/// Up/Down/PageUp/PageDown cursor math) can step by a full visual row in
/// grid mode instead of by one item at a time.
pub(super) const GRID_COLUMNS: usize = 6;

/// The glyph square's side length.
///
/// **Upstream gap** (verified against the pinned `saola-theme-v0.5.0`
/// tag — same posture as `list.rs`'s `row_style` TODO, and already
/// anticipated in the Stage 3 handoff's upstream-debt list): there is no
/// `sizes.grid_tile` token yet. TODO(saola-theme): promote this to
/// `sizes.grid_tile` and delete the local constant once a new tag ships
/// it; bump the pinned tag in this crate's `Cargo.toml` in the same PR.
const GRID_TILE_SIZE: f32 = 96.0;

/// Extra height below the glyph square reserved for the name label.
const GRID_LABEL_HEIGHT: f32 = 34.0;

/// Gap between tiles, both axes, and between the tile and its own border.
/// Same upstream-gap posture as `GRID_TILE_SIZE` — pending
/// `sizes.grid_tile_gap`.
const GRID_GAP: f32 = 12.0;

/// Extra rows rendered above/below the on-screen band, same purpose as
/// `list.rs`'s `OVERSCAN_ROWS` but in units of tile-rows (each covers
/// `GRID_COLUMNS` entries, so a smaller row-overscan already covers plenty
/// of entries).
const OVERSCAN_ROWS: usize = 2;

/// Tile-rows rendered before the first `Scrolled` event arrives.
const INITIAL_ROWS: usize = 6;

pub(super) fn view<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
) -> Element<'a, Message> {
    if let Some(err) = &state.error {
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

    let total = state.visible.len();
    let tile_row_height = GRID_TILE_SIZE + GRID_LABEL_HEIGHT + GRID_GAP;
    let total_rows = total.div_ceil(GRID_COLUMNS);
    let (first_row, last_row) = visible_row_range(state, tile_row_height, total_rows);

    let before = Space::new().height(tile_row_height * first_row as f32);
    let after = Space::new().height(tile_row_height * total_rows.saturating_sub(last_row) as f32);

    let rows = (first_row..last_row).map(|row_index| {
        let start = row_index * GRID_COLUMNS;
        let end = start.saturating_add(GRID_COLUMNS).min(total);
        let tiles = state
            .visible
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .filter_map(|(offset, &entry_index)| {
                state
                    .entries
                    .get(entry_index)
                    .map(|entry| tile(state, t, mime_db, thumb_cache, start + offset, entry))
            });
        row(tiles).spacing(GRID_GAP).into()
    });

    let body_column = column(
        std::iter::once(before.into())
            .chain(rows)
            .chain(std::iter::once(after.into())),
    )
    .spacing(GRID_GAP)
    .padding(GRID_GAP)
    .width(Fill);

    scrollable(body_column)
        .on_scroll(Message::Scrolled)
        .style(style::scrollable::rest(t, Surface::Paper))
        .width(Fill)
        .height(Fill)
        .into()
}

/// Which band of tile-rows is on (or near) screen — the grid-row analogue
/// of `list.rs`'s `visible_range`, working in row units instead of
/// individual item indices.
fn visible_row_range(state: &DirectoryView, row_height: f32, total_rows: usize) -> (usize, usize) {
    if row_height <= 0.0 {
        return (0, total_rows.min(INITIAL_ROWS));
    }
    let Some(viewport) = state.scroll else {
        return (0, total_rows.min(INITIAL_ROWS));
    };
    let offset = viewport.absolute_offset().y.max(0.0);
    let visible_rows = (viewport.bounds().height / row_height).ceil().max(0.0) as usize;
    let first = ((offset / row_height).floor() as usize).saturating_sub(OVERSCAN_ROWS);
    let last = (first
        .saturating_add(visible_rows)
        .saturating_add(OVERSCAN_ROWS * 2))
    .min(total_rows);
    (first.min(last), last)
}

fn tile<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
    visible_index: usize,
    entry: &'a FileEntry,
) -> Element<'a, Message> {
    // Stage 8: same inline-rename swap `list.rs::entry_row` does — see
    // that function's doc comment for why this checks by name.
    if let Some(rename) = state.rename_state()
        && rename.original == entry.name
    {
        return renaming_tile(t, mime_db, entry, rename);
    }

    let selected = state.selection.is_selected(&entry.name);
    let has_cursor = state.selection.cursor() == Some(visible_index);

    // Glyph shape carries type, never hue (style guide §1) — see
    // `list.rs`'s identical note on why the tint only follows selected/
    // not-selected, never hover, at this call site. Stage 11: a cached
    // thumbnail (regular files only) replaces the glyph square — see
    // `list.rs::entry_row`'s identical swap and `thumbnail_for`'s own doc
    // comment for exactly what qualifies.
    let icon_color = if selected {
        t.palette.paper
    } else {
        t.on_paper.primary
    };
    let glyph: Element<'a, Message> = match thumbnail_for(state, thumb_cache, entry) {
        Some(handle) => iced::widget::image(handle.handle())
            .width(GRID_TILE_SIZE)
            .height(GRID_TILE_SIZE)
            .into(),
        None => icons::icon(
            row_icon(entry, mime_db),
            t.sizes.icon_bare,
            icon_color.into_iced(),
        )
        .into(),
    };

    let name = text(entry.display_name().into_owned())
        .size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t))
        .align_x(iced::alignment::Horizontal::Center);

    // `align_x(Center)` on the column, not just on the `name` text: a
    // `text` widget's own `align_x` only positions the glyphs inside the
    // text's own box, and that box is `Shrink`-wide — so without this the
    // label hugs the tile's left edge instead of sitting under the icon.
    let content = column![
        container(glyph)
            .width(Fill)
            .height(Length::Fixed(GRID_TILE_SIZE))
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(Center),
        name,
    ]
    .width(Length::Fixed(GRID_TILE_SIZE))
    .align_x(Center);

    // The wrapping `container` centres glyph+label vertically in the tile.
    // A `button` places its content at the padding's top-left corner and
    // never aligns it, so the tile's leftover height would otherwise all
    // collect below the label; a `container` hands its child loose limits
    // and *does* align the result.
    let styled = button(container(content).height(Fill).align_y(Center))
        .style(tile_style(t, selected, has_cursor))
        .width(Length::Fixed(GRID_TILE_SIZE))
        .height(Length::Fixed(GRID_TILE_SIZE + GRID_LABEL_HEIGHT))
        .padding(t.sizes.pill_gap / 2.0)
        .on_press(Message::RowClicked(visible_index));

    mouse_area(styled)
        .on_double_click(Message::RowDoubleClicked(visible_index))
        .into()
}

/// The inline-rename presentation of one tile: same glyph, a `text_input`
/// in place of the name label below it. No error-message slot the way
/// `list.rs::renaming_row`'s date column has (a tile has no second text
/// line to spare) — a rejected rename's message still lands in the field
/// via `RenameState::error`, it just isn't rendered here; the field stays
/// open either way, so nothing is silently lost, only the specific reason
/// isn't shown in grid view. Worth a follow-up if this proves confusing in
/// practice; not blocking for this stage.
fn renaming_tile<'a>(
    t: &'a Theme,
    mime_db: &'a MimeDb,
    entry: &'a FileEntry,
    rename: &'a super::rename::RenameState,
) -> Element<'a, Message> {
    let glyph = icons::icon(
        row_icon(entry, mime_db),
        t.sizes.icon_bare,
        t.on_paper.primary.into_iced(),
    );

    let field = text_input("Name", &rename.buffer)
        .id(RENAME_INPUT_ID)
        .on_input(Message::RenameChanged)
        .on_submit(Message::RenameSubmitted)
        .style(style::text_input::rest(t, Surface::Paper))
        .font(convert::ui_font(t))
        .size(t.typography.size.secondary);

    let content = column![
        container(glyph)
            .width(Fill)
            .height(Length::Fixed(GRID_TILE_SIZE))
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(Center),
        field,
    ]
    .width(Length::Fixed(GRID_TILE_SIZE))
    .align_x(Center);

    // Same centring an ordinary `tile` gets, so a tile mid-rename doesn't
    // shift its glyph relative to its neighbours.
    container(content)
        .width(Length::Fixed(GRID_TILE_SIZE))
        .height(Length::Fixed(GRID_TILE_SIZE + GRID_LABEL_HEIGHT))
        .padding(t.sizes.pill_gap / 2.0)
        .align_y(Center)
        .into()
}

/// Selected/cursor tile treatment. Same "no upstream style for this yet"
/// posture as `list.rs`'s `row_style` — a tile is a bigger, squarer
/// analogue of a list row, so the recipe (transparent rest, `fill_subtle`
/// hover, terracotta selected, `focus_border` ring for the keyboard
/// cursor) is deliberately identical, just at `radii.tile` instead of
/// `radii.pill`.
///
/// TODO(saola-theme): promote to a `style::button`/`style::container`
/// "selected tile" helper (paired with `sizes.grid_tile`) and delete this
/// once saola-theme ships it; bump the pinned tag in the same PR.
fn tile_style(
    t: &Theme,
    selected: bool,
    has_cursor: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    let radius = t.radii.tile;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn div_ceil_matches_expected_row_counts() {
        // Sanity check on the constant this module's whole virtualization
        // scheme rests on — a change to `GRID_COLUMNS` should still round
        // row counts up, not down (a trailing partial row must still get
        // its own spacer-bracketed row).
        assert_eq!(7usize.div_ceil(GRID_COLUMNS), 2);
        assert_eq!(GRID_COLUMNS.div_ceil(GRID_COLUMNS), 1);
        assert_eq!(0usize.div_ceil(GRID_COLUMNS), 0);
    }
}
