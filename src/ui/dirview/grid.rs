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

use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Center, Element, Fill, Length};
use saola_theme::icon;
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::entry::FileEntry;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;

use super::rename::RENAME_INPUT_ID;
use super::{DirectoryView, Message, row_icon, thumbnail_for};

/// Tiles per row. See the module docs — this is a placeholder, not a
/// layout measurement. `pub(super)` so `DirectoryView::row_step` (the
/// Up/Down/PageUp/PageDown cursor math) can step by a full visual row in
/// grid mode instead of by one item at a time.
pub(super) const GRID_COLUMNS: usize = 6;

/// Extra rows rendered above/below the on-screen band, same purpose as
/// `list.rs`'s `OVERSCAN_ROWS` but in units of tile-rows (each covers
/// `GRID_COLUMNS` entries, so a smaller row-overscan already covers plenty
/// of entries).
const OVERSCAN_ROWS: usize = 2;

/// Tile-rows rendered before the first `Scrolled` event arrives.
const INITIAL_ROWS: usize = 6;

/// Average glyph advance of the UI sans, as a fraction of the font size.
/// An approximation, deliberately: the face is proportional, so an `m` and
/// an `l` are nowhere near this same width — but the style guide (§7) asks
/// for a limit "measured in characters, not pixels … approximate under a
/// proportional face", and iced 0.14's `view()` cannot measure a string
/// before layout runs anyway. 0.55 em is the usual ballpark for a humanist
/// sans at mixed-case text; see [`label_char_budget`] for what it feeds.
///
/// Verified on screen 2026-08-10 against the shipped tokens: a 13-character
/// Latin label renders ~77 px wide in a 96 px tile — close to the edge with
/// margin left for wider-than-average strings, which is the calibration
/// this constant is for.
///
/// **Known limitation, disclosed rather than silently accepted.** One
/// average cannot describe every script. CJK is full-width — nearer 1.0 em
/// than 0.55 — so a 13-character CJK name renders roughly 145 px and spills
/// past its tile into the neighbouring label. That is the style guide's own
/// documented trade (§7, and `saola_theme::overflow`'s module docs: "a
/// 50-character cap is not a 50-column cap … the guide blesses the
/// approximation rather than making every consumer measure glyphs"), so it
/// is not fixed by re-tuning this number: raising it to 1.0 for CJK's sake
/// would cut Latin labels to 7 characters and underfill every ordinary
/// filename. Fixing it properly means charging wide characters more than
/// one unit of budget (an East-Asian-width table) or measuring with the
/// renderer — either is a design-language decision for saola-theme to make
/// once, for every surface, not a local patch here.
const LABEL_AVG_ADVANCE_EM: f32 = 0.55;

/// How many characters of filename fit across one tile, derived from tokens
/// rather than hardcoded so a token change carries the label with it.
///
/// The `.max(4)` is a floor, not a tuning knob: a degenerate token pair (a
/// tiny `grid_tile`, a huge font) would otherwise compute a budget of 0 or
/// 1, and `overflow::truncate` spends the last character of its budget on
/// the `…` itself — so every label in the view would collapse to a lone
/// ellipsis, which reads as a bug rather than as elision. Four leaves at
/// least three real characters visible.
///
/// Pure function of two numbers, which is what makes it testable below
/// without a `Theme` or a renderer.
fn label_char_budget(tile: f32, font: f32) -> usize {
    let advance = font * LABEL_AVG_ADVANCE_EM;
    if advance <= 0.0 {
        return 4;
    }
    // `as usize` on a f32 saturates at 0 for negatives and at usize::MAX for
    // huge values in Rust 2021+ — no UB, no panic, so a nonsense `tile`
    // can't take the app down (CLAUDE.md's no-panic rule).
    ((tile / advance).floor() as usize).max(4)
}

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
    let tile_row_height = t.sizes.grid_tile + t.sizes.grid_tile_label + t.sizes.grid_tile_gap;
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
        row(tiles).spacing(t.sizes.grid_tile_gap).into()
    });

    let body_column = column(
        std::iter::once(before.into())
            .chain(rows)
            .chain(std::iter::once(after.into())),
    )
    .spacing(t.sizes.grid_tile_gap)
    .padding(t.sizes.grid_tile_gap)
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
            .width(t.sizes.grid_tile)
            .height(t.sizes.grid_tile)
            .into(),
        None => icon::icon(
            row_icon(entry, mime_db),
            t.sizes.icon_bare,
            icon_color.into_iced(),
        )
        .into(),
    };

    // Two belts for one job, because each covers what the other can't.
    // `truncate` is the *style guide's* answer to a name that doesn't fit
    // (§7's default overflow mode: cut at a character limit, exactly one
    // `…`, never three dots, no motion) — it makes the label honest about
    // being elided. But a character count is only an approximation of
    // pixel width under a proportional face, so a wide-glyph name can
    // still come out over the tile (see `LABEL_AVG_ADVANCE_EM`'s disclosed
    // limitation). `Wrapping::None` is the hard guarantee for that case,
    // and it is the one that actually matters here: a horizontal spill is
    // cosmetic, whereas a second line would push the tile past `grid_tile
    // + grid_tile_label` and break the fixed row height every spacer
    // offset in `view()` above is computed from — labels would then drift
    // out of sync with the scroll position, which is the bug this whole
    // change exists to kill. Rendering is what shortens the name; nothing
    // here touches `entry.name`, so rename, selection and the sort still
    // see the real one.
    let max_chars = label_char_budget(t.sizes.grid_tile, t.typography.size.secondary);
    let name = text(saola_theme::overflow::truncate(
        &entry.display_name(),
        max_chars,
    ))
    .size(t.typography.size.secondary)
    .font(convert::ui_font_regular(t))
    .wrapping(iced::widget::text::Wrapping::None)
    .align_x(iced::alignment::Horizontal::Center);

    // `align_x(Center)` on the column, not just on the `name` text: a
    // `text` widget's own `align_x` only positions the glyphs inside the
    // text's own box, and that box is `Shrink`-wide — so without this the
    // label hugs the tile's left edge instead of sitting under the icon.
    let content = column![
        container(glyph)
            .width(Fill)
            .height(Length::Fixed(t.sizes.grid_tile))
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(Center),
        name,
    ]
    .width(Length::Fixed(t.sizes.grid_tile))
    .align_x(Center);

    // The wrapping `container` centres glyph+label vertically in the tile.
    // A `button` places its content at the padding's top-left corner and
    // never aligns it, so the tile's leftover height would otherwise all
    // collect below the label; a `container` hands its child loose limits
    // and *does* align the result.
    // No `mouse_area(...).on_double_click(...)` wrapper — same reasoning
    // as `list.rs::entry_row`: the button's `on_press` captures every left
    // press before an outer `MouseArea` could track it, so doubles are
    // paired app-side in `Message::RowClicked`'s handler instead.
    button(container(content).height(Fill).align_y(Center))
        .style(style::button::selection_tile(
            t,
            Surface::Paper,
            selected,
            has_cursor,
        ))
        .width(Length::Fixed(t.sizes.grid_tile))
        .height(Length::Fixed(t.sizes.grid_tile + t.sizes.grid_tile_label))
        .padding(t.sizes.pill_gap / 2.0)
        .on_press(Message::RowClicked(visible_index))
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
    let glyph = icon::icon(
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
            .height(Length::Fixed(t.sizes.grid_tile))
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(Center),
        field,
    ]
    .width(Length::Fixed(t.sizes.grid_tile))
    .align_x(Center);

    // Same centring an ordinary `tile` gets, so a tile mid-rename doesn't
    // shift its glyph relative to its neighbours.
    container(content)
        .width(Length::Fixed(t.sizes.grid_tile))
        .height(Length::Fixed(t.sizes.grid_tile + t.sizes.grid_tile_label))
        .padding(t.sizes.pill_gap / 2.0)
        .align_y(Center)
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

    #[test]
    fn label_budget_at_todays_tokens() {
        // `sizes.grid_tile` 96, `typography.size.secondary` 12.5 — the
        // shipped values at saola-theme 0.9.0. 96 / (12.5 * 0.55) = 13.96,
        // floored to 13. Pinned so a token bump that silently halves the
        // budget shows up here rather than on screen.
        assert_eq!(label_char_budget(96.0, 12.5), 13);
    }

    #[test]
    fn label_budget_never_falls_below_the_floor() {
        // Degenerate token pairs: a tiny tile, an enormous font, and the
        // nonsense cases. None may compute a budget small enough that
        // `truncate` spends the whole thing on the ellipsis, and none may
        // panic (CLAUDE.md's no-panic rule) — `as usize` saturates rather
        // than wrapping, and a non-positive advance short-circuits.
        assert_eq!(label_char_budget(10.0, 100.0), 4);
        assert_eq!(label_char_budget(0.0, 12.5), 4);
        assert_eq!(label_char_budget(96.0, 0.0), 4);
        assert_eq!(label_char_budget(-96.0, 12.5), 4);
        assert_eq!(label_char_budget(96.0, -12.5), 4);
    }

    #[test]
    fn label_budget_tracks_tile_size() {
        // A bigger tile earns more characters, a smaller one fewer — the
        // point of deriving this from tokens instead of hardcoding it.
        assert!(label_char_budget(192.0, 12.5) > label_char_budget(96.0, 12.5));
        assert!(label_char_budget(64.0, 12.5) < label_char_budget(96.0, 12.5));
    }
}
