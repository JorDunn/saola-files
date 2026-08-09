//! The explorer toolbar: back/forward/up navigation, the breadcrumb (or
//! editable path) bar, the list/grid switcher, the hidden-files toggle, a
//! refresh button (only when the backend can't signal changes itself),
//! and an overflow menu slot.
//!
//! Sized at `sizes.window_header` (46px) — the same token `ui::window`'s
//! own title bar uses, deliberately reused (not a new hardcoded literal)
//! so this toolbar reads at the same rhythm as the window chrome above it.
//!
//! Every control here emits a [`crate::ui::dirview::Message`] directly —
//! there is no separate `header::Message`. Most controls send
//! `Message::Action(keymap::Action)`: a click on the Back button does
//! *exactly* what Alt+Left does, so both input paths funnel through the
//! same `Action` vocabulary and the same `DirectoryView::apply_action`
//! match arm (CLAUDE.md's messages rule — keyboard resolves through
//! `keymap::Action` — extended here to "mouse-driven equivalents of a
//! keyboard action reuse that same `Action`", not a second vocabulary).

use iced::widget::{button, container, row, text};
use iced::{Center, Element, Fill};
use saola_theme::{Surface, Theme, convert, style};

use crate::config::View;
use crate::core::vfs::Caps;
use crate::keymap::Action;
use crate::ui::breadcrumbs;
use crate::ui::dirview::{self, DirectoryView};

pub fn view<'a>(t: &'a Theme, state: &'a DirectoryView) -> Element<'a, dirview::Message> {
    let mut items: Vec<Element<'a, dirview::Message>> = vec![
        nav_button(t, "←", state.can_go_back(), Action::HistoryBack),
        nav_button(t, "→", state.can_go_forward(), Action::HistoryForward),
        // Ascend is always wired: at the filesystem root `location.parent()`
        // is `None`, so `apply_action` already degrades that to "no event"
        // rather than needing this button to know the root in advance.
        nav_button(t, "↑", true, Action::Ascend),
        breadcrumbs::view(t, state),
        view_switcher(t, state.view_mode()),
        hidden_toggle(t, state.show_hidden()),
    ];

    // Capability-honest UI (CLAUDE.md): a backend that can already signal
    // changes (`Caps::WATCH`) doesn't get a manual refresh button — one
    // would just be redundant chrome, not a real affordance.
    if !state.caps().contains(Caps::WATCH) {
        items.push(nav_button(t, "⟳", true, Action::Refresh));
    }

    items.push(overflow_button(t));

    let content = row(items)
        .spacing(t.sizes.pill_gap)
        .align_y(Center)
        .width(Fill);

    container(content)
        .height(t.sizes.window_header)
        .padding([0.0, t.sizes.pill_gap])
        .align_y(Center)
        .into()
}

/// A bare-glyph nav button. `enabled = false` deliberately omits
/// `.on_press` rather than attaching a no-op — the iced 0.14 gotcha
/// (CLAUDE.md): a button without `.on_press` renders `Disabled` and
/// doesn't capture its press, which is exactly "there's nowhere to go"
/// for Back/Forward at the ends of the history stacks.
fn nav_button<'a>(
    t: &'a Theme,
    glyph: &'static str,
    enabled: bool,
    action: Action,
) -> Element<'a, dirview::Message> {
    let content = text(glyph)
        .size(t.typography.size.body)
        .font(convert::ui_font(t));
    let mut b = button(content)
        .style(style::button::bare(t, Surface::Paper))
        .padding([6.0, 12.0]);
    if enabled {
        b = b.on_press(dirview::Message::Action(action));
    }
    b.into()
}

/// The list/grid segmented switcher, built from `style::segmented`
/// exactly the way that module's own docs describe: a track container
/// plus one button per option, each styled `segment(t, s, is_selected)`.
fn view_switcher<'a>(t: &'a Theme, mode: View) -> Element<'a, dirview::Message> {
    let segment = |label: &'static str, target: View, action: Action| {
        button(
            text(label)
                .size(t.typography.size.label)
                .font(convert::ui_font(t)),
        )
        .style(style::segmented::segment(t, Surface::Paper, mode == target))
        .padding([6.0, 14.0])
        .on_press(dirview::Message::Action(action))
    };

    container(row![
        segment("List", View::List, Action::SetViewList),
        segment("Grid", View::Grid, Action::SetViewGrid),
    ])
    .style(style::segmented::track(t, Surface::Paper))
    .padding(2.0)
    .into()
}

/// The hidden-files toggle: an ordinary pill that's `active` (terracotta)
/// when dotfiles are showing, `rest` otherwise — the same on/off language
/// as the segmented switcher above, just a single control instead of two.
fn hidden_toggle<'a>(t: &'a Theme, active: bool) -> Element<'a, dirview::Message> {
    let content = text("Hidden")
        .size(t.typography.size.label)
        .font(convert::ui_font(t));
    let styled = if active {
        button(content).style(style::button::active(t, Surface::Paper))
    } else {
        button(content).style(style::button::rest(t, Surface::Paper))
    };
    styled
        .padding([6.0, 14.0])
        .on_press(dirview::Message::Action(Action::ToggleHidden))
        .into()
}

/// The overflow menu's chrome slot. Stage 6 wires the real menu (cut/
/// copy/paste/new/properties/…); this stage only reserves the space, so
/// it's deliberately `.on_press`-less — same "renders `Disabled`, doesn't
/// capture its press" gotcha as `nav_button`'s disabled state, here used
/// because there is genuinely nothing to open yet, not as a workaround.
fn overflow_button<'a>(t: &'a Theme) -> Element<'a, dirview::Message> {
    button(
        text("⋯")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::bare(t, Surface::Paper))
    .padding([6.0, 12.0])
    .into()
}
