//! The error notice: how a failed operation shows up in the window.
//!
//! A copy that hits a read-only folder, a file with no app to open it, a
//! New Folder that can't be created: each used to print a line to stderr
//! and nothing else. This notice puts the sentence
//! (`core::failure::describe`) in the window, and keeps it there until the
//! person dismisses it (the close button, or Esc via `App`) or a newer
//! incident replaces it. Navigating does not clear it.
//!
//! # Where it sits (the ordering rule)
//!
//! The footer band is a stack, top to bottom: the window body, then this
//! notice, then the progress strip or undo toast. The notice is a row in
//! that column, not an overlay, so it shrinks the body instead of covering
//! anything, and **never covers a running operation's progress strip**,
//! which always stays the bottom-most thing in the window. The
//! Details list, when open, is a card directly above the notice's own strip.
//!
//! # One notice = one incident
//!
//! Every failure carries an [`Incident`]: one user action (or one engine
//! operation), minted when it started. [`Slot::report`] decides:
//!
//! - same incident as the notice showing: the failure joins it ("3 items
//!   couldn't be trashed" grows from two to three);
//! - a newer incident: replaces the notice, collapsed;
//! - an older (or already-dismissed) incident: dropped. It is still on
//!   stderr; a straggler from an earlier action must not overwrite what the
//!   person is looking at, nor resurrect a notice they dismissed.
//!
//! Two unrelated copies ten minutes apart are two incidents, so they never
//! read as "2 items couldn't be copied".
//!
//! # Bounded
//!
//! A failed 100k-item delete must not hold or format 100k failures. The
//! notice stores a total count and only the first [`MAX_DETAIL_LINES`]
//! failures (the rest are dropped on arrival), and its text is built once
//! per report, not per frame.
//!
//! # Severity is wording
//!
//! No danger colour exists in the style guide. The strip is the same
//! `footer_strip` the progress strip and undo toast use, with an
//! `Icon::Info` glyph; the sentence says what is wrong.
//!
//! State lives here and is mutated by [`update`]; `App` owns the [`Slot`]
//! and the Esc routing. Like the other dialogs this module holds no clock
//! and ticks nothing.

use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, responsive, row, text};
use iced::{Center, Element, Fill, Shrink};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, overflow, style, widget};

use crate::core::failure::{self, Failure, Incident, Operation, Summary};
use crate::tr;

/// Failures kept (and detail lines drawn) per notice; the rest are counted
/// ("…and N more") and dropped.
pub const MAX_DETAIL_LINES: usize = 8;

/// Width reserved for the Details button when working out how much of the
/// headline fits: its label is the longer of two translated words, so its
/// width is not known up front. This is a local fixed column width, a
/// generous guess rather than a measurement, pending a saola-theme token
/// for it; the precedent is the fixed `SIZE_COLUMN`/`DATE_COLUMN` in
/// `ui/dirview/list.rs`.
const DETAILS_COLUMN: f32 = 120.0;

#[derive(Debug, Clone)]
pub enum Message {
    /// The close button (and Esc, which `App` turns into this).
    Dismiss,
    /// The Details / Hide details button.
    ToggleDetails,
    /// Something failed: show it. This is the message other code sends;
    /// `App` has a helper that builds it.
    Report(Incident, Operation, Vec<Failure>),
}

/// What the notice is currently telling the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    incident: Incident,
    operation: Operation,
    /// The first [`MAX_DETAIL_LINES`] failures; never more.
    failures: Vec<Failure>,
    /// How many failed in all, including those not stored.
    total: usize,
    expanded: bool,
    /// Text for `failures`/`total`, rebuilt only when they change.
    summary: Summary,
}

impl Notice {
    fn new(incident: Incident, operation: Operation) -> Self {
        Notice {
            incident,
            operation,
            failures: Vec::new(),
            total: 0,
            expanded: false,
            summary: Summary {
                headline: String::new(),
                details: Vec::new(),
            },
        }
    }

    /// Counts `failures`, keeps what fits, and rebuilds the text.
    fn add(&mut self, failures: Vec<Failure>, omitted: usize) {
        self.total = self
            .total
            .saturating_add(failures.len().saturating_add(omitted));
        let room = MAX_DETAIL_LINES.saturating_sub(self.failures.len());
        self.failures.extend(failures.into_iter().take(room));
        if let Some(summary) = failure::summarize(self.operation, &self.failures, self.total) {
            self.summary = summary;
        }
    }

    pub fn incident(&self) -> Incident {
        self.incident
    }

    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// How many failed in all (more than are stored).
    pub fn total(&self) -> usize {
        self.total
    }

    /// The stored failures: at most [`MAX_DETAIL_LINES`].
    pub fn failures(&self) -> &[Failure] {
        &self.failures
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// The sentence(s) to draw.
    pub fn summary(&self) -> &Summary {
        &self.summary
    }
}

/// The notice (if any) plus the newest incident ever reported, which
/// outlives a dismissal so a late straggler can't bring the notice back.
#[derive(Debug, Default)]
pub struct Slot {
    notice: Option<Notice>,
    newest: Option<Incident>,
}

impl Slot {
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub fn is_showing(&self) -> bool {
        self.notice.is_some()
    }

    /// Applies the one-notice-per-incident rule (module docs). An empty
    /// list changes nothing. Returns whether the failures were shown.
    pub fn report(
        &mut self,
        incident: Incident,
        operation: Operation,
        failures: Vec<Failure>,
    ) -> bool {
        self.report_omitting(incident, operation, failures, 0)
    }

    /// [`Self::report`] for an engine that kept only the first few
    /// failures: `omitted` more failed and are counted in the headline.
    pub fn report_omitting(
        &mut self,
        incident: Incident,
        operation: Operation,
        failures: Vec<Failure>,
        omitted: usize,
    ) -> bool {
        if failures.is_empty() {
            return false;
        }
        if let Some(notice) = self.notice.as_mut()
            && notice.incident == incident
        {
            notice.add(failures, omitted);
            return true;
        }
        if self.newest.is_some_and(|newest| incident <= newest) {
            return false;
        }
        self.newest = Some(incident);
        let mut notice = Notice::new(incident, operation);
        notice.add(failures, omitted);
        self.notice = Some(notice);
        true
    }

    pub fn dismiss(&mut self) {
        self.notice = None;
    }

    pub fn toggle_details(&mut self) {
        if let Some(notice) = self.notice.as_mut() {
            notice.expanded = !notice.expanded;
        }
    }
}

/// Applies `message` to the slot.
pub fn update(slot: &mut Slot, message: Message) {
    match message {
        Message::Dismiss => slot.dismiss(),
        Message::ToggleDetails => slot.toggle_details(),
        Message::Report(incident, operation, failures) => {
            slot.report(incident, operation, failures);
        }
    }
}

/// Renders the notice: an optional Details card above a `footer_strip`
/// holding the glyph, the headline, a Details toggle (only when more than
/// one thing failed) and the close button.
///
/// Long names are cut with `overflow::truncate` (one `…`), budgeted from the
/// width the notice really has: `responsive` hands the closure the notice's
/// measured width, the same technique as `ui::trashview`'s rows.
pub fn view<'a>(t: &'a Theme, notice: &'a Notice) -> Element<'a, Message> {
    let has_details = !notice.summary.details.is_empty();
    responsive(move |size| {
        let font = convert::ui_font(t);
        let font_size = t.typography.size.secondary;
        let glyph = icon::icon(Icon::Info, t.sizes.icon_row, t.on_paper.primary.into_iced());

        // What the strip spends its width on besides the headline: its own
        // padding, the glyph, the close button (glyph + the button's
        // padding), the gaps between children, and the Details button.
        let strip_padding = t.sizes.popover_padding;
        let close_width = t.sizes.icon_row + 2.0 * t.paddings.strip[1];
        let children = if has_details { 4.0 } else { 3.0 };
        let reserved = strip_padding
            + t.sizes.icon_row
            + close_width
            + (children - 1.0) * t.sizes.pill_gap
            + if has_details { DETAILS_COLUMN } else { 0.0 };
        let headline_units = overflow::unit_budget(size.width - reserved, font_size);

        let headline = text(overflow::truncate(&notice.summary.headline, headline_units))
            .size(font_size)
            .font(font)
            .wrapping(Wrapping::None)
            .width(Fill);

        let mut strip = row![glyph, headline]
            .spacing(t.sizes.pill_gap)
            .align_y(Center)
            .width(Fill);

        if has_details {
            let label = if notice.expanded {
                tr!("error-notice-hide-details")
            } else {
                tr!("error-notice-details")
            };
            strip = strip.push(
                button(text(label).size(font_size).font(font))
                    .style(style::button::bare(t, Surface::Paper))
                    .padding(t.paddings.strip)
                    .on_press(Message::ToggleDetails),
            );
        }

        strip = strip.push(
            button(icon::icon(
                Icon::X,
                t.sizes.icon_row,
                t.on_paper.primary.into_iced(),
            ))
            .style(style::button::bare(t, Surface::Paper))
            .padding(t.paddings.strip)
            .on_press(Message::Dismiss),
        );

        let strip = widget::footer_strip(t, Surface::Paper, strip);

        if has_details && notice.expanded {
            let card_padding = t.sizes.popover_padding;
            let line_units = overflow::unit_budget(size.width - card_padding, font_size);
            let mut lines: Vec<Element<'_, Message>> = notice
                .summary
                .details
                .iter()
                .map(|line| {
                    text(overflow::truncate(line, line_units))
                        .size(font_size)
                        .font(font)
                        .wrapping(Wrapping::None)
                        .into()
                })
                .collect();
            if notice.total > notice.failures.len() {
                lines.push(
                    text(tr!(
                        "error-notice-more",
                        count = notice.total - notice.failures.len()
                    ))
                    .size(font_size)
                    .font(font)
                    .into(),
                );
            }
            let card = container(column(lines).spacing(t.sizes.gap_tight).width(Fill))
                .style(style::container::card(t, Surface::Paper))
                .width(Fill)
                .padding([t.sizes.pill_gap / 2.0, t.sizes.popover_padding / 2.0]);
            column![card, strip].width(Fill).into()
        } else {
            strip
        }
    })
    .width(Fill)
    .height(Shrink)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vfs::VfsError;

    fn failure(name: &str) -> Failure {
        Failure::new(
            name,
            VfsError::PermissionDenied {
                location: name.to_owned(),
            },
        )
    }

    fn two_incidents() -> (Incident, Incident) {
        (Incident::mint(), Incident::mint())
    }

    #[test]
    fn a_report_into_an_empty_slot_shows_it() {
        let (first, _) = two_incidents();
        let mut slot = Slot::default();
        assert!(slot.report(first, Operation::Copy, vec![failure("a")]));
        let notice = slot.notice().expect("a notice");
        assert_eq!(notice.operation(), Operation::Copy);
        assert_eq!(notice.total(), 1);
        assert!(!notice.is_expanded());
    }

    #[test]
    fn omitted_failures_count_toward_the_total() {
        let mut slot = Slot::default();
        let incident = Incident::mint();
        slot.report_omitting(
            incident,
            Operation::Delete,
            vec![failure("a"), failure("b")],
            98,
        );
        assert_eq!(slot.notice().expect("a notice").total(), 100);
    }

    #[test]
    fn an_empty_report_changes_nothing() {
        let (first, second) = two_incidents();
        let mut slot = Slot::default();
        assert!(!slot.report(first, Operation::Copy, Vec::new()));
        assert!(!slot.is_showing());
        slot.report(first, Operation::Copy, vec![failure("a")]);
        assert!(!slot.report(second, Operation::Move, Vec::new()));
        assert_eq!(slot.notice().map(Notice::incident), Some(first));
    }

    #[test]
    fn failures_of_the_same_incident_aggregate() {
        let (first, _) = two_incidents();
        let mut slot = Slot::default();
        for name in ["a", "b", "c"] {
            slot.report(first, Operation::Trash, vec![failure(name)]);
        }
        let notice = slot.notice().expect("a notice");
        assert_eq!(notice.total(), 3);
        assert_eq!(
            notice.summary().headline,
            "3 items couldn't be moved to the trash"
        );
        assert_eq!(notice.summary().details.len(), 3);
    }

    #[test]
    fn a_new_incident_of_the_same_kind_replaces_rather_than_joins() {
        let (first, second) = two_incidents();
        let mut slot = Slot::default();
        slot.report(first, Operation::Copy, vec![failure("a")]);
        slot.report(second, Operation::Copy, vec![failure("b")]);
        let notice = slot.notice().expect("a notice");
        assert_eq!(notice.incident(), second);
        assert_eq!(notice.total(), 1);
        assert!(notice.summary().headline.contains("“b”"));
    }

    #[test]
    fn a_late_failure_from_an_older_incident_does_not_overwrite() {
        let (first, second) = two_incidents();
        let mut slot = Slot::default();
        slot.report(second, Operation::Open, vec![failure("new")]);
        assert!(!slot.report(first, Operation::Copy, vec![failure("old")]));
        let notice = slot.notice().expect("a notice");
        assert_eq!(notice.incident(), second);
        assert_eq!(notice.operation(), Operation::Open);
    }

    #[test]
    fn a_straggler_cannot_bring_back_a_dismissed_notice() {
        let (first, second) = two_incidents();
        let mut slot = Slot::default();
        slot.report(second, Operation::Open, vec![failure("a")]);
        slot.dismiss();
        assert!(!slot.report(first, Operation::Copy, vec![failure("old")]));
        assert!(!slot.report(second, Operation::Open, vec![failure("late")]));
        assert!(!slot.is_showing());
    }

    #[test]
    fn the_notice_persists_until_dismissed_or_replaced() {
        // Nothing in the slot reacts to navigation: there is no message for
        // it. Only Dismiss and a newer incident remove or replace it.
        let (first, _) = two_incidents();
        let mut slot = Slot::default();
        slot.report(first, Operation::Copy, vec![failure("a")]);
        update(&mut slot, Message::ToggleDetails);
        update(&mut slot, Message::ToggleDetails);
        assert!(slot.is_showing());
        update(&mut slot, Message::Dismiss);
        assert!(!slot.is_showing());
    }

    #[test]
    fn a_replacement_starts_collapsed_and_joining_keeps_details_open() {
        let (first, second) = two_incidents();
        let mut slot = Slot::default();
        slot.report(first, Operation::Copy, vec![failure("a"), failure("b")]);
        update(&mut slot, Message::ToggleDetails);
        slot.report(first, Operation::Copy, vec![failure("c")]);
        assert!(slot.notice().is_some_and(Notice::is_expanded));
        slot.report(second, Operation::Move, vec![failure("d")]);
        assert!(slot.notice().is_some_and(|n| !n.is_expanded()));
    }

    #[test]
    fn toggle_on_nothing_is_harmless() {
        let mut slot = Slot::default();
        update(&mut slot, Message::ToggleDetails);
        assert!(!slot.is_showing());
    }

    #[test]
    fn a_huge_report_is_counted_but_only_the_first_few_are_kept() {
        let (first, _) = two_incidents();
        let mut slot = Slot::default();
        let many: Vec<Failure> = (0..100_000).map(|i| failure(&i.to_string())).collect();
        slot.report(first, Operation::Delete, many);
        // More arrive in a second batch for the same incident.
        slot.report(first, Operation::Delete, vec![failure("x"), failure("y")]);
        let notice = slot.notice().expect("a notice");
        assert_eq!(notice.total(), 100_002);
        assert_eq!(notice.failures().len(), MAX_DETAIL_LINES);
        assert_eq!(notice.summary().details.len(), MAX_DETAIL_LINES);
        assert_eq!(
            notice.summary().headline,
            "100002 items couldn't be deleted"
        );
    }

    #[test]
    fn a_slow_action_started_earlier_does_not_replace_a_newer_notice() {
        // The ordering rule end to end: the incident is minted at the start
        // of each action, so the earlier-started one is older whichever
        // failure arrives first.
        let (started_first, started_second) = two_incidents();
        let mut slot = Slot::default();
        slot.report(started_second, Operation::Open, vec![failure("fast")]);
        assert!(!slot.report(started_first, Operation::Create, vec![failure("slow")]));
        assert_eq!(slot.notice().map(Notice::incident), Some(started_second));
    }

    #[test]
    fn the_view_builds_for_every_state() {
        // Smoke test: building the widget tree must not panic in any state
        // (single, collapsed group, expanded group, more than the cap).
        let theme = Theme::saola();
        let (first, _) = two_incidents();
        for count in [1, 3, MAX_DETAIL_LINES + 3] {
            for expanded in [false, true] {
                let mut slot = Slot::default();
                let failures = (0..count).map(|i| failure(&format!("f{i}"))).collect();
                slot.report(first, Operation::Copy, failures);
                if expanded {
                    slot.toggle_details();
                }
                if let Some(notice) = slot.notice() {
                    let _element = view(&theme, notice);
                }
            }
        }
    }
}
