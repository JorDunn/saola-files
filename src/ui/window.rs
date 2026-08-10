//! Window chrome for the app's single toplevel window.
//!
//! saola-files is the first *ordinary* window in the Saola desktop — every
//! sibling app is a layer-shell surface — and niri draws no server-side
//! decorations, so all chrome is ours. **Stage 12 (saola-theme 0.7
//! adoption):** the geometry and layout this module used to own directly
//! (the header row, the paper frame, the eight invisible resize grips) is
//! now [`saola_theme::chrome`] — the upstreamed copy of what this file
//! looked like through Stage 11, after saola-capture needed the same header
//! shape. This module is now just the app-specific wiring `chrome`'s own
//! module docs describe as the consumer's job: turning a chrome
//! [`chrome::ResizeEdge`]/press into the actual `iced::window` runtime
//! `Task`.
//!
//! The window itself is created with `decorations: false, transparent: true`
//! (see `main.rs`); the compositor-visible shape is whatever we paint, which
//! is `chrome::window_frame`'s `style::container::paper_window` — 24 px
//! radius, 2 px ink border. The corners outside that radius stay genuinely
//! transparent because the app-level `Style` sets a transparent background
//! via `saola_theme::chrome::transparent_clear` (the hard-won capture
//! lesson: iced otherwise clears the surface to ink first, and the corners
//! render as square ink wedges — see that function's doc comment for the
//! full account, now upstream rather than re-derived here).

use iced::{Element, Task, window};
use saola_theme::Theme;
use saola_theme::chrome::{self, ResizeEdge};

/// Chrome interactions. The parent wraps these into its own message type via
/// the `map` closure passed to [`view`].
#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// Pointer pressed on the header background — start an interactive move.
    Drag,
    /// Header double-clicked — toggle maximise.
    ToggleMaximize,
    /// The close pill.
    Close,
    /// Pointer pressed on an edge/corner grip — start an interactive resize
    /// toward that grip's edge/corner.
    Resize(ResizeEdge),
}

/// Turns a chrome [`Event`] into the corresponding runtime task.
///
/// Every task needs the window's `Id`. A single-window `iced::application`
/// never hands the view its Id, so we ask the runtime for the latest window
/// at the moment of the interaction; `and_then` runs the action only when a
/// window actually exists (`None` can't happen while the user is clicking
/// our header, but the no-panic rule says degrade, not unwrap). This is
/// exactly the dispatch `chrome`'s own module doc comment shows as the
/// canonical shape for a `chrome` consumer.
pub fn update<M: Send + 'static>(event: Event) -> Task<M> {
    match event {
        Event::Drag => window::latest().and_then(window::drag),
        Event::ToggleMaximize => window::latest().and_then(window::toggle_maximize),
        Event::Close => window::latest().and_then(window::close),
        Event::Resize(edge) => {
            let direction = edge.direction();
            window::latest().and_then(move |id| window::drag_resize(id, direction))
        }
    }
}

/// The full window: [`chrome::window_frame`]'s paper chrome and
/// `chrome::window_header`'s 46 px header (title, close pill) around `body`,
/// with [`chrome::with_resize_grips`] stacking the eight invisible
/// resize regions on top — this window is resizable, so [`Event::ToggleMaximize`]
/// is always wired (`Some`), unlike saola-capture's fixed-size header, which
/// passes `None`.
///
/// Generic over the parent's message type: `map` lifts chrome [`Event`]s
/// into it, so this module never needs to know about app messages.
pub fn view<'a, M: Clone + 'a>(
    theme: &'a Theme,
    title: &'a str,
    body: Element<'a, M>,
    map: impl Fn(Event) -> M + 'a,
) -> Element<'a, M> {
    let header = chrome::window_header(
        theme,
        title,
        map(Event::Close),
        map(Event::Drag),
        Some(map(Event::ToggleMaximize)),
    );
    let frame = chrome::window_frame(theme, header, body);
    chrome::with_resize_grips(frame, move |edge| map(Event::Resize(edge)))
}
