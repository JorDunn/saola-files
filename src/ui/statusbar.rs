//! The window's status bar: counts and selection size on the left, free
//! space and a capacity meter on the right.
//!
//! The band itself is `saola_theme::widget::status_bar` (hairline top edge,
//! no fill) and the text is `widget::text::meta`; nothing here styles
//! anything. This module decides *what the words are*, and keeps that
//! decision in plain functions over plain numbers so every state is a unit
//! test:
//!
//! | State                   | Left                                        |
//! |-------------------------|---------------------------------------------|
//! | listing in flight       | `Loading…`                                  |
//! | listing failed          | `Couldn't list this folder`                 |
//! | nothing selected        | `42 items` / `42 items, 3 hidden`           |
//! | empty folder            | `No items`                                  |
//! | files selected          | `3 of 42 selected, 12.4 MB`                 |
//! | files and folders       | `3 of 42 selected, 12.4 MB in files`        |
//! | only folders selected   | `2 of 42 selected` (no size: not known)     |
//! | Trash                   | `5 items in the trash` / `Trash is empty`   |
//!
//! The right side is `18.2 GB free` plus the meter, present only when the
//! caller has a [`FsStats`] to show — which it has only for a backend with
//! `Caps::STATFS` and a successful `statfs` (see `app::statusbar`). Absent
//! otherwise: no placeholder, no dash. How full the disk is is carried by
//! the meter and the number alone; there is no warning colour or wording.
//!
//! The bar is a pure function of the view it is given, so a tabs or split
//! feature gets one bar per pane by calling [`view`] with that pane's
//! [`ViewSummary`] and stats.

use iced::widget::{container, row};
use iced::{Alignment, Element, Length};
use saola_theme::{Surface, Theme, widget};

use crate::core::fs::format::human_size;
use crate::core::vfs::FsStats;
use crate::tr;
use crate::ui::dirview::summary::{Phase, ViewSummary};

/// The left-hand text for a directory view in `summary`'s state.
pub fn start_text(summary: &ViewSummary) -> String {
    match summary.phase {
        Phase::Loading => tr!("status-loading"),
        Phase::Failed => tr!("statusbar-unavailable"),
        Phase::Listed => {
            let selection = &summary.selection;
            if selection.count() == 0 {
                if summary.hidden > 0 {
                    tr!(
                        "statusbar-items-hidden",
                        count = summary.visible,
                        hidden = summary.hidden
                    )
                } else {
                    tr!("statusbar-items", count = summary.visible)
                }
            } else if selection.files == 0 {
                // Only folders: their size is unknown (never walked), so
                // there is nothing true to add after the count.
                tr!(
                    "statusbar-selected",
                    count = selection.count(),
                    total = summary.visible
                )
            } else if selection.folders == 0 {
                tr!(
                    "statusbar-selected-size",
                    count = selection.count(),
                    total = summary.visible,
                    size = human_size(selection.bytes)
                )
            } else {
                tr!(
                    "statusbar-selected-size-files",
                    count = selection.count(),
                    total = summary.visible,
                    size = human_size(selection.bytes)
                )
            }
        }
    }
}

/// The left-hand text for the Trash browser. It has no selection model and
/// no capacity of its own, so it states only the item count.
pub fn trash_text(items: usize, loading: bool, failed: bool) -> String {
    if loading {
        tr!("status-loading")
    } else if failed {
        tr!("statusbar-trash-unavailable")
    } else {
        tr!("statusbar-trash-items", count = items)
    }
}

/// "18.2 GB free". Uses `available` (what this user can write, `df`'s
/// "Avail"), not `free` (which includes space reserved for root).
pub fn free_text(stats: &FsStats) -> String {
    tr!("statusbar-free", size = human_size(stats.available))
}

/// How full the filesystem is, `0.0..=1.0`, the way `df` computes "Use%":
/// used / (used + available), where used is `total - free`. `None` when the
/// filesystem reports no size (some pseudo-filesystems), in which case the
/// whole right side is left out rather than showing an empty meter.
pub fn used_fraction(stats: &FsStats) -> Option<f32> {
    let used = stats.total.saturating_sub(stats.free);
    let denominator = used.saturating_add(stats.available);
    if stats.total == 0 || denominator == 0 {
        return None;
    }
    // f64 for the division so a multi-terabyte count keeps its precision;
    // the meter only needs an f32.
    Some((used as f64 / denominator as f64) as f32)
}

/// Width of the capacity meter. The theme's meter fills whatever width it is
/// given, so the bar must bound it.
///
/// PENDING TOKEN: this is a size derived from an unrelated token
/// (`menu_width / 3.0`), not a real one. saola-theme#45 ("Token: width for
/// the inline capacity meter") asks for the proper token. When that request
/// closes, bump the saola-theme pin to the release that ships it and replace
/// this function's body with the token.
fn meter_width(t: &Theme) -> f32 {
    t.sizes.menu_width / 3.0
}

/// The bar. `start` is the left text (from [`start_text`] or
/// [`trash_text`]); `free` is the right side, `None` to omit it.
///
/// `s` is the window's ground: the bar paints nothing of its own, so its
/// text sits on whatever the window is, and follows the `surface` knob.
pub fn view<'a, M: 'a>(
    t: &Theme,
    s: Surface,
    start: String,
    free: Option<&FsStats>,
) -> Element<'a, M> {
    let end: Element<'a, M> = match free.and_then(|stats| used_fraction(stats).map(|f| (stats, f)))
    {
        Some((stats, fraction)) => {
            let meter = container(widget::capacity_meter(t, s, fraction))
                .width(Length::Fixed(meter_width(t)));
            row![meter, widget::text::meta(t, s, free_text(stats))]
                .spacing(t.sizes.pill_gap)
                .align_y(Alignment::Center)
                .into()
        }
        None => row![].into(),
    };
    widget::status_bar(t, s, widget::text::meta(t, s, start), end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::dirview::summary::SelectionTotals;

    fn listed(visible: usize, hidden: usize, selection: SelectionTotals) -> ViewSummary {
        ViewSummary {
            phase: Phase::Listed,
            visible,
            hidden,
            selection,
        }
    }

    fn none() -> SelectionTotals {
        SelectionTotals::default()
    }

    #[test]
    fn nothing_selected_counts_items_with_plurals() {
        assert_eq!(start_text(&listed(0, 0, none())), "No items");
        assert_eq!(start_text(&listed(1, 0, none())), "1 item");
        assert_eq!(start_text(&listed(42, 0, none())), "42 items");
    }

    #[test]
    fn hidden_entries_are_mentioned_only_when_there_are_some() {
        assert_eq!(start_text(&listed(42, 3, none())), "42 items, 3 hidden");
        assert_eq!(start_text(&listed(1, 2, none())), "1 item, 2 hidden");
        // Nothing visible but hidden entries exist: say so, don't claim empty.
        assert_eq!(start_text(&listed(0, 2, none())), "0 items, 2 hidden");
    }

    #[test]
    fn a_selection_of_files_shows_count_and_size() {
        let selection = SelectionTotals {
            files: 3,
            folders: 0,
            bytes: 13_002_342,
        };
        assert_eq!(
            start_text(&listed(42, 0, selection)),
            "3 of 42 selected, 12.4 MB"
        );
        let one = SelectionTotals {
            files: 1,
            folders: 0,
            bytes: 512,
        };
        assert_eq!(start_text(&listed(5, 0, one)), "1 of 5 selected, 512 B");
    }

    #[test]
    fn a_selection_with_folders_says_the_size_is_files_only() {
        let selection = SelectionTotals {
            files: 2,
            folders: 1,
            bytes: 2048,
        };
        assert_eq!(
            start_text(&listed(42, 0, selection)),
            "3 of 42 selected, 2.0 KB in files"
        );
    }

    #[test]
    fn only_folders_selected_shows_no_size() {
        let selection = SelectionTotals {
            files: 0,
            folders: 2,
            bytes: 0,
        };
        assert_eq!(start_text(&listed(42, 0, selection)), "2 of 42 selected");
    }

    #[test]
    fn selection_wins_over_the_hidden_count() {
        let selection = SelectionTotals {
            files: 1,
            folders: 0,
            bytes: 1,
        };
        assert_eq!(
            start_text(&listed(42, 9, selection)),
            "1 of 42 selected, 1 B"
        );
    }

    #[test]
    fn loading_and_failed_listings_say_so_instead_of_counting() {
        let mut summary = listed(42, 3, none());
        summary.phase = Phase::Loading;
        assert_eq!(start_text(&summary), "Loading…");
        summary.phase = Phase::Failed;
        assert_eq!(start_text(&summary), "Couldn't list this folder");
    }

    #[test]
    fn the_trash_states() {
        assert_eq!(trash_text(0, false, false), "Trash is empty");
        assert_eq!(trash_text(1, false, false), "1 item in the trash");
        assert_eq!(trash_text(5, false, false), "5 items in the trash");
        assert_eq!(trash_text(5, true, false), "Loading…");
        assert_eq!(trash_text(0, false, true), "Couldn't read the trash");
    }

    #[test]
    fn free_text_uses_available_space_and_the_shared_size_format() {
        let stats = FsStats {
            total: 100 * 1024 * 1024 * 1024,
            free: 30 * 1024 * 1024 * 1024,
            available: 20 * 1024 * 1024 * 1024,
        };
        assert_eq!(free_text(&stats), "20.0 GB free");
    }

    #[test]
    fn used_fraction_matches_dfs_use_percent() {
        // df: used = total - free = 70; Use% = used / (used + avail) = 70/90.
        let stats = FsStats {
            total: 100,
            free: 30,
            available: 20,
        };
        let fraction = used_fraction(&stats).unwrap_or(f32::NAN);
        assert!((fraction - 70.0 / 90.0).abs() < 1e-6);
    }

    #[test]
    fn used_fraction_is_absent_for_a_filesystem_with_no_size() {
        let empty = FsStats {
            total: 0,
            free: 0,
            available: 0,
        };
        assert_eq!(used_fraction(&empty), None);
        // A full disk is 1.0, not an error.
        let full = FsStats {
            total: 10,
            free: 0,
            available: 0,
        };
        assert_eq!(used_fraction(&full), Some(1.0));
    }

    #[test]
    fn the_bar_builds_with_and_without_free_space() {
        let t = Theme::saola();
        let stats = FsStats {
            total: 100,
            free: 50,
            available: 50,
        };
        let _with: Element<'_, ()> = view(&t, Surface::Paper, "42 items".into(), Some(&stats));
        let _without: Element<'_, ()> = view(&t, Surface::Ink, "42 items".into(), None);
    }
}
