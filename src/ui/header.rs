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
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::config::View;
use crate::core::vfs::Caps;
use crate::icons::{self, Icon};
use crate::keymap::Action;
use crate::ui::breadcrumbs;
use crate::ui::dirview::{self, DirectoryView};

pub fn view<'a>(t: &'a Theme, state: &'a DirectoryView) -> Element<'a, dirview::Message> {
    let mut items: Vec<Element<'a, dirview::Message>> = vec![
        nav_button(t, Icon::ArrowLeft, state.can_go_back(), Action::HistoryBack),
        nav_button(
            t,
            Icon::ArrowRight,
            state.can_go_forward(),
            Action::HistoryForward,
        ),
        // Ascend is always wired: at the filesystem root `location.parent()`
        // is `None`, so `apply_action` already degrades that to "no event"
        // rather than needing this button to know the root in advance.
        nav_button(t, Icon::ArrowUp, true, Action::Ascend),
        breadcrumbs::view(t, state),
        view_switcher(t, state.view_mode()),
        hidden_toggle(t, state.show_hidden()),
    ];

    // Capability-honest UI (CLAUDE.md): a backend that can already signal
    // changes (`Caps::WATCH`) doesn't get a manual refresh button — one
    // would just be redundant chrome, not a real affordance.
    if !state.caps().contains(Caps::WATCH) {
        items.push(nav_button(t, Icon::RefreshCw, true, Action::Refresh));
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

/// A bare-icon nav button. `enabled = false` deliberately omits
/// `.on_press` rather than attaching a no-op — the iced 0.14 gotcha
/// (CLAUDE.md): a button without `.on_press` renders `Disabled` and
/// doesn't capture its press, which is exactly "there's nowhere to go"
/// for Back/Forward at the ends of the history stacks. `style::button::
/// bare`'s own `Status::Disabled` arm dims *label text*, but an `Svg`
/// icon's tint is a fixed color baked in at build time (`icons::icon`'s
/// `.style()` closure never reads `button::Status`) — so this function
/// picks the dimmed `on_paper.disabled` tint itself when `!enabled`,
/// rather than relying on the button style to do it.
fn nav_button<'a>(
    t: &'a Theme,
    glyph: Icon,
    enabled: bool,
    action: Action,
) -> Element<'a, dirview::Message> {
    let color = if enabled {
        t.on_paper.primary
    } else {
        t.on_paper.disabled
    };
    let content = icons::icon(glyph, t.sizes.icon_row, color.into_iced());
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
/// Icon tint follows `segment`'s own selected/unselected label color
/// (ivory-on-terracotta selected, ink-on-fill otherwise) — same reasoning
/// as `nav_button`'s doc comment: an `Svg` icon's tint is fixed at build
/// time, so it can't ride the button style's own per-`Status` text color.
fn view_switcher<'a>(t: &'a Theme, mode: View) -> Element<'a, dirview::Message> {
    let segment = |glyph: Icon, label: &'static str, target: View, action: Action| {
        let selected = mode == target;
        let color = if selected {
            t.palette.paper
        } else {
            t.on_paper.primary
        };
        let content = row![
            icons::icon(glyph, t.sizes.icon_row, color.into_iced()),
            text(label)
                .size(t.typography.size.label)
                .font(convert::ui_font(t)),
        ]
        .spacing(4.0)
        .align_y(Center);
        button(content)
            .style(style::segmented::segment(t, Surface::Paper, selected))
            .padding([6.0, 14.0])
            .on_press(dirview::Message::Action(action))
    };

    container(row![
        segment(Icon::List, "List", View::List, Action::SetViewList),
        segment(Icon::LayoutGrid, "Grid", View::Grid, Action::SetViewGrid),
    ])
    .style(style::segmented::track(t, Surface::Paper))
    .padding(2.0)
    .into()
}

/// The hidden-files toggle: an ordinary pill that's `active` (terracotta)
/// when dotfiles are showing, `rest` otherwise — the same on/off language
/// as the segmented switcher above, just a single control instead of two.
/// The glyph itself tracks the same state the label does: an open eye
/// once hidden files are showing, a slashed eye while they're hidden.
fn hidden_toggle<'a>(t: &'a Theme, active: bool) -> Element<'a, dirview::Message> {
    let glyph = if active { Icon::Eye } else { Icon::EyeOff };
    let color = if active {
        t.palette.paper
    } else {
        t.on_paper.primary
    };
    let content = row![
        icons::icon(glyph, t.sizes.icon_row, color.into_iced()),
        text("Hidden")
            .size(t.typography.size.label)
            .font(convert::ui_font(t)),
    ]
    .spacing(4.0)
    .align_y(Center);
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

/// The overflow menu trigger: opens `ui::menus`'s context menu
/// (`Message::OpenMenu`) — Stage 6's real wiring of the chrome slot Stage 4
/// reserved.
fn overflow_button<'a>(t: &'a Theme) -> Element<'a, dirview::Message> {
    let content = icons::icon(
        Icon::Ellipsis,
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    );
    button(content)
        .style(style::button::bare(t, Surface::Paper))
        .padding([6.0, 12.0])
        .on_press(dirview::Message::OpenMenu)
        .into()
}
