//! The undo toast (Stage 10) — PLAN.md's "an undo toast in the ops strip
//! (sanctioned notification animation only)". Lives in the exact same
//! footer band `ui::dialogs::progress`'s strip occupies (`main.rs::App::view`
//! shows at most one of the two at a time: the progress strip while
//! `active_op_progress` is `Some`, this toast otherwise while
//! `App::undo_toast` is) — one persistent bar at the bottom of the window,
//! never a floating popover, matching this app's "no popovers outside
//! menus" posture.
//!
//! **The animation is the one CLAUDE.md names "notification"** — the same
//! `motion.toast_in`/`toast_idle`/`toast_out` three-phase fade
//! `saola-capture::modules::toast` already established as the sanctioned
//! shape (see that module's own doc comment for the full account this one
//! deliberately doesn't repeat). This toast is simpler than capture's: no
//! hover-pause, no stacking (there is only ever one undo-able "most recent
//! op" at a time — a second push while one toast is showing just replaces
//! it, the same way a second `Started` op replaces `active_op_progress`),
//! and no slide (the strip is a fixed-position bar, not a floating card
//! with an off-screen edge to slide in from) — only the fade-alpha half of
//! capture's `phase` function is reused here, not the leading-spacer slide.
//!
//! **Time is injected, never read inside this module** (the same teaching
//! note `saola-capture::modules::toast`/`saola-lockscreen::modules::reveal`
//! give): [`Toast::alpha`]/[`Toast::expired`] both take `now` as a plain
//! parameter. `main.rs` reads the real clock at exactly two points: once
//! when a push seeds `Toast::shown_at`, and once per `Message::Tick`
//! (fed by [`subscription`], gated to only run while a toast is actually
//! showing — CLAUDE.md's "nothing ticks without a documented exception",
//! and this is that exception, same as capture's own toast tick).

use std::time::{Duration, Instant};

use iced::widget::{button, container, progress_bar, row, text};
use iced::{Center, Element, Fill, Subscription};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, motion, style};

/// How often the strip re-renders while a toast is up — coarse enough to
/// cost nothing (this is a fade, not `saola-capture::modules::toast`'s
/// life-rule countdown, which needs its own finer 32 ms per that module's
/// own comment), fine enough that the fade still reads as smooth motion
/// rather than a series of visible steps.
const TICK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy)]
pub enum Message {
    /// One [`TICK`] elapsed — `main.rs` re-checks [`Toast::expired`] and
    /// drops `App::undo_toast` once it is.
    Tick,
    /// The toast's own "Undo" button — the mouse equivalent of Ctrl+Z,
    /// same "one behavior, two input paths" posture every other keymap
    /// `Action` already has a mouse equivalent through
    /// (`ui::dirview::Message::MenuCopyRequested` and friends).
    UndoClicked,
}

/// One undo toast's state: a frozen label (built once at push time from
/// `core::fs::undo::UndoEntry::label` — this module never sees an
/// `UndoEntry` itself, matching `ui::dialogs::progress`'s own "translate
/// once in `App`, render the translation" split) and when it was shown.
#[derive(Debug, Clone)]
pub struct Toast {
    label: String,
    shown_at: Instant,
}

impl Toast {
    pub fn new(label: String, now: Instant) -> Self {
        Toast {
            label,
            shown_at: now,
        }
    }

    /// True once the toast has fully lived out `motion.toast_total` —
    /// `main.rs`'s `Message::Tick` handling clears `App::undo_toast` the
    /// moment this flips.
    pub fn expired(&self, theme: &Theme, now: Instant) -> bool {
        let total = Duration::from_millis(theme.motion.toast_total.into());
        now.saturating_duration_since(self.shown_at) >= total
    }
}

/// Ticks only while `has_toast` — the same gated-subscription shape
/// `saola-capture::modules::toast::ToastStack::subscription` uses, and for
/// the same reason (CLAUDE.md: nothing ticks without a documented
/// exception; this is the one this module gets to claim).
pub fn subscription(has_toast: bool) -> Subscription<Message> {
    if has_toast {
        iced::time::every(TICK).map(|_instant| Message::Tick)
    } else {
        Subscription::none()
    }
}

/// Renders the toast: the §6 notification card kit (a `notification_card`
/// backdrop, a leading `icon_tile`, and a bottom-edge `life_rule` countdown)
/// around a rotate-ccw icon, the frozen label, and an "Undo" button —
/// `App::view` only calls this while `App::undo_toast` is `Some`, mirroring
/// `ui::dialogs::progress::view`'s own "no empty rendering to keep honest
/// here, the caller decides" posture.
///
/// Stage 12: promoted from a hand-rolled `container::card` + manual
/// `scale_alpha` closure to `style::container::notification_card` +
/// `style::notification::{life_rule, icon_tile}` +
/// `motion::{toast_alpha, life_fraction}` — the exact upstream recipe those
/// helpers' own doc comments describe as "saola-capture's toast derived
/// exactly this card locally". `notification_card` is ink-only (a toast is
/// shell-layer chrome per the style guide, regardless of the paper window
/// it floats over), so this toast now reads as an ink card rather than a
/// paper one — a deliberate part of adopting the shared recipe, not a bug.
pub fn view<'a>(t: &'a Theme, toast: &'a Toast, now: Instant) -> Element<'a, Message> {
    let elapsed = now.saturating_duration_since(toast.shown_at);
    let alpha = motion::toast_alpha(t, elapsed);
    let life = motion::life_fraction(t, elapsed);

    let icon_color = t.on_ink.primary.with_opacity(alpha);
    let text_color = t.on_ink.primary.with_opacity(alpha);
    let accent_text = t.palette.accent.with_opacity(alpha);

    let icon_tile = container(icon::icon(Icon::RotateCcw, t.sizes.icon_row, icon_color))
        .style(style::notification::icon_tile(t))
        .width(t.sizes.icon_tile)
        .height(t.sizes.icon_tile)
        .align_x(Center)
        .align_y(Center);

    let label = text(toast.label.clone())
        .size(t.typography.size.secondary)
        .font(convert::ui_font(t))
        .color(text_color);

    let undo = button(
        text("Undo")
            .size(t.typography.size.secondary)
            .font(convert::ui_font(t))
            .color(accent_text),
    )
    .style(style::button::bare(t, Surface::Ink))
    .padding(t.paddings.strip)
    .on_press(Message::UndoClicked);

    let body = row![
        icon_tile,
        label,
        container(undo)
            .width(Fill)
            .align_x(iced::alignment::Horizontal::Right)
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center)
    .width(Fill);

    let life_rule = progress_bar(0.0..=1.0, life)
        .length(Fill)
        .girth(t.sizes.life_rule)
        .style(style::notification::life_rule(t));

    container(
        iced::widget::column![body, life_rule]
            .spacing(t.sizes.pill_gap / 2.0)
            .width(Fill),
    )
    .style(style::container::notification_card(t, alpha))
    .width(Fill)
    .height(t.sizes.ops_strip)
    .padding([t.sizes.pill_gap / 2.0, t.sizes.popover_padding / 2.0])
    // Centred for the same reason `ui::dialogs::progress`'s strip is —
    // the two occupy the identical footer band and must read as one
    // continuous piece of chrome when one replaces the other.
    .align_y(Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::saola()
    }

    #[test]
    fn a_fresh_toast_is_not_expired() {
        let now = Instant::now();
        let toast = Toast::new("Moved \"a.txt\"".to_owned(), now);
        assert!(!toast.expired(&theme(), now));
    }

    #[test]
    fn a_toast_expires_once_its_total_lifetime_has_passed() {
        let theme = theme();
        let now = Instant::now();
        let toast = Toast::new("Moved \"a.txt\"".to_owned(), now);
        let total = Duration::from_millis(theme.motion.toast_total.into());
        assert!(toast.expired(&theme, now + total));
        assert!(!toast.expired(&theme, now + total / 2));
    }

    // The fade-envelope math itself (`alpha_for`/`fraction`) moved upstream
    // to `saola_theme::motion::toast_alpha`/`fraction` in Stage 12 — that
    // crate's own `motion` test module covers the three-phase envelope now;
    // this module has no local math left to re-test.

    #[test]
    fn subscription_is_none_without_a_toast() {
        // `Subscription` has no public introspection, so this only proves
        // the gate doesn't panic and both branches are reachable — the
        // meaningful behavior (no `Tick` messages arrive) is exercised at
        // the `App::subscription` integration level, not unit-testable
        // here in isolation.
        let _ = subscription(false);
        let _ = subscription(true);
    }
}
