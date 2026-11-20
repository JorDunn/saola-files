//! The error notice: how a failed operation shows up in the window.
//!
//! A copy that hits a read-only folder, a file with no app to open it, a
//! New Folder that can't be created: each used to print a line to stderr
//! and nothing else. This notice puts the sentence
//! (`core::failure::describe`) in the window, and keeps it there until the
//! person dismisses it (the close button, or Esc via `App`).
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
//! # Several failures
//!
//! Failures reported for the same [`Operation`] while a notice is already
//! showing join it ("3 items couldn't be trashed" grows from two to three),
//! so one multi-item delete is one notice however many tasks reported in.
//! A failure of a different operation replaces the notice; the replaced
//! one is already on stderr and was, by definition, not dismissed because
//! the person was busy with something else.
//!
//! # Severity is wording
//!
//! No danger colour exists in the style guide. The strip is the same
//! `footer_strip` the progress strip and undo toast use, with an
//! `Icon::Info` glyph; the sentence says what is wrong.
//!
//! State lives here and is mutated by [`update`]; `App` owns the
//! `Option<Notice>` and the Esc binding. Like the other dialogs this module
//! holds no clock and ticks nothing.

use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, text};
use iced::{Center, Element, Fill};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style, widget};

use crate::core::failure::{self, Failure, Operation, Summary};
use crate::tr;

/// Detail lines drawn before "…and N more". The strip has no scrolling
/// (and nothing in this app builds a list it can't bound), so a notice for
/// a thousand failed items shows the first few and counts the rest.
const MAX_DETAIL_LINES: usize = 8;

#[derive(Debug, Clone)]
pub enum Message {
    /// The close button (and Esc, which `App` turns into this).
    Dismiss,
    /// The Details / Hide details button.
    ToggleDetails,
    /// Something failed: show it. This is the message other code sends;
    /// `App` has a helper that builds it.
    Report(Operation, Vec<Failure>),
}

/// What the notice is currently telling the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    operation: Operation,
    failures: Vec<Failure>,
    expanded: bool,
}

impl Notice {
    /// Adds `failures` of `operation` to `slot`: joins the notice already
    /// showing for the same operation, otherwise replaces it. An empty list
    /// changes nothing.
    pub fn report(slot: &mut Option<Notice>, operation: Operation, failures: Vec<Failure>) {
        if failures.is_empty() {
            return;
        }
        match slot {
            Some(notice) if notice.operation == operation => notice.failures.extend(failures),
            _ => {
                *slot = Some(Notice {
                    operation,
                    failures,
                    expanded: false,
                });
            }
        }
    }

    pub fn operation(&self) -> Operation {
        self.operation
    }

    pub fn failures(&self) -> &[Failure] {
        &self.failures
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// The sentence(s) to draw. `None` only for a notice with no failures,
    /// which [`Notice::report`] never creates.
    pub fn summary(&self) -> Option<Summary> {
        failure::summarize(self.operation, &self.failures)
    }
}

/// Applies `message` to the notice slot.
pub fn update(slot: &mut Option<Notice>, message: Message) {
    match message {
        Message::Dismiss => *slot = None,
        Message::ToggleDetails => {
            if let Some(notice) = slot {
                notice.expanded = !notice.expanded;
            }
        }
        Message::Report(operation, failures) => Notice::report(slot, operation, failures),
    }
}

/// Renders the notice: an optional Details card above a `footer_strip`
/// holding the glyph, the headline, a Details toggle (only when more than
/// one thing failed) and the close button.
pub fn view<'a>(t: &'a Theme, notice: &Notice) -> Element<'a, Message> {
    let Some(summary) = notice.summary() else {
        return iced::widget::Space::new().into();
    };
    let has_details = !summary.details.is_empty();

    let glyph = icon::icon(Icon::Info, t.sizes.icon_row, t.on_paper.primary.into_iced());

    // `Wrapping::None` keeps a long filename on the one line the strip has
    // room for; the container clips it rather than growing the strip.
    let headline = text(summary.headline)
        .size(t.typography.size.secondary)
        .font(convert::ui_font(t))
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
            button(
                text(label)
                    .size(t.typography.size.secondary)
                    .font(convert::ui_font(t)),
            )
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
        let total = summary.details.len();
        let mut lines: Vec<Element<'a, Message>> = summary
            .details
            .into_iter()
            .take(MAX_DETAIL_LINES)
            .map(|line| {
                text(line)
                    .size(t.typography.size.secondary)
                    .font(convert::ui_font(t))
                    .wrapping(Wrapping::None)
                    .into()
            })
            .collect();
        if total > MAX_DETAIL_LINES {
            lines.push(
                text(tr!("error-notice-more", count = total - MAX_DETAIL_LINES))
                    .size(t.typography.size.secondary)
                    .font(convert::ui_font(t))
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

    #[test]
    fn a_report_into_an_empty_slot_shows_it() {
        let mut slot = None;
        Notice::report(&mut slot, Operation::Copy, vec![failure("a")]);
        let notice = slot.expect("a notice");
        assert_eq!(notice.operation(), Operation::Copy);
        assert_eq!(notice.failures().len(), 1);
        assert!(!notice.is_expanded());
    }

    #[test]
    fn an_empty_report_changes_nothing() {
        let mut slot = None;
        Notice::report(&mut slot, Operation::Copy, Vec::new());
        assert!(slot.is_none());
        Notice::report(&mut slot, Operation::Copy, vec![failure("a")]);
        let before = slot.clone();
        Notice::report(&mut slot, Operation::Move, Vec::new());
        assert_eq!(slot, before);
    }

    #[test]
    fn failures_of_the_same_operation_join_one_notice() {
        let mut slot = None;
        for name in ["a", "b", "c"] {
            Notice::report(&mut slot, Operation::Trash, vec![failure(name)]);
        }
        let notice = slot.expect("a notice");
        assert_eq!(notice.failures().len(), 3);
        let summary = notice.summary().expect("a summary");
        assert_eq!(summary.headline, "3 items couldn't be moved to the trash");
        assert_eq!(summary.details.len(), 3);
    }

    #[test]
    fn a_different_operation_replaces_the_notice() {
        let mut slot = None;
        Notice::report(&mut slot, Operation::Copy, vec![failure("a"), failure("b")]);
        Notice::report(&mut slot, Operation::Open, vec![failure("c")]);
        let notice = slot.expect("a notice");
        assert_eq!(notice.operation(), Operation::Open);
        assert_eq!(notice.failures().len(), 1);
    }

    #[test]
    fn a_replacement_starts_collapsed() {
        let mut slot = None;
        update(
            &mut slot,
            Message::Report(Operation::Copy, vec![failure("a"), failure("b")]),
        );
        update(&mut slot, Message::ToggleDetails);
        assert!(slot.as_ref().is_some_and(Notice::is_expanded));
        update(
            &mut slot,
            Message::Report(Operation::Move, vec![failure("c")]),
        );
        assert!(slot.as_ref().is_some_and(|n| !n.is_expanded()));
    }

    #[test]
    fn joining_keeps_the_details_open() {
        let mut slot = None;
        update(
            &mut slot,
            Message::Report(Operation::Copy, vec![failure("a"), failure("b")]),
        );
        update(&mut slot, Message::ToggleDetails);
        update(
            &mut slot,
            Message::Report(Operation::Copy, vec![failure("c")]),
        );
        assert!(slot.as_ref().is_some_and(Notice::is_expanded));
    }

    #[test]
    fn dismiss_clears_and_toggle_on_nothing_is_harmless() {
        let mut slot = None;
        update(&mut slot, Message::ToggleDetails);
        assert!(slot.is_none());
        update(
            &mut slot,
            Message::Report(Operation::Copy, vec![failure("a")]),
        );
        update(&mut slot, Message::Dismiss);
        assert!(slot.is_none());
    }

    #[test]
    fn the_view_builds_for_every_state() {
        // Smoke test: building the widget tree must not panic in any state
        // (single, collapsed group, expanded group, more than the cap).
        let theme = Theme::saola();
        let many: Vec<Failure> = (0..MAX_DETAIL_LINES + 3)
            .map(|i| failure(&format!("f{i}")))
            .collect();
        for failures in [vec![failure("a")], many.clone()] {
            for expanded in [false, true] {
                let mut slot = None;
                Notice::report(&mut slot, Operation::Copy, failures.clone());
                if expanded {
                    update(&mut slot, Message::ToggleDetails);
                }
                if let Some(notice) = &slot {
                    let _element = view(&theme, notice);
                }
            }
        }
    }
}
