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
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style, widget};

use crate::config::View;
use crate::core::vfs::Caps;
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

    // The toolbar is the second of the window's two chrome regions (the
    // places sidebar is the first) and takes the same recessed ground:
    // `style::container::inset` at `on_paper.fill_subtle`, `radii.inset` —
    // Stage 12's upstreamed promotion of `container::tile`-at-`radii.inset`
    // (see `ui::sidebar::Sidebar::view`'s doc comment for the full
    // rationale). Sitting the navigation controls on a step of ink — rather
    // than on the same paper the file listing uses — is what stops the
    // toolbar, the column headers and the rows below them from reading as
    // one undifferentiated sheet. Its inset from the window edge and from
    // the listing below comes from `ui::explorer` (`sizes.island_gap`), not
    // from here.
    container(content)
        .style(style::container::inset(t, Surface::Paper))
        .width(Fill)
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
/// icon's tint is a fixed color baked in at build time — so this function
/// picks the dimmed `on_paper.disabled` tint itself when `!enabled`,
/// exactly the tint `widget::icon_button`'s own doc comment says the caller
/// (not the button style) must choose.
fn nav_button<'a>(
    t: &'a Theme,
    glyph: Icon,
    enabled: bool,
    action: Action,
) -> Element<'a, dirview::Message> {
    let tint = if enabled {
        widget::role(t, Surface::Paper, widget::Emphasis::Rest)
    } else {
        widget::role(t, Surface::Paper, widget::Emphasis::Disabled)
    };
    widget::icon_button(
        t,
        Surface::Paper,
        glyph,
        None,
        tint,
        enabled.then_some(dirview::Message::Action(action)),
    )
}

/// The list/grid segmented switcher — Stage 12: `widget::segmented_row`, the
/// upstreamed promotion of this function's own hand-rolled track+segment
/// assembly (ported from saola-capture per that constructor's own doc
/// comment). `segmented_row` is label-only (no icon slot), so the previous
/// List/Grid glyphs are dropped here — a deliberate part of adopting the
/// shared control, not an oversight; see the Stage 12 handoff's "expect
/// visual diffs" note.
fn view_switcher<'a>(t: &'a Theme, mode: View) -> Element<'a, dirview::Message> {
    widget::segmented_row(
        t,
        Surface::Paper,
        &[(View::List, "List"), (View::Grid, "Grid")],
        &mode,
        |target| {
            dirview::Message::Action(match target {
                View::List => Action::SetViewList,
                View::Grid => Action::SetViewGrid,
            })
        },
    )
}

/// The hidden-files toggle: an ordinary pill that's `active` (terracotta)
/// when dotfiles are showing, `rest` otherwise — the same on/off language
/// as the segmented switcher above, just a single control instead of two.
/// The glyph itself tracks the same state the label does: an open eye
/// once hidden files are showing, a slashed eye while they're hidden.
/// `style::button::emphasis` (Stage 12) picks between the two recipes
/// behind one closure type — the exact constraint that helper's own doc
/// comment names this call site as an example of.
fn hidden_toggle<'a>(t: &'a Theme, active: bool) -> Element<'a, dirview::Message> {
    let glyph = if active { Icon::Eye } else { Icon::EyeOff };
    let color = if active {
        t.palette.paper
    } else {
        t.on_paper.primary
    };
    let content = row![
        icon::icon(glyph, t.sizes.icon_row, color.into_iced()),
        text("Hidden")
            .size(t.typography.size.label)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.gap_tight)
    .align_y(Center);
    button(content)
        .style(style::button::emphasis(t, Surface::Paper, active))
        .padding(t.paddings.pill_button)
        .on_press(dirview::Message::Action(Action::ToggleHidden))
        .into()
}

/// The overflow menu trigger: opens `ui::menus`'s context menu
/// (`Message::OpenMenu`) — Stage 6's real wiring of the chrome slot Stage 4
/// reserved.
fn overflow_button<'a>(t: &'a Theme) -> Element<'a, dirview::Message> {
    widget::icon_button(
        t,
        Surface::Paper,
        Icon::Ellipsis,
        None,
        t.on_paper.primary.into_iced(),
        Some(dirview::Message::OpenMenu),
    )
}
