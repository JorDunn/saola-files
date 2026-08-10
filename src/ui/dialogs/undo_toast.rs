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

use iced::widget::{button, container, row, text};
use iced::{Center, Element, Fill, Length, Subscription};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::icons::{self, Icon};

/// Height of the toast strip — deliberately the same literal
/// `ui::dialogs::progress::STRIP_HEIGHT` uses (that constant is private to
/// its own module, so this is a second copy, not a shared one): the two
/// strips occupy the identical footer position and must read as one
/// continuous piece of chrome, never a visible height jump when one
/// replaces the other. TODO(saola-theme): promote to a shared
/// `sizes.ops_strip` token — `ui::dialogs::progress`'s own TODO comment
/// already flags this gap; a second consumer here is exactly the "second
/// call site" that comment says would justify the tag bump.
const STRIP_HEIGHT: f32 = 56.0;

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

    fn alpha(&self, theme: &Theme, now: Instant) -> f32 {
        alpha_for(theme, now.saturating_duration_since(self.shown_at))
    }
}

/// The fade-alpha half of `saola-capture::modules::toast::phase` — in over
/// `toast_in`, steady over `toast_idle`, out over `toast_out`. No offset
/// component here (see the module doc comment on why this toast never
/// slides).
fn alpha_for(theme: &Theme, elapsed: Duration) -> f32 {
    let in_dur = Duration::from_millis(theme.motion.toast_in.into());
    let idle_dur = Duration::from_millis(theme.motion.toast_idle.into());
    let out_dur = Duration::from_millis(theme.motion.toast_out.into());

    if elapsed < in_dur {
        fraction(elapsed, in_dur)
    } else if elapsed < in_dur + idle_dur {
        1.0
    } else {
        let fade_elapsed = elapsed.saturating_sub(in_dur + idle_dur);
        1.0 - fraction(fade_elapsed, out_dur)
    }
}

/// `elapsed / total`, clamped — identical shape to
/// `core::fs::ops`'s/`saola-capture::modules::toast`'s own `fraction`
/// helpers, re-derived locally rather than shared (eight lines, three
/// independent modules, none of which may import each other — `core`
/// can't see `ui`, and this crate shares no code with `saola-capture` at
/// all).
fn fraction(elapsed: Duration, total: Duration) -> f32 {
    if total.is_zero() {
        return 1.0;
    }
    (elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
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

/// Renders the toast: a rotate-ccw icon, the frozen label, and an "Undo"
/// button — `App::view` only calls this while `App::undo_toast` is `Some`,
/// mirroring `ui::dialogs::progress::view`'s own "no empty rendering to
/// keep honest here, the caller decides" posture.
pub fn view<'a>(t: &'a Theme, toast: &'a Toast, now: Instant) -> Element<'a, Message> {
    let alpha = toast.alpha(t, now);
    let scale_alpha = |mut color: iced::Color| {
        color.a *= alpha;
        color
    };

    let icon_color = scale_alpha(t.on_paper.primary.into_iced());
    let text_color = scale_alpha(t.on_paper.primary.into_iced());
    let accent_text = scale_alpha(t.palette.accent.into_iced());

    let icon = icons::icon(Icon::RotateCcw, t.sizes.icon_row, icon_color);

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
    .style(style::button::bare(t, Surface::Paper))
    .padding([6.0, 10.0])
    .on_press(Message::UndoClicked);

    container(
        row![
            icon,
            label,
            container(undo)
                .width(Fill)
                .align_x(iced::alignment::Horizontal::Right)
        ]
        .spacing(t.sizes.pill_gap)
        .align_y(Center)
        .width(Fill),
    )
    .style(style::container::card(t, Surface::Paper))
    .width(Fill)
    .height(Length::Fixed(STRIP_HEIGHT))
    .padding([0.0, t.sizes.popover_padding / 2.0])
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

    #[test]
    fn alpha_fades_in_then_stays_at_full_then_fades_out() {
        let theme = theme();
        let in_dur = Duration::from_millis(theme.motion.toast_in.into());
        let idle_dur = Duration::from_millis(theme.motion.toast_idle.into());
        let total = Duration::from_millis(theme.motion.toast_total.into());

        assert_eq!(alpha_for(&theme, Duration::ZERO), 0.0);
        assert_eq!(alpha_for(&theme, in_dur), 1.0);
        assert_eq!(alpha_for(&theme, in_dur + idle_dur / 2), 1.0);
        assert_eq!(alpha_for(&theme, total), 0.0);
    }

    #[test]
    fn fraction_clamps_and_guards_a_zero_total() {
        assert_eq!(fraction(Duration::from_secs(1), Duration::ZERO), 1.0);
        assert_eq!(
            fraction(Duration::from_secs(10), Duration::from_secs(1)),
            1.0
        );
        assert_eq!(fraction(Duration::ZERO, Duration::from_secs(1)), 0.0);
    }

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
