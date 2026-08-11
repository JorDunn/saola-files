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

use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
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

/// How many *width units* of filename fit in the name column at a given
/// list width — the list's analogue of the grid's fixed-tile budget, except
/// that here the column is `Fill`, so the number has to come from a real
/// measurement rather than a token.
///
/// Every subtraction below is one thing the row actually spends its width
/// on, read straight off `entry_row`'s layout:
///
/// - `2.0 * pill_gap` — the row's own `padding([0, pill_gap])`, one gap on
///   each side.
/// - `SIZE_COLUMN` and `DATE_COLUMN` — the two fixed columns, which take
///   their width before the `Fill` name column sees any.
/// - `icon_row` — the glyph (or thumbnail) box at the head of the name cell.
/// - one more `pill_gap` — the name row's `.spacing(pill_gap)` between that
///   icon and the label itself.
///
/// Nothing is subtracted for the scrollbar, deliberately: iced's default
/// `Scrollable` reserves no layout width for its bar (this file never sets
/// `.spacing()` on the scrollable, which is what would turn the overlay into
/// a reserved gutter), so the bar floats over the tail of the date column
/// and never over the name. Subtracting a guessed scrollbar width here would
/// be a magic number paying for space that was never taken.
///
/// Squeezing the window narrower than the two fixed columns makes
/// `available` negative. That is fine and upstream-documented: `unit_budget`
/// saturates a negative through `as usize` to 0 and then clamps to its floor
/// of 4, so the worst case is every name eliding down to three narrow
/// characters plus the `…` — never a panic, never a budget of 0 that would
/// render every label as a lone ellipsis.
///
/// Pure function of four `f32`s, which is what makes it testable below
/// without a `Theme` or a renderer — the same posture `grid.rs` takes.
fn name_unit_budget(list_width: f32, font: f32, pill_gap: f32, icon_row: f32) -> usize {
    let available = list_width - 2.0 * pill_gap - SIZE_COLUMN - DATE_COLUMN - icon_row - pill_gap;
    saola_theme::overflow::unit_budget(available, font)
}

/// How many *width units* of a failed rename's message fit where the date
/// normally sits (`renaming_row`'s trailing cell).
///
/// Unlike [`name_unit_budget`] this one needs no measurement and no
/// `responsive`: the cell is `container(..).width(Length::Fixed(
/// DATE_COLUMN))`, a fixed 168 px whatever the window does. Nothing is
/// subtracted from it — that container carries no padding of its own, and
/// the row's `padding([0, pill_gap])` sits *outside* all three columns, so
/// the full 168 px is the cell's to spend.
///
/// `font` is the caller's `typography.size.secondary`, which is what the
/// trailing text is actually set at (the same size the date it replaces
/// uses, so a row mid-rename doesn't change type size when a submit
/// fails). It's set in the *mono* face, whose advance runs a little wider
/// than the 0.55 em average `unit_budget` is calibrated on for the
/// proportional UI face; there is no mono-specific budget upstream yet, and
/// inventing a local fudge factor would be exactly the local restyling
/// CLAUDE.md forbids, so the residue is left to `Wrapping::None` at the
/// call site — see the note there.
///
/// Pure function of one `f32`, testable without a `Theme` or a renderer.
fn error_unit_budget(font: f32) -> usize {
    saola_theme::overflow::unit_budget(DATE_COLUMN, font)
}

pub(super) fn view<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    s: Surface,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
) -> Element<'a, Message> {
    if let Some(err) = &state.error {
        // `err.to_string()` is owned, so `empty_state` gets a `String` it
        // can consume into a `text` widget directly — a `&str` borrowing
        // this temporary can't outlive this function, but the returned
        // `Element<'a, _>` must.
        return empty_state_owned(t, s, err.to_string());
    }
    if state.entries.is_empty() {
        let message = if state.loading {
            "Loading…"
        } else {
            "This folder is empty"
        };
        return empty_state(t, s, message);
    }

    // `responsive` is how a widget learns its own width in iced 0.14: the
    // closure runs *inside* `Widget::layout`, after limits are known, and is
    // handed the `Size` this container actually got. That is the only place
    // in a `view()` where a real pixel width exists, and it is why the name
    // budget can be a measurement instead of a guess.
    //
    // Two things this closure must honour. It is `Fn`, not `FnOnce` — iced
    // re-runs it on every relayout — so everything it captures is captured
    // by value, which is free here: `&DirectoryView`, `&Theme`, `&MimeDb`
    // and `&ThumbCache` are all shared references, and shared references are
    // `Copy`; `Surface` is a `Copy` enum for the same reason. And it must
    // return the *same tree shape* every run
    // (`column![header, scrollable]`, always): `Responsive::diff` defers to
    // the ordinary `diff_children`, so a stable shape means the scrollable's
    // widget state — including the scroll offset — is matched up and carried
    // across a resize instead of being torn down and reset to the top.
    iced::widget::responsive(move |size| {
        let units = name_unit_budget(
            size.width,
            t.typography.size.body,
            t.sizes.pill_gap,
            t.sizes.icon_row,
        );

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
                state.entries.get(entry_index).map(|entry| {
                    entry_row(
                        state,
                        t,
                        s,
                        mime_db,
                        thumb_cache,
                        first + offset,
                        entry,
                        units,
                    )
                })
            });

        let body_column = column(
            std::iter::once(before.into())
                .chain(rows)
                .chain(std::iter::once(after.into())),
        )
        .width(Fill);

        let body = scrollable(body_column)
            .on_scroll(Message::Scrolled)
            .style(style::scrollable::rest(t, s))
            .width(Fill)
            .height(Fill);

        column![header_row(state, t, s), body]
            .width(Fill)
            .height(Fill)
            .into()
    })
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

fn header_row<'a>(state: &'a DirectoryView, t: &'a Theme, s: Surface) -> Element<'a, Message> {
    use iced::alignment::Horizontal;

    // Each header is aligned the way its own column's values are (see
    // `entry_row`): Name and Date read left-to-right, Size is right-aligned
    // so the digits line up under each other.
    let name = header_cell(t, s, "Name", SortKey::Name, state, Fill, Horizontal::Left);
    let size = header_cell(
        t,
        s,
        "Size",
        SortKey::Size,
        state,
        Length::Fixed(SIZE_COLUMN),
        Horizontal::Right,
    );
    let date = header_cell(
        t,
        s,
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
    s: Surface,
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
            icon::icon(glyph, t.sizes.icon_row, t.on(s).secondary.into_iced()),
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
        .style(style::button::bare(t, s))
        .on_press(Message::HeaderClicked(key))
        .width(width)
        .into()
}

// The `surface` knob pushes this from 7 params to 8, one past clippy's
// default threshold. `t` and `s` together are "how to draw", the two
// caches are "what to draw from", and the last three are this row's own
// coordinates — no two of them are the same kind of thing, so a bundling
// struct would be indirection for its own sake (CLAUDE.md: "prefer
// explicit code over clever abstraction"), the same call the sibling
// `ui::explorer::view` already makes for its own parameter list.
#[allow(clippy::too_many_arguments)]
fn entry_row<'a>(
    state: &'a DirectoryView,
    t: &'a Theme,
    s: Surface,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
    visible_index: usize,
    entry: &'a FileEntry,
    units: usize,
) -> Element<'a, Message> {
    // Stage 8: a row mid-inline-rename swaps its label for a `text_input`
    // and stops being a clickable button entirely (there is nothing sane
    // for a click on a field mid-edit to do) — checked by name, matching
    // `RenameState::original`'s own doc comment on why the target is fixed
    // by name rather than by (possibly now-stale) row position.
    if let Some(rename) = state.rename_state()
        && rename.original == entry.name
    {
        return renaming_row(t, s, mime_db, entry, rename);
    }

    let selected = state.selection.is_selected(&entry.name);
    let has_cursor = state.selection.cursor() == Some(visible_index);

    // Glyph shape carries type, never hue (style guide §1) — the icon's
    // tint only ever follows selected/not-selected (the same two-value
    // split `row_style` below draws the row background/text from); unlike
    // the row's own button chrome it can't also brighten on hover, since
    // an `svg::Style` closure is fixed at build time, not re-evaluated
    // per `button::Status`. The selected arm is `palette.paper` rather than
    // a role read on either ground: a selected row is filled terracotta, and
    // terracotta's foreground is ivory whatever the window is drawn on.
    let icon_color = if selected {
        t.palette.paper
    } else {
        t.on(s).primary
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

    // Two belts for one job, because each covers what the other can't (the
    // same pairing `grid.rs::tile` draws, for the same reasons).
    //
    // `truncate` is the style guide's honest answer to a name that doesn't
    // fit (§7: cut at a width limit, exactly one `…` — one glyph, never
    // three dots — and no motion), spending the budget in UAX #11 width
    // units so a CJK or emoji name lands at about the same pixel width as a
    // Latin one instead of running twice as long. What a unit-counted
    // budget still can't see is the spread *within* narrow characters
    // (`mmmm` vs `llll`), so a wide-for-its-count name can paint a little
    // past where the budget said it would.
    //
    // `Wrapping::None` is the hard guarantee for exactly that residue, and
    // here it is the belt that actually matters: a name long enough to wrap
    // made the row taller than `sizes.list_row`, and every spacer offset in
    // `view()` above is computed from that height being fixed — so a
    // wrapped row didn't just look wrong, it walked the whole virtualized
    // list out of sync with its own scroll position. That was a real layout
    // bug, not a cosmetic one. A few pixels of horizontal lean into the
    // size column's left margin is the cost, and it is the cheap one.
    //
    // Only the *rendering* is shortened: `entry.name` is untouched, so
    // selection, the rename target match, and the sort all still key off
    // the real `OsString`. Nothing downstream ever sees the elided string.
    let name = text(saola_theme::overflow::truncate(
        &entry.display_name(),
        units,
    ))
    .size(t.typography.size.body)
    .font(convert::ui_font(t))
    .wrapping(iced::widget::text::Wrapping::None);

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

    // No `mouse_area(...).on_double_click(...)` wrapper: this button's
    // `on_press` captures every left press over the row (it must — the
    // themed hover/press styling and press-swallowing both depend on it),
    // and iced's `MouseArea` forwards events to its child first, so an
    // outer double-click handler would never see a single press. Doubles
    // are paired app-side instead — see `Message::RowClicked`'s docs.
    button(content)
        .style(style::button::list_row(t, s, selected, has_cursor))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding(0)
        .on_press(Message::RowClicked(visible_index))
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
    s: Surface,
    mime_db: &'a MimeDb,
    entry: &'a FileEntry,
    rename: &'a super::rename::RenameState,
) -> Element<'a, Message> {
    let icon = icon::icon(
        row_icon(entry, mime_db),
        t.sizes.icon_row,
        t.on(s).primary.into_iced(),
    );

    let field = text_input("Name", &rename.buffer)
        .id(RENAME_INPUT_ID)
        .on_input(Message::RenameChanged)
        .on_submit(Message::RenameSubmitted)
        .style(style::text_input::rest(t, s))
        .font(convert::ui_font(t))
        .size(t.typography.size.body)
        .width(Fill);

    let name_row = row![icon, field]
        .spacing(t.sizes.pill_gap)
        .align_y(iced::Center);

    // A rename error is arbitrary-length prose about an arbitrary-length
    // path ("<location> already exists", "<location> is a folder" — see
    // `VfsError`'s `Display`), and it lands in a cell sized for a 16-
    // character timestamp. Untruncated it wrapped to two or three lines,
    // which pushed this row past `sizes.list_row` and put every spacer
    // offset in `view()` out by the difference — the same fixed-row-height
    // contract the name column's truncation protects, broken from the
    // other end of the row.
    //
    // So: the same two belts, for the same two reasons (see `entry_row`'s
    // long note). `truncate` is §7's honest elision — one `…`, no motion,
    // budget counted in width units; `Wrapping::None` is the hard
    // one-line guarantee covering what a unit budget can't see. The date
    // is left alone: `format_system_time` is a fixed 16 characters, well
    // inside the budget, so running it through `truncate` would only cost
    // an allocation.
    //
    // Severity stays carried by wording, never color (CLAUDE.md: three
    // colors, never a fourth) — an elided message is still the same
    // message, and the full one is one keystroke away since the field
    // stays open to retry.
    let trailing_text = match &rename.error {
        Some(error) => {
            saola_theme::overflow::truncate(error, error_unit_budget(t.typography.size.secondary))
        }
        None => date_text(entry),
    };
    let trailing = text(trailing_text)
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t))
        .wrapping(iced::widget::text::Wrapping::None);
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

fn empty_state<'a>(t: &'a Theme, s: Surface, message: &'a str) -> Element<'a, Message> {
    saola_theme::widget::empty_state(t, s, message)
}

fn empty_state_owned<'a>(t: &'a Theme, s: Surface, message: String) -> Element<'a, Message> {
    container(
        text(message)
            .size(t.typography.size.secondary)
            .font(convert::ui_font_regular(t))
            .color(t.on(s).tertiary.into_iced()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Today's tokens, spelled out once: `typography.size.body` 13.5,
    /// `sizes.pill_gap` 8, `sizes.icon_row` 16.
    fn budget(width: f32) -> usize {
        name_unit_budget(width, 13.5, 8.0, 16.0)
    }

    #[test]
    fn name_budget_at_todays_tokens() {
        // 866 px is roughly what the list content area works out to at the
        // default 1100×720 window, once the window border, the island gaps
        // and the 200 px sidebar are taken out. It is a *representative*
        // width for pinning arithmetic, not a contract — the whole point of
        // `responsive` is that the runtime budget comes from the measured
        // size, so no chrome token here can drift the feature out of true;
        // it can only drift this number.
        //
        // available = 866 − 2×8 − 84 − 168 − 16 − 8 = 574
        // 574 / (13.5 × 0.55) = 77.3, floored to 77 — the value pinned
        // upstream in saola-theme 0.11.0's own tests, so the two agree.
        assert_eq!(budget(866.0), 77);
    }

    #[test]
    fn name_budget_widens_with_the_window() {
        // Half a niri column against a full one, and one more widening
        // pair: more width has to buy more name, or the feature does
        // nothing on resize.
        assert!(budget(435.0) < budget(866.0));
        assert!(budget(866.0) < budget(1732.0));
    }

    #[test]
    fn name_budget_never_falls_below_the_floor() {
        // A window squeezed under the fixed columns drives `available`
        // negative (200 − 16 − 84 − 168 − 8 = −76, and it only gets worse
        // from there). Upstream saturates that to the floor of 4 rather
        // than panicking or computing a budget of 0 — CLAUDE.md's no-panic
        // rule reaching all the way down into layout.
        assert_eq!(budget(200.0), 4);
        assert_eq!(budget(0.0), 4);
        assert_eq!(budget(-50.0), 4);
    }

    #[test]
    fn rename_error_budget_at_todays_tokens() {
        // `DATE_COLUMN` 168, `typography.size.secondary` 12.5 — the
        // shipped values. 168 / (12.5 × 0.55) = 24.4, floored to 24.
        //
        // Fixed, not measured: this cell doesn't grow with the window, so
        // unlike the name budget there's nothing here for `responsive` to
        // do. What this pins is the pairing of *our* column width with
        // upstream's calibration — a `DATE_COLUMN` change, a token bump, or
        // an upstream drift in `unit_budget`'s average advance/floor all
        // surface here rather than on screen.
        assert_eq!(error_unit_budget(12.5), 24);
        // A wider column buys more message; a degenerate font size still
        // lands on upstream's floor rather than panicking or budgeting 0.
        assert!(saola_theme::overflow::unit_budget(336.0, 12.5) > error_unit_budget(12.5));
        assert_eq!(error_unit_budget(0.0), 4);
        assert_eq!(error_unit_budget(-12.5), 4);
    }

    #[test]
    fn a_long_rename_error_elides_inside_the_date_column() {
        // The real message a colliding rename produces: `VfsError::
        // AlreadyExists`'s `Display` is "{location} already exists", and
        // `location` is a whole absolute path (`modules::local::io_error`
        // words it against the `from` `Location`). At 24 units that's 23
        // characters of path plus the single `…` — the reader sees where
        // the failure is about, not a three-line wrap.
        let cut = |s: &str| saola_theme::overflow::truncate(s, error_unit_budget(12.5));

        assert_eq!(
            cut("/home/jordan/Documents/notes/report-2026-final.txt already exists"),
            "/home/jordan/Documents/…"
        );
        // `VfsError::IsADirectory` — what renaming a file onto an existing
        // folder produces, and the message the on-screen verification of
        // this change actually shows.
        assert_eq!(
            cut("/home/jordan/Documents/notes/ok.txt is a folder"),
            "/home/jordan/Documents/…"
        );
        // `submit_rename`'s own locally-worded rejection is short enough to
        // survive whole — no stray ellipsis on the common case.
        assert_eq!(cut("Enter a valid name"), "Enter a valid name");
        // And the date this cell normally shows (a fixed 16 characters
        // from `format_system_time`) is comfortably inside the same budget,
        // which is why `renaming_row` doesn't bother truncating it.
        assert_eq!(cut("2026-08-11 09:41"), "2026-08-11 09:41");
    }

    #[test]
    fn a_narrow_window_elides_every_script_the_same_way() {
        // 435 px is the list content area of a 667 px window — a third of
        // a 1706 px niri column, i.e. the app squeezed hard. Not derived
        // from chrome tokens but *measured*: at that window the on-screen
        // budget came out 19 units, which brackets the real content width
        // to [433.1, 440.5), and 435 sits inside it. Deriving it from a
        // token sum instead was off by a unit — which is the whole argument
        // for `responsive` measuring the width at runtime rather than any
        // code adding chrome up in its head.
        //
        // available = 435 − 2×8 − 84 − 168 − 16 − 8 = 143
        // 143 / 7.425 = 19.2 → 19 units. These are the exact names in the
        // on-screen verification tree at exactly that window width, so the
        // assertions here and the screenshots claim the same strings.
        let narrow = budget(435.0);
        assert_eq!(narrow, 19);
        let cut = |s: &str| saola_theme::overflow::truncate(s, narrow);

        // Long ASCII: 18 characters of prefix plus the single `…`.
        assert_eq!(
            cut(
                "the-quarterly-financial-report-for-fiscal-year-2026-final-revision-v3-approved-by-the-board-of-directors.txt"
            ),
            "the-quarterly-fina…"
        );
        // Full-width Japanese spends two units per glyph, so the same
        // budget buys nine characters instead of eighteen — which is the
        // point: both land at roughly the same pixel width. The tenth is
        // dropped rather than squeezed into the leftover unit, so the
        // result comes in *under* budget, never over.
        assert_eq!(
            cut("設定ウィンドウのタイトルはとても長いファイル名です.txt"),
            "設定ウィンドウのタ…"
        );
        // Three deer are six units before a single Latin character is
        // spent, which is exactly the accounting that keeps an emoji name
        // from running off the end of the column.
        assert_eq!(
            cut("🦌🦌🦌-saola-deer-emoji-filename.txt"),
            "🦌🦌🦌-saola-deer-…"
        );
        // Exactly at the budget: 19 narrow characters is 19 units, and the
        // cap is inclusive, so this comes back whole — no stray ellipsis on
        // a name that fits.
        assert_eq!(cut("exactly-19-units.md"), "exactly-19-units.md");
        // And a short name is never touched at all.
        assert_eq!(cut("ok.txt"), "ok.txt");
    }
}
