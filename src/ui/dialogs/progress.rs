//! The ops progress strip — live progress + Cancel for the app's one
//! active copy/move op — and the `Subscription` bridge from
//! `core::fs::ops::run`'s plain event stream.
//!
//! `main.rs::App` owns the actual [`Progress`] snapshot (updated in place
//! as `OpEvent::Started`/`Progress` arrive off the subscription below);
//! this module only ever renders whatever snapshot it's handed. Kept
//! separate from `core::fs::ops::OpEvent` itself so the strip's rendering
//! doesn't have to pattern-match a raw event on every frame — `App::update`
//! does that translation once, per event, same split `ui::sidebar` draws
//! between `core::udisks::Mount`s and its own render-time `view`.

use iced::widget::{button, column, container, progress_bar, row, text};
use iced::{Center, Element, Fill, Length, Subscription};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::ops::{OpEvent, OpKind, OpRequest};
use crate::icons::{self, Icon};

/// Height of the ops strip. Layout-specific to this chrome, not a
/// saola-theme design-system size — same "local constant, documented
/// upstream gap" posture `ui::sidebar::SIDEBAR_WIDTH` already takes.
/// TODO(saola-theme): promote to a `sizes.ops_strip` token if a second
/// consumer of the same height ever shows up; one call site isn't worth a
/// tag bump on its own yet.
const STRIP_HEIGHT: f32 = 56.0;

#[derive(Debug, Clone, Copy)]
pub enum Message {
    CancelRequested,
}

/// The live progress snapshot `App` accumulates from one op's event
/// stream. `main.rs` builds one from `OpEvent::Started` and mutates it in
/// place off every later `OpEvent::Progress`/`FileStarted`.
#[derive(Debug, Clone)]
pub struct Progress {
    pub kind: OpKind,
    pub current_name: Option<std::ffi::OsString>,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl Progress {
    pub fn started(kind: OpKind, files_total: usize, bytes_total: u64) -> Self {
        Progress {
            kind,
            current_name: None,
            files_done: 0,
            files_total,
            bytes_done: 0,
            bytes_total,
        }
    }

    /// `0.0..=100.0`, clamped. `core::fs::ops::count_totals`'s pre-scan is
    /// documented best-effort (it can undercount on a permission-denied
    /// subtree), so `bytes_done` can end up slightly past `bytes_total`
    /// near the end of such an op — this must still read as "done", never
    /// overshoot the bar past its own end.
    pub fn percent(&self) -> f32 {
        if self.bytes_total == 0 {
            // Nothing to divide by (every source was an empty directory,
            // or the pre-scan saw nothing at all) — "done" is the honest
            // reading, not a divide-by-zero.
            return 100.0;
        }
        ((self.bytes_done as f64 / self.bytes_total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// Bridges `core::fs::ops::run`'s plain `BoxStream` into an
/// `iced::Subscription`, identified by `request` (`OpRequest`'s manual
/// `Hash`-by-`id` — see that type's doc comment) so iced keeps the same
/// running op's stream alive across re-renders and tears it down the
/// moment `App::subscription` stops including it (the op finished, or
/// `active_op` was replaced by a fresh one). Mirrors `ui::dirview::watch::
/// subscription`'s `Subscription::run_with(location.clone(), build)` shape
/// exactly, just keyed by an op instead of a `Location`.
pub fn subscription(request: &OpRequest) -> Subscription<OpEvent> {
    Subscription::run_with(request.clone(), crate::core::fs::ops::run)
}

/// Renders the strip: an icon, "Copying/Moving <name>… N of M", a progress
/// bar, and a Cancel button. `App::view` only calls this while
/// `active_op`/`active_op_progress` are both `Some` — there is no "empty"
/// rendering here to keep honest, the caller decides whether to show it at
/// all.
pub fn view<'a>(t: &'a Theme, progress: &'a Progress) -> Element<'a, Message> {
    let verb = match progress.kind {
        OpKind::Copy => "Copying",
        OpKind::Move => "Moving",
    };
    let name = progress
        .current_name
        .as_deref()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label = if name.is_empty() {
        format!(
            "{verb}… {} of {}",
            progress.files_done, progress.files_total
        )
    } else {
        format!(
            "{verb} {name}… {} of {}",
            progress.files_done, progress.files_total
        )
    };

    let icon = icons::icon(
        Icon::ClipboardPaste,
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    );

    let text_col = column![
        text(label)
            .size(t.typography.size.secondary)
            .font(convert::ui_font(t)),
        progress_bar(0.0..=100.0, progress.percent())
            .style(style::progress::bar(t, Surface::Paper))
            .girth(6.0),
    ]
    .spacing(4.0)
    .width(Fill);

    let cancel = button(icons::icon(
        Icon::X,
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    ))
    .style(style::button::bare(t, Surface::Paper))
    .padding([6.0, 10.0])
    .on_press(Message::CancelRequested);

    container(
        row![icon, text_col, cancel]
            .spacing(t.sizes.pill_gap)
            .align_y(Center)
            .width(Fill),
    )
    .style(style::container::card(t, Surface::Paper))
    .width(Fill)
    .height(Length::Fixed(STRIP_HEIGHT))
    .padding([0.0, t.sizes.popover_padding / 2.0])
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_zero_at_the_start() {
        let progress = Progress::started(OpKind::Copy, 4, 4000);
        assert_eq!(progress.percent(), 0.0);
    }

    #[test]
    fn percent_scales_with_bytes_done() {
        let mut progress = Progress::started(OpKind::Copy, 4, 4000);
        progress.bytes_done = 2000;
        assert_eq!(progress.percent(), 50.0);
    }

    #[test]
    fn percent_never_exceeds_100_even_if_bytes_done_overshoots() {
        let mut progress = Progress::started(OpKind::Copy, 1, 100);
        progress.bytes_done = 150; // the pre-scan undercounted
        assert_eq!(progress.percent(), 100.0);
    }

    #[test]
    fn percent_is_100_when_the_total_is_zero() {
        let progress = Progress::started(OpKind::Copy, 0, 0);
        assert_eq!(progress.percent(), 100.0);
    }
}
