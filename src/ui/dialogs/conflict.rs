//! The conflict-resolution dialog: a modal shown whenever `core::fs::ops`
//! hits a destination that already exists. Severity by wording only, per
//! CLAUDE.md's design language — no danger/warning color, no fourth hue;
//! this is an ordinary `container::card` at the dialog radius, the same
//! chrome any other Saola confirmation surface uses.
//!
//! `main.rs::App` owns the actual state (which `Conflict` is pending, the
//! reply `mpsc::Sender`, and the "Apply to all" checkbox) — this module
//! only ever renders it and emits [`Message`]; the reply send itself
//! happens in `App::update`, the one place that also owns the channel.

use iced::widget::{button, checkbox, column, container, row, text};
use iced::{Center, Element, Fill, Length};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::ops::{Conflict, ConflictChoice};
use crate::icons::{self, Icon};

/// Dialog card width. Layout-specific, not a saola-theme size — same
/// "local constant, documented style debt" posture `ui::sidebar::
/// SIDEBAR_WIDTH`/`ui::dialogs::progress::STRIP_HEIGHT` already take.
const DIALOG_WIDTH: f32 = 380.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// One of the three resolution buttons — clicking any of them submits
    /// immediately (there is no separate "OK" step); `apply_to_all`'s
    /// current checkbox state travels with it into
    /// `core::fs::ops::ConflictDecision`.
    ChoiceSelected(ConflictChoice),
    ApplyToAllToggled(bool),
}

/// Renders the modal: title naming the colliding entry, the two full
/// locations for disambiguation, three resolution buttons, and the "Apply
/// to all" checkbox. `apply_to_all` is `App`'s current checkbox state, fed
/// back in at render time (this module holds no state of its own).
pub fn view<'a>(t: &'a Theme, conflict: &'a Conflict, apply_to_all: bool) -> Element<'a, Message> {
    let dest_name = conflict
        .dest
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| conflict.dest.to_string());

    let title = text(format!("\"{dest_name}\" already exists"))
        .size(t.typography.size.dialog_title)
        .font(convert::display_font(t))
        .color(t.on_paper.primary.into_iced());

    let body = text(format!(
        "{} is already there. What should happen to {}?",
        conflict.dest, conflict.source
    ))
    .size(t.typography.size.secondary)
    .font(convert::ui_font_regular(t))
    .color(t.on_paper.secondary.into_iced());

    let buttons = column![
        choice_button(t, "Keep both", Icon::Copy, ConflictChoice::RenameCopy),
        choice_button(
            t,
            "Overwrite",
            Icon::ClipboardPaste,
            ConflictChoice::Overwrite
        ),
        choice_button(t, "Skip", Icon::X, ConflictChoice::Skip),
    ]
    .spacing(t.sizes.pill_gap / 2.0);

    let apply_all = checkbox(apply_to_all)
        .label("Apply to all conflicts in this operation")
        .on_toggle(Message::ApplyToAllToggled)
        .style(style::toggles::checkbox(t, Surface::Paper))
        .text_size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t));

    let content = column![title, body, buttons, apply_all]
        .spacing(t.sizes.popover_padding / 2.0)
        .width(Length::Fixed(DIALOG_WIDTH));

    container(content)
        .style(style::container::card(t, Surface::Paper))
        .padding(t.sizes.popover_padding)
        .into()
}

fn choice_button<'a>(
    t: &'a Theme,
    label: &'a str,
    glyph: Icon,
    choice: ConflictChoice,
) -> Element<'a, Message> {
    let content = row![
        icons::icon(glyph, t.sizes.icon_row, t.on_paper.primary.into_iced()),
        text(label)
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);

    button(content)
        .style(style::button::rest(t, Surface::Paper))
        .padding([10.0, 16.0])
        .width(Fill)
        .on_press(Message::ChoiceSelected(choice))
        .into()
}
