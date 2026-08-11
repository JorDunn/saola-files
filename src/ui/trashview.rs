//! The Trash browser (Stage 9): a synthetic, read-mostly directory-view-
//! shaped surface reachable from the sidebar's Trash place — list what's
//! in `$XDG_DATA_HOME/Trash`, restore an item, or empty everything. Not a
//! literal `DirectoryView` (it doesn't navigate, has no `Backend`, no
//! selection/keyboard-cursor model) — the stage's "synthetic
//! `DirectoryView`" wording means "the same *idea*: self-contained state
//! plus a `Message`/`Task`-driven `update`, composed beside the sidebar
//! the way `ui::explorer` composes a real `DirectoryView`", not the same
//! struct. `main.rs::App` swaps this in for the ordinary explorer body
//! whenever the active location is the sidebar's trash sentinel
//! (`core::places::trash_location()`) — see that function's doc comment
//! for the swap mechanics.
//!
//! Deliberately minimal for a first cut, documented as such rather than
//! silently thin:
//! - **No selection/keyboard model.** Each row's own "Restore" button acts
//!   on that row alone; there is no multi-select restore or keyboard
//!   navigation yet (`main.rs` also stops feeding the keyboard
//!   subscription into the hidden background `DirectoryView` while this is
//!   showing — see its own doc comment — so there's nothing *accidentally*
//!   reachable by keyboard here either). Trash browsing is a "get one
//!   thing back" surface, not a primary workflow, so this is a reasonable
//!   v1 scope cut, not an oversight.
//! - **No per-item permanent-delete-from-trash.** Only "Empty Trash"
//!   (everything at once, gated on `files.toml`'s `confirm-empty-trash`)
//!   and "Restore" (one item) exist.
//! - Every list/restore/empty call runs through `core::fs::trash` inside
//!   `tokio::task::spawn_blocking` (the same posture `modules::local`'s
//!   own `run_blocking` takes for its `std::fs` calls) — this view never
//!   touches `std::fs` itself, keeping the "trash is local-only,
//!   core/fs/trash.rs is the one exception" boundary in one place.

use std::path::PathBuf;

use iced::widget::{Space, button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Length, Task};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::entry::EntryKind;
use crate::core::fs::trash::{self, TrashedItem};
use crate::core::vfs::VfsError;

/// The two fixed cells at the tail of a trash row. Layout-specific to this
/// surface's row shape, not saola-theme design-system sizes — the same
/// distinction `ui::dirview::list`'s `SIZE_COLUMN`/`DATE_COLUMN` draw (and
/// `window.rs`'s `RESIZE_EDGE` before them).
///
/// They are *fixed* for the same reason the list's are: everything else in
/// the row is either a token-sized box (the glyph) or the one `Fill` cell
/// (the two label lines), so pinning these two is what makes the `Fill`
/// cell's width derivable from the row's own width. Left `Shrink`, both
/// would be font-measurement-dependent and no honest budget could be
/// computed for the labels beside them.
///
/// `DATE_COLUMN` carries `TrashedItem::deletion_date` — the `.trashinfo`'s
/// raw `YYYY-MM-DDThh:mm:ss`, always exactly 19 characters, in the mono
/// face at `typography.size.secondary`. 168 px is what `list.rs`'s own date
/// column already spends on the same face at the same size for a 16-
/// character stamp, so it clears this longer one with room to spare.
const DATE_COLUMN: f32 = 168.0;
/// `RESTORE_COLUMN` carries the row's Restore button, whose natural width
/// is `2 × paddings.strip[1]` (20) + `sizes.icon_row` (16) +
/// `sizes.gap_tight` (4) + the word "Restore" at `typography.size.label`
/// (~42 px at the design language's 0.55 em average advance) ≈ 82 px. 104
/// leaves the label a comfortable margin without letting the button's
/// chrome grow into the name.
const RESTORE_COLUMN: f32 = 104.0;

/// How many *width units* of text fit on one of a trash row's two label
/// lines at a given view width — this surface's analogue of
/// `ui::dirview::list::name_unit_budget`, and derived the same way: from a
/// measured width rather than a token, because the labels cell is the row's
/// `Fill` cell.
///
/// Every subtraction is one thing the row actually spends its width on,
/// read straight off `item_row`'s layout:
///
/// - `2.0 * pill_gap` — the row's own `padding([pill_gap / 2, pill_gap])`,
///   one gap on each side.
/// - `icon_row` — the kind glyph at the head of the row.
/// - `3.0 * pill_gap` — the row's `.spacing(pill_gap)`, which falls in the
///   three seams between its four children (glyph, labels, date, Restore).
/// - `DATE_COLUMN` and `RESTORE_COLUMN` — the two fixed cells above, which
///   take their width before the `Fill` labels cell sees any.
///
/// Nothing is subtracted for the scrollbar, deliberately, for the reason
/// `list.rs::name_unit_budget` spells out in full: iced's default
/// `Scrollable` reserves no layout width for its bar (this file never sets
/// `.spacing()` on the scrollable, which is what would turn the overlay
/// into a reserved gutter), so subtracting a guessed bar width would be
/// paying for space that was never taken.
///
/// Called twice per row, with a different `font` each time, because the two
/// lines are set at different sizes: the name at `typography.size.body`,
/// the original path at `typography.size.secondary`. They share the same
/// `available` because they share the same cell.
///
/// A window squeezed narrower than the two fixed cells makes `available`
/// negative. That is fine and upstream-documented: `unit_budget` saturates
/// a negative through `as usize` to 0 and then clamps to its floor of 4, so
/// the worst case is a name eliding to three narrow characters plus the
/// `…` — never a panic.
///
/// Pure function of four `f32`s, which is what makes it testable below
/// without a `Theme` or a renderer.
fn label_unit_budget(view_width: f32, font: f32, pill_gap: f32, icon_row: f32) -> usize {
    let available =
        view_width - 2.0 * pill_gap - icon_row - 3.0 * pill_gap - DATE_COLUMN - RESTORE_COLUMN;
    saola_theme::overflow::unit_budget(available, font)
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A `core::fs::trash::list` response — from the initial `load()` the
    /// owner kicks off when switching into Trash, and from every reload
    /// this view triggers itself after a restore/empty.
    Loaded(Result<Vec<TrashedItem>, VfsError>),
    /// A row's "Restore" button, by its index into `items`.
    RestoreRequested(usize),
    /// The `core::fs::trash::restore` call `RestoreRequested` kicks off.
    /// Either outcome reloads the list (see `update`'s doc comment on that
    /// arm) — there's no per-item error slot this stage, a failed restore
    /// is worded to stderr and the row simply stays put.
    RestoreResult(usize, Result<PathBuf, VfsError>),
    /// The toolbar's "Empty Trash" button.
    EmptyRequested,
    /// The inline confirm strip's "Empty Trash" button (only reachable
    /// when `files.toml`'s `confirm-empty-trash` is true).
    EmptyConfirmClicked,
    /// The inline confirm strip's "Cancel" button.
    EmptyCancelClicked,
    EmptyResult(Result<(), VfsError>),
}

pub struct TrashView {
    items: Vec<TrashedItem>,
    loading: bool,
    error: Option<String>,
    /// `files.toml`'s `confirm-empty-trash` knob, baked in at construction
    /// — the same "config knobs become fixed per-view state, not re-read
    /// live" posture `DirectoryView::new` takes for `sort`/`view_mode`/etc.
    confirm_empty_trash: bool,
    /// `true` while the inline "really empty the trash?" strip is showing
    /// in place of the ordinary toolbar — only reachable when
    /// `confirm_empty_trash` is set; otherwise `EmptyRequested` empties
    /// immediately and this never becomes `true`.
    confirming_empty: bool,
}

impl TrashView {
    pub fn new(confirm_empty_trash: bool) -> Self {
        TrashView {
            items: Vec::new(),
            loading: false,
            error: None,
            confirm_empty_trash,
            confirming_empty: false,
        }
    }

    /// Fetches the trash's current contents — the owner calls this once
    /// when switching into Trash view; `update` calls it again itself
    /// after every restore/empty so the list is always what's actually on
    /// disk, never optimistically edited in place.
    pub fn load(&mut self) -> Task<Message> {
        self.loading = true;
        self.error = None;
        Task::perform(run_blocking(trash::list), Message::Loaded)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(items) => {
                        self.items = items;
                        self.error = None;
                    }
                    Err(err) => {
                        self.items.clear();
                        self.error = Some(err.to_string());
                    }
                }
                Task::none()
            }
            Message::RestoreRequested(index) => {
                let Some(item) = self.items.get(index).cloned() else {
                    return Task::none();
                };
                Task::perform(
                    run_blocking(move || trash::restore(&item.id)),
                    move |result| Message::RestoreResult(index, result),
                )
            }
            Message::RestoreResult(_index, result) => {
                if let Err(err) = &result {
                    eprintln!("saola-files: couldn't restore from trash: {err}");
                }
                // Reload either way: success removes the restored row,
                // failure just re-confirms it's still there — there's no
                // per-item error slot this stage (see the module doc
                // comment's stated gaps).
                self.load()
            }
            Message::EmptyRequested => {
                if self.confirm_empty_trash {
                    self.confirming_empty = true;
                    Task::none()
                } else {
                    self.start_empty()
                }
            }
            Message::EmptyConfirmClicked => {
                self.confirming_empty = false;
                self.start_empty()
            }
            Message::EmptyCancelClicked => {
                self.confirming_empty = false;
                Task::none()
            }
            Message::EmptyResult(result) => {
                if let Err(err) = &result {
                    eprintln!("saola-files: couldn't empty the trash: {err}");
                }
                self.load()
            }
        }
    }

    fn start_empty(&mut self) -> Task<Message> {
        Task::perform(run_blocking(trash::empty), Message::EmptyResult)
    }

    pub fn view<'a>(&'a self, t: &'a Theme) -> Element<'a, Message> {
        let toolbar_row = toolbar(t, self.items.is_empty(), self.confirming_empty);

        let body: Element<'a, Message> = if let Some(err) = &self.error {
            saola_theme::widget::empty_state(t, Surface::Paper, err)
        } else if self.items.is_empty() {
            let message = if self.loading {
                "Loading…"
            } else {
                "Trash is empty"
            };
            saola_theme::widget::empty_state(t, Surface::Paper, message)
        } else {
            // `responsive` is how a widget learns its own width in iced
            // 0.14: the closure runs *inside* `Widget::layout`, after
            // limits are known, and is handed the `Size` this container
            // actually got — the only place in a `view()` where a real
            // pixel width exists, which is why the label budgets below can
            // be measurements instead of guesses. Same mechanism, same
            // posture as `ui::dirview::list::view`; see its comment for the
            // two rules the closure has to honour (it is `Fn`, re-run on
            // every relayout, and everything it captures — `&TrashView`,
            // `&Theme` — is a shared reference, which is `Copy`; and it
            // returns the same tree shape every run, so the scrollable's
            // widget state, including its offset, survives a resize).
            //
            // It wraps only this branch, after the error/empty early
            // returns, exactly as the list view does: those branches build
            // a differently-shaped tree, and they change with *state*, not
            // with width.
            iced::widget::responsive(move |size| {
                let name_units = label_unit_budget(
                    size.width,
                    t.typography.size.body,
                    t.sizes.pill_gap,
                    t.sizes.icon_row,
                );
                let path_units = label_unit_budget(
                    size.width,
                    t.typography.size.secondary,
                    t.sizes.pill_gap,
                    t.sizes.icon_row,
                );
                let rows: Vec<Element<'a, Message>> = self
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| item_row(t, index, item, name_units, path_units))
                    .collect();
                scrollable(column(rows).width(Fill))
                    .style(style::scrollable::rest(t, Surface::Paper))
                    .width(Fill)
                    .height(Fill)
                    .into()
            })
            .into()
        };

        // `sizes.island_gap` between the toolbar island and the listing
        // below it — the identical seam `ui::explorer::view` puts between
        // `ui::header`'s toolbar and the directory view, so switching to
        // the trash browser doesn't change the window's rhythm.
        column![toolbar_row, body]
            .spacing(t.sizes.island_gap)
            .width(Fill)
            .height(Fill)
            .into()
    }
}

/// Runs `f` on the blocking pool, turning a `JoinError` (the task
/// panicked) into a worded `VfsError` rather than propagating a panic —
/// the no-panic rule extends to background work, the same posture
/// `modules::local::run_blocking` already takes for its own `std::fs`
/// calls (not reused directly: that one is private to `modules::local` and
/// keyed to a `Location` for its error wording, this one to a bare
/// message, since `core::fs::trash`'s own functions already word their
/// errors against a `Path`).
async fn run_blocking<T, F>(f: F) -> Result<T, VfsError>
where
    F: FnOnce() -> Result<T, VfsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(VfsError::Other {
            message: format!("internal error talking to the trash: {join_err}"),
        }),
    }
}

fn toolbar<'a>(t: &'a Theme, items_empty: bool, confirming: bool) -> Element<'a, Message> {
    if confirming {
        return confirm_strip(t);
    }

    let title = text("Trash")
        .size(t.typography.size.dialog_title)
        .font(convert::display_font(t))
        .color(t.on_paper.primary.into_iced());

    let empty_content = row![
        icon::icon(
            Icon::Trash2,
            t.sizes.icon_row,
            t.on_paper.primary.into_iced()
        ),
        text("Empty Trash")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);
    let mut empty_button = button(empty_content)
        .style(style::button::rest(t, Surface::Paper))
        .padding(t.paddings.pill_button);
    if !items_empty {
        empty_button = empty_button.on_press(Message::EmptyRequested);
    }

    // The trash browser's toolbar is the same chrome region `ui::header`'s
    // is, so it takes the same recessed `style::container::inset` ground
    // (Stage 12) rather than sitting on the listing's paper.
    container(
        row![title, Space::new().width(Fill), empty_button]
            .align_y(Center)
            .width(Fill),
    )
    .style(style::container::inset(t, Surface::Paper))
    .width(Fill)
    .height(t.sizes.window_header)
    .padding([0.0, t.sizes.pill_gap])
    .align_y(Center)
    .into()
}

/// The inline "really empty the trash?" strip — replaces the ordinary
/// toolbar in place (not a modal: CLAUDE.md's severity-by-wording posture
/// doesn't call for a scrim/dialog for a single static yes/no, unlike the
/// per-item conflict dialog `core::fs::ops` drives, which carries real
/// per-conflict data). `App` never sees this state at all — it's entirely
/// local to `TrashView`, the same "self-contained per-surface state"
/// posture `DirectoryView`'s own inline-rename state takes.
fn confirm_strip<'a>(t: &'a Theme) -> Element<'a, Message> {
    let label = text("Permanently delete everything in the trash?")
        .size(t.typography.size.body)
        .font(convert::ui_font(t))
        .color(t.on_paper.primary.into_iced());

    let cancel = button(
        text("Cancel")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper))
    .padding(t.paddings.pill_button)
    .on_press(Message::EmptyCancelClicked);

    let confirm = button(
        text("Empty Trash")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper))
    .padding(t.paddings.pill_button)
    .on_press(Message::EmptyConfirmClicked);

    // Takes the toolbar's own `style::container::inset` ground (Stage 12):
    // it *replaces* the toolbar in place, so it has to occupy the same
    // region, at the same height, on the same surface — otherwise
    // confirming would look like the chrome band vanished.
    container(
        row![label, Space::new().width(Fill), cancel, confirm]
            .spacing(t.sizes.pill_gap)
            .align_y(Center),
    )
    .style(style::container::inset(t, Surface::Paper))
    .width(Fill)
    .height(t.sizes.window_header)
    .padding([0.0, t.sizes.pill_gap])
    .align_y(Center)
    .into()
}

/// One trash row: glyph, the two label lines, the deletion stamp, Restore.
///
/// `name_units`/`path_units` are the two width budgets `view` measured for
/// this frame (see [`label_unit_budget`]) — the row is handed them rather
/// than deriving them itself, so every row in a frame elides against the
/// same measurement.
fn item_row<'a>(
    t: &'a Theme,
    index: usize,
    item: &'a TrashedItem,
    name_units: usize,
    path_units: usize,
) -> Element<'a, Message> {
    let glyph = if item.is_symlink {
        Icon::Link
    } else if item.kind == EntryKind::Directory {
        Icon::Folder
    } else {
        Icon::File
    };
    let icon = icon::icon(glyph, t.sizes.icon_row, t.on_paper.primary.into_iced());

    let name = item
        .original_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| item.original_path.display().to_string());

    // Two belts for one job on both lines, the same pairing `list.rs` and
    // `grid.rs` draw at their own labels.
    //
    // `truncate` is the style guide's honest answer to text that doesn't
    // fit (§7: cut at a width limit, exactly one `…` — one glyph, never
    // three dots — and no motion), spending its budget in UAX #11 width
    // units so a CJK name or path lands at about the same pixel width as a
    // Latin one instead of running twice as long.
    //
    // `Wrapping::None` is the hard guarantee for what a unit-counted budget
    // still can't see — the spread *within* narrow characters (`mmmm` vs
    // `llll`), and, on the path line, that the mono face's advance runs a
    // little wider than the design language's 0.55 em average (calibrated
    // for the proportional UI face; there is no mono-specific budget
    // upstream yet, and inventing a local fudge factor here would be
    // exactly the local restyling CLAUDE.md forbids). Without it a long
    // name became two lines and the row grew taller than its neighbours;
    // with it the residue is at worst a few pixels of lean into the gap
    // before the date cell.
    //
    // Only the *rendering* is shortened. `item.original_path` and
    // `item.id` are untouched, so Restore still keys off the real path —
    // nothing downstream ever sees an elided string.
    let name_text = text(saola_theme::overflow::truncate(&name, name_units))
        .size(t.typography.size.body)
        .font(convert::ui_font(t))
        .wrapping(iced::widget::text::Wrapping::None);
    let original = text(saola_theme::overflow::truncate(
        &item.original_path.display().to_string(),
        path_units,
    ))
    .size(t.typography.size.secondary)
    .font(convert::mono_font(t))
    .color(t.on_paper.secondary.into_iced())
    .wrapping(iced::widget::text::Wrapping::None);
    let labels = column![name_text, original].spacing(2.0).width(Fill);

    let date = text(item.deletion_date.clone())
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t))
        .color(t.on_paper.secondary.into_iced());

    let restore_content = row![
        icon::icon(
            Icon::RotateCcw,
            t.sizes.icon_row,
            t.on_paper.primary.into_iced()
        ),
        text("Restore")
            .size(t.typography.size.label)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.gap_tight)
    .align_y(Center);
    let restore = button(restore_content)
        .style(style::button::bare(t, Surface::Paper))
        .padding(t.paddings.strip)
        .width(Length::Fixed(RESTORE_COLUMN))
        .on_press(Message::RestoreRequested(index));

    // The date and Restore cells are pinned to their declared widths (see
    // `DATE_COLUMN`/`RESTORE_COLUMN`): `label_unit_budget` subtracts exactly
    // these two numbers, so the layout has to actually spend exactly these
    // two numbers or the budget would be arithmetic about a row that never
    // existed.
    let content = row![
        icon,
        labels,
        container(date).width(Length::Fixed(DATE_COLUMN)),
        restore
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center)
    .padding([t.sizes.pill_gap / 2.0, t.sizes.pill_gap]);

    container(content).width(Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Today's tokens, spelled out once: `sizes.pill_gap` 8,
    /// `sizes.icon_row` 16, `typography.size.body` 13.5,
    /// `typography.size.secondary` 12.5.
    fn name_budget(width: f32) -> usize {
        label_unit_budget(width, 13.5, 8.0, 16.0)
    }

    fn path_budget(width: f32) -> usize {
        label_unit_budget(width, 12.5, 8.0, 16.0)
    }

    #[test]
    fn label_budgets_at_todays_tokens() {
        // 866 px is roughly what this column works out to at the default
        // 1100×720 window, once the window border, the island gaps and the
        // 200 px sidebar are taken out — the trash browser is composed into
        // the same region geometry as the ordinary directory view (see
        // `main.rs::view`'s trash branch), so it is the same measurement
        // `ui::dirview::list`'s own budget test pins against. Representative
        // for pinning arithmetic, not a contract: the runtime budget comes
        // from the measured size, so no chrome token can drift the feature
        // out of true — only this number.
        //
        // available = 866 − 2×8 − 16 − 3×8 − 168 − 104 = 538
        // name: 538 / (13.5 × 0.55) = 72.4 → 72
        // path: 538 / (12.5 × 0.55) = 78.2 → 78
        assert_eq!(name_budget(866.0), 72);
        assert_eq!(path_budget(866.0), 78);
        // The path line is set smaller, so it always buys at least as many
        // units out of the same cell as the name line does.
        assert!(path_budget(866.0) > name_budget(866.0));
    }

    #[test]
    fn label_budgets_widen_with_the_window() {
        // Half a niri column against a full one, and one more widening
        // pair: more width has to buy more label, or the feature does
        // nothing on resize.
        assert!(name_budget(435.0) < name_budget(866.0));
        assert!(name_budget(866.0) < name_budget(1732.0));
        assert!(path_budget(435.0) < path_budget(866.0));
        assert!(path_budget(866.0) < path_budget(1732.0));
    }

    #[test]
    fn label_budgets_never_fall_below_the_floor() {
        // A window squeezed under the two fixed cells drives `available`
        // negative (330 − 16 − 16 − 24 − 168 − 104 = 2, and it only gets
        // worse from there). Upstream saturates that to the floor of 4
        // rather than panicking or computing a budget of 0 — CLAUDE.md's
        // no-panic rule reaching all the way down into layout.
        assert_eq!(name_budget(330.0), 4);
        assert_eq!(name_budget(0.0), 4);
        assert_eq!(name_budget(-50.0), 4);
        assert_eq!(path_budget(-50.0), 4);
    }

    #[test]
    fn a_narrow_trash_column_elides_both_lines() {
        // 435 px is the same "app squeezed to a third of a niri column"
        // width `ui::dirview::list`'s tests use — see that test's comment
        // for why it is a measured number rather than a token sum.
        //
        // available = 435 − 16 − 16 − 24 − 168 − 104 = 107
        // name: 107 / 7.425 = 14.4 → 14 units
        // path: 107 / 6.875 = 15.5 → 15 units
        let name_units = name_budget(435.0);
        let path_units = path_budget(435.0);
        assert_eq!(name_units, 14);
        assert_eq!(path_units, 15);

        let name = |s: &str| saola_theme::overflow::truncate(s, name_units);
        let path = |s: &str| saola_theme::overflow::truncate(s, path_units);

        // The long ASCII name from the on-screen verification tree: 13
        // characters of prefix plus the single `…`.
        assert_eq!(
            name(
                "the-quarterly-financial-report-for-fiscal-year-2026-final-revision-v3-approved-by-the-board-of-directors.txt"
            ),
            "the-quarterly…"
        );
        // Full-width Japanese spends two units per glyph, so the same
        // budget buys six characters instead of thirteen — which is the
        // point: both land at roughly the same pixel width. The seventh is
        // dropped rather than squeezed into the leftover unit, so the
        // result comes in *under* budget, never over.
        assert_eq!(
            name("設定ウィンドウのタイトルはとても長いファイル名です.txt"),
            "設定ウィンド…"
        );
        // A short name is never touched at all — no stray ellipsis on the
        // common case.
        assert_eq!(name("ok.txt"), "ok.txt");

        // The original-path line is the one that overflows first in
        // practice: an absolute path is long by construction.
        assert_eq!(
            path("/home/jordan/Documents/receipts/2026/quarterly-report.pdf"),
            "/home/jordan/D…"
        );
        assert_eq!(
            path("/home/jordan/書類/設定ウィンドウのタイトル.txt"),
            "/home/jordan/…"
        );
        // A path that already fits comes back whole (15 narrow characters
        // is exactly 15 units, and the cap is inclusive).
        assert_eq!(path("/tmp/x/ok.txt"), "/tmp/x/ok.txt");
    }

    #[test]
    fn new_view_starts_empty_and_not_confirming() {
        let view = TrashView::new(true);
        assert!(view.items.is_empty());
        assert!(!view.confirming_empty);
        assert!(!view.loading);
    }

    #[test]
    fn empty_requested_without_confirmation_configured_never_shows_the_strip() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::EmptyRequested);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn empty_requested_with_confirmation_configured_shows_the_strip() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        assert!(view.confirming_empty);
    }

    #[test]
    fn cancelling_the_confirm_strip_clears_it() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        let _ = view.update(Message::EmptyCancelClicked);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn confirming_the_strip_clears_it_too() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        let _ = view.update(Message::EmptyConfirmClicked);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn restore_requested_out_of_range_is_a_safe_no_op() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::RestoreRequested(0));
        assert!(view.items.is_empty());
    }

    #[test]
    fn loaded_error_words_the_failure_and_clears_any_previous_items() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::Loaded(Err(VfsError::PermissionDenied {
            location: "/x".to_owned(),
        })));
        assert!(view.items.is_empty());
        assert!(view.error.is_some());
    }

    #[test]
    fn a_later_successful_load_clears_a_previous_error() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::Loaded(Err(VfsError::Other {
            message: "x".to_owned(),
        })));
        assert!(view.error.is_some());
        let _ = view.update(Message::Loaded(Ok(Vec::new())));
        assert!(view.error.is_none());
    }
}
