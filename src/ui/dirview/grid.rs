//! Grid view: fixed-size glyph tiles, virtualized by *row of tiles* the
//! same way `list.rs` virtualizes by row of text — only the row band
//! actually on (or near) screen is ever built into `Element`s. CLAUDE.md's
//! "no virtualized list widget, never build 100k Elements" rule applies to
//! every directory view, not just the list.
//!
//! Column count is **measured**, not fixed: [`column_count`] works out how
//! many `sizes.grid_tile`-wide tiles fit in a real width, and `view` calls
//! it from inside an `iced::widget::responsive` closure — the same
//! mechanism `list.rs` uses to size its flexible name column, adopted here
//! now that it's proven. The virtualization below doesn't care where "how
//! many tiles per row" comes from, so it reads that one number and nothing
//! else changed.
//!
//! One number per frame is the rule that matters: the row builder, the
//! `div_ceil` total-row count and both spacers all take the *same*
//! `columns` value, or the spacer heights would describe a grid with a
//! different shape than the one being drawn and the view would drift out of
//! sync with its own scroll offset.
//!
//! `update()` needs the count too (Up/Down move the cursor by a whole tile
//! row) and can't see the closure's `Size`, so it goes through
//! [`columns_for_scroll`] instead — same arithmetic, width taken from the
//! last reported scroll viewport.

use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Center, Element, Fill, Length};
use saola_theme::icon;
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::entry::FileEntry;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;

use super::rename::RENAME_INPUT_ID;
use super::{DirectoryView, Message, row_icon, thumbnail_for};

/// The width `columns_for_scroll` assumes before any scroll viewport has
/// been reported — the grid's analogue of `INITIAL_ROWS` below, and used
/// only by the update-side path (`view` always has a real measurement).
///
/// 866 px is roughly what this view's content area works out to at the
/// app's default 1100×720 window, once the window border, the island gaps
/// and the 200 px sidebar are taken out; it is the same measured number
/// `list.rs`'s own budget test pins against, and is deliberately expressed
/// as a *width* rather than as a column count so the fallback still tracks
/// a `grid_tile`/`grid_tile_gap` token change instead of freezing whatever
/// count today's tokens happen to yield.
const INITIAL_GRID_WIDTH: f32 = 866.0;

/// Extra rows rendered above/below the on-screen band, same purpose as
/// `list.rs`'s `OVERSCAN_ROWS` but in units of tile-rows (each covers a
/// full row of tiles, so a smaller row-overscan already covers plenty of
/// entries).
const OVERSCAN_ROWS: usize = 2;

/// Tile-rows rendered before the first `Scrolled` event arrives.
const INITIAL_ROWS: usize = 6;

/// How many `tile`-wide tiles fit across `width` — the number that used to
/// be a hardcoded 6.
///
/// Every term is read straight off `view`'s layout below:
///
/// - the tile band's outer `column` carries `.padding(grid_tile_gap)`, a
///   uniform inset, so one `gap` is spent on each side: `width - 2 × gap`
///   is what a row of tiles actually gets.
/// - each row carries `.spacing(grid_tile_gap)`, which falls in the `n - 1`
///   seams *between* n tiles, not around them.
/// - each tile is exactly `Length::Fixed(grid_tile)` wide.
///
/// So n tiles fit when `n × tile + (n − 1) × gap ≤ width − 2 × gap`, which
/// rearranges to `n ≤ (width − gap) / (tile + gap)` — the floor of that
/// fraction, which is what this returns.
///
/// Nothing is subtracted for the scrollbar, deliberately, for the reason
/// `list.rs::name_unit_budget` spells out in full: iced's default
/// `Scrollable` reserves no layout width for its bar (this file never sets
/// `.spacing()` on the scrollable, which is what would turn the overlay
/// into a reserved gutter), so subtracting a guessed bar width would be
/// paying for space that was never taken.
///
/// No-panic saturation, in the order it applies (CLAUDE.md's no-panic rule
/// reaching into layout):
///
/// - a non-positive `tile + gap` (degenerate tokens) would divide by zero,
///   so it short-circuits to one column before the division happens —
///   otherwise `inf as usize` would saturate to `usize::MAX` and the row
///   builder would try to slice a wildly out-of-range range (it would
///   survive that, `get(..)` returns `None`, but the row count arithmetic
///   would be nonsense).
/// - a negative or NaN quotient goes through `as usize`, which in Rust
///   2021+ saturates to 0 rather than being UB, and then `.max(1)` lifts it
///   to a single column — a window squeezed narrower than one tile still
///   renders one tile per row (clipped by the viewport, which is the honest
///   outcome) rather than dividing by zero further down `div_ceil`.
///
/// Pure function of three `f32`s, testable below without a `Theme` or a
/// renderer — the same posture `list.rs::name_unit_budget` takes.
pub(super) fn column_count(width: f32, tile: f32, gap: f32) -> usize {
    let step = tile + gap;
    if step <= 0.0 {
        return 1;
    }
    (((width - gap) / step).floor() as usize).max(1)
}

/// Tiles per row for the update-side cursor math
/// (`DirectoryView::row_step`, which steps Up/Down by a whole visual row in
/// grid mode).
///
/// `update()` never sees the `responsive` closure's `Size`, so the width
/// comes from the last scroll viewport instead: iced's `Scrollable`
/// republishes `on_scroll` on `RedrawRequested` whenever its bounds change,
/// so this is stale for at most one frame after a resize — and a one-frame
/// stale column count costs, at worst, one arrow key landing a row off
/// before the next redraw corrects it. `None` (nothing has been laid out
/// yet) falls back to [`INITIAL_GRID_WIDTH`].
///
/// The tokens come from `saola_theme::tokens::Sizes::default()` rather than
/// from a `Theme` because `update()` has no `Theme` to read (the same
/// constraint `mod.rs`'s `PAGE_ROWS`/`THUMB_ROW_HEIGHT_GUESS` document) —
/// but unlike those two this is not a guess: `App` builds exactly one
/// theme, `Theme::saola()`, whose `sizes` field *is* `Sizes::default()`, so
/// these are the same numbers `view` reads from `&Theme` a few lines below.
/// If saola-files ever grows runtime theming, this is the call site that
/// has to start taking a `&Theme` instead.
pub(super) fn columns_for_scroll(scroll: Option<scrollable::Viewport>) -> usize {
    let sizes = saola_theme::tokens::Sizes::default();
    let width = scroll.map_or(INITIAL_GRID_WIDTH, |viewport| viewport.bounds().width);
    column_count(width, sizes.grid_tile, sizes.grid_tile_gap)
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

    // `responsive` is how a widget learns its own width in iced 0.14: the
    // closure runs *inside* `Widget::layout`, after limits are known, and
    // is handed the `Size` this container actually got — the only place in
    // a `view()` where a real pixel width exists, and so the only place
    // `column_count` can be asked an honest question. Same mechanism and
    // same two rules as `list.rs::view` (see its comment in full): the
    // closure is `Fn`, re-run on every relayout, and everything it captures
    // is a shared reference, which is `Copy`; and it returns the same tree
    // shape every run (one `scrollable`, always), so the scrollable's
    // widget state — including its offset — is carried across a resize
    // instead of being torn down and reset to the top.
    iced::widget::responsive(move |size| {
        // One column count per frame, read once here and handed to
        // everything below it. The row builder, `div_ceil` and both spacers
        // must agree or the spacer heights describe a differently-shaped
        // grid than the one being drawn — see the module docs.
        let columns = column_count(size.width, t.sizes.grid_tile, t.sizes.grid_tile_gap);

        let total = state.visible.len();
        let tile_row_height = t.sizes.grid_tile + t.sizes.grid_tile_label + t.sizes.grid_tile_gap;
        let total_rows = total.div_ceil(columns);
        let (first_row, last_row) = visible_row_range(state, tile_row_height, total_rows);

        let before = Space::new().height(tile_row_height * first_row as f32);
        let after =
            Space::new().height(tile_row_height * total_rows.saturating_sub(last_row) as f32);

        let rows = (first_row..last_row).map(move |row_index| {
            let start = row_index.saturating_mul(columns);
            let end = start.saturating_add(columns).min(total);
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
    })
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
    // (§7's default overflow mode: cut at a width limit, exactly one `…`,
    // never three dots, no motion) — it makes the label honest about being
    // elided, and since saola-theme 0.10 it counts that limit in UAX #11
    // width units, so a CJK or emoji name spends its budget twice as fast
    // as a Latin one and lands at about the same pixel width. What the
    // budget still can't see is the spread *within* narrow characters
    // (`mmmm` vs `llll` — see `LABEL_AVG_ADVANCE_EM`), so an unusually
    // wide-for-its-count Latin name can still paint a little over the
    // tile — see `saola_theme::overflow`'s module docs for the full
    // calibration essay (the 0.55 em average advance, what it does and
    // doesn't cover); it lives upstream since 0.11.0 because the list view
    // needs the same arithmetic and a shared constant deserves one home.
    // `Wrapping::None` is the hard guarantee for that residue, and
    // it is the one that actually matters here: a few pixels of horizontal
    // lean into the tile gap is cosmetic, whereas a second line would push
    // the tile past `grid_tile + grid_tile_label` and break the fixed row
    // height every spacer offset in `view()` above is computed from —
    // labels would then drift out of sync with the scroll position, which
    // is the bug this whole change exists to kill. Rendering is what
    // shortens the name; nothing here touches `entry.name`, so rename,
    // selection and the sort still see the real one.
    let max_units =
        saola_theme::overflow::unit_budget(t.sizes.grid_tile, t.typography.size.secondary);
    let name = text(saola_theme::overflow::truncate(
        &entry.display_name(),
        max_units,
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

    /// Today's tokens, spelled out once: `sizes.grid_tile` 96,
    /// `sizes.grid_tile_gap` 12.
    fn columns(width: f32) -> usize {
        column_count(width, 96.0, 12.0)
    }

    #[test]
    fn div_ceil_matches_expected_row_counts() {
        // Sanity check on the arithmetic this module's whole virtualization
        // scheme rests on — whatever the measured column count turns out to
        // be, row counts must round *up*, not down (a trailing partial row
        // must still get its own spacer-bracketed row). Parametrized by the
        // measured count now that there's no constant to test against.
        let n = columns(INITIAL_GRID_WIDTH);
        assert_eq!((n + 1).div_ceil(n), 2);
        assert_eq!(n.div_ceil(n), 1);
        assert_eq!(0usize.div_ceil(n), 0);
        // And the same three shapes at a much narrower width, where `n` is
        // a completely different number.
        let narrow = columns(300.0);
        assert!(narrow < n);
        assert_eq!((narrow + 1).div_ceil(narrow), 2);
        assert_eq!(narrow.div_ceil(narrow), 1);
    }

    #[test]
    fn column_count_at_todays_tokens() {
        // `n ≤ (width − gap) / (tile + gap)`, floored — see
        // `column_count`'s doc comment for where each term comes from.
        //
        // 866 px is the content width at the app's default 1100×720 window
        // (the same measured number `list.rs`'s budget test pins against,
        // and this module's `INITIAL_GRID_WIDTH`):
        // (866 − 12) / 108 = 7.90 → 7 tiles, spending
        // 7×96 + 6×12 + 2×12 = 768 of the 866, with 98 px to spare — not
        // quite an eighth tile, which is exactly what the floor says.
        assert_eq!(columns(866.0), 7);
        assert_eq!(columns(INITIAL_GRID_WIDTH), 7);
        // A niri column halved: (435 − 12) / 108 = 3.9 → 3.
        assert_eq!(columns(435.0), 3);
        // Fullscreen on a 2560-logical-px output, roughly:
        // (2326 − 12) / 108 = 21.4 → 21.
        assert_eq!(columns(2326.0), 21);
        // The exact boundary: 7 tiles need 7×96 + 6×12 + 2×12 = 768 px, so
        // 768 fits 7 and one pixel less fits only 6. An off-by-one in the
        // padding/spacing accounting would show up right here.
        assert_eq!(columns(768.0), 7);
        assert_eq!(columns(767.0), 6);
    }

    #[test]
    fn column_count_never_falls_below_one() {
        // A window squeezed under a single tile still draws one tile per
        // row (clipped by the viewport — the honest outcome) rather than
        // returning 0, which would make `div_ceil` divide by zero and take
        // the app down. CLAUDE.md's no-panic rule reaching into layout.
        assert_eq!(columns(100.0), 1);
        assert_eq!(columns(0.0), 1);
        assert_eq!(columns(-500.0), 1);
        assert_eq!(columns(f32::NAN), 1);
        // Degenerate tokens: a zero (or negative) tile+gap step would
        // divide by zero, so it short-circuits before the division.
        assert_eq!(column_count(866.0, 0.0, 0.0), 1);
        assert_eq!(column_count(866.0, -96.0, -12.0), 1);
    }

    #[test]
    fn column_count_grows_with_the_window() {
        // Wider has to mean more tiles per row, or the whole change does
        // nothing on resize — and never fewer, which would mean the
        // arithmetic isn't monotonic in width.
        assert!(columns(435.0) < columns(866.0));
        assert!(columns(866.0) < columns(1732.0));
        assert!(columns(1732.0) < columns(2326.0));
        // Monotonic across a sweep, not just at the sampled pairs.
        let mut previous = 0usize;
        for step in 0..200 {
            let n = columns(step as f32 * 25.0);
            assert!(n >= previous, "column count shrank at width {}", step * 25);
            previous = n;
        }
    }

    #[test]
    fn columns_for_scroll_falls_back_to_the_initial_window() {
        // The update-side path, before any viewport has been reported: it
        // must agree with what `view` would compute at the same width, and
        // must never be the old hardcoded placeholder by coincidence of
        // reading a stale constant.
        assert_eq!(
            columns_for_scroll(None),
            columns(INITIAL_GRID_WIDTH),
            "the pre-layout fallback has to be the measured count at the \
             default window, not a number of its own"
        );
        assert!(columns_for_scroll(None) >= 1);
    }

    #[test]
    fn label_budget_at_todays_tokens() {
        // `sizes.grid_tile` 96, `typography.size.secondary` 12.5 — the
        // shipped values. 96 / (12.5 * 0.55) = 13.96, floored to 13.
        //
        // Since saola-theme 0.11.0 the arithmetic lives upstream, so this
        // now guards two things at once: a *token* bump that silently
        // halves the budget, and a *calibration* drift upstream (a changed
        // average advance or floor) that would quietly reshape every label
        // in this view. Either shows up here rather than on screen.
        assert_eq!(saola_theme::overflow::unit_budget(96.0, 12.5), 13);
    }

    #[test]
    fn label_budget_degenerate_tokens_stay_at_the_floor() {
        // One spot-check that a degenerate token pair (a tiny tile against
        // an enormous font) still can't produce a budget so small that
        // `truncate` spends the whole thing on the ellipsis — and can't
        // panic. The exhaustive cases (zeroes, negatives, both signs) are
        // upstream's tests now; this only pins that *this* view still sees
        // that behaviour.
        assert_eq!(saola_theme::overflow::unit_budget(10.0, 100.0), 4);
    }

    #[test]
    fn label_budget_tracks_tile_size() {
        // One spot-check that a bigger tile earns more units — the point of
        // deriving this from tokens instead of hardcoding it. Full
        // monotonicity is upstream's test.
        assert!(
            saola_theme::overflow::unit_budget(192.0, 12.5)
                > saola_theme::overflow::unit_budget(96.0, 12.5)
        );
    }

    #[test]
    fn todays_budget_keeps_every_script_inside_one_tile() {
        // What the budget buys once `truncate` charges by width unit: 13
        // units is 12 of prefix plus the `…`, so a Latin name keeps twelve
        // characters while a full-width one keeps only six — both painting
        // roughly the same number of pixels, which is the ~145 px CJK spill
        // into the neighbouring tile shut for good. These are the exact
        // names in the on-screen verification tree, so the assertions below
        // and the screenshots are claiming the same thing.
        //
        // The width table itself is saola-theme's business (and tested
        // there); what this pins is the pairing of *our* tokens with it,
        // which is the part a token bump — or an upstream calibration
        // change to `unit_budget` — could break without anyone noticing.
        let budget = saola_theme::overflow::unit_budget(96.0, 12.5);
        let cut = |s: &str| saola_theme::overflow::truncate(s, budget);

        assert_eq!(cut("a-very-long-ascii-filename.txt"), "a-very-long-…");
        assert_eq!(
            cut("設定ウィンドウのタイトルはとても長いファイル名です.txt"),
            "設定ウィンド…"
        );
        assert_eq!(cut("🦌🦌🦌-saola-deer-emoji-filename.txt"), "🦌🦌🦌-saola…");
        // Mixed scripts split the same budget between them, rather than one
        // script's average deciding the whole label.
        assert_eq!(cut("プロジェクト-final-draft-v2.txt"), "プロジェクト…");

        // A name that already fits comes back untouched — no stray ellipsis
        // on the common case (13 narrow characters is exactly 13 units).
        assert_eq!(cut("exactly13.txt"), "exactly13.txt");
    }
}
