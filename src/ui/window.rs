//! Window chrome for the app's single toplevel window.
//!
//! saola-files is the first *ordinary* window in the Saola desktop — every
//! sibling app is a layer-shell surface — and niri draws no server-side
//! decorations, so all chrome is ours: the rounded ivory `paper_window`
//! container, the 46 px header (title, close pill), and the interactive
//! move/resize regions. Per the style guide there is **no minimise button**
//! (niri has no taskbar to minimise into) and no maximise button either —
//! double-clicking the header toggles maximise, matching how niri users
//! expect windows to behave.
//!
//! The window itself is created with `decorations: false, transparent: true`
//! (see `main.rs`); the compositor-visible shape is whatever we paint, which
//! is `container::paper_window` — 24 px radius, 2 px ink border. The corners
//! outside that radius stay genuinely transparent because the app-level
//! `Style` sets a transparent background (the hard-won capture lesson: iced
//! otherwise clears the surface to ink first, and the corners render as
//! square ink wedges).

use iced::widget::{Space, button, column, container, mouse_area, row, text};
use iced::{Element, Fill, Task, mouse, window};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::icons::{self, Icon};

/// Thickness of the invisible resize strips along each window edge.
///
/// Not a design-system size: this is an interaction hit zone, not a drawn
/// element (the strips paint nothing). 4 px matches typical CSD grab edges.
const RESIZE_EDGE: f32 = 4.0;

/// Side length of the invisible corner resize squares. Larger than the edge
/// strips because diagonal grabs are aimed at a point, not a line.
const RESIZE_CORNER: f32 = 12.0;

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
    /// Pointer pressed on an edge/corner strip — start an interactive
    /// resize in that direction.
    Resize(window::Direction),
}

/// Turns a chrome [`Event`] into the corresponding runtime task.
///
/// Every task needs the window's `Id`. A single-window `iced::application`
/// never hands the view its Id, so we ask the runtime for the latest window
/// at the moment of the interaction; `and_then` runs the action only when a
/// window actually exists (`None` can't happen while the user is clicking
/// our header, but the no-panic rule says degrade, not unwrap).
pub fn update<M: Send + 'static>(event: Event) -> Task<M> {
    match event {
        Event::Drag => window::latest().and_then(window::drag),
        Event::ToggleMaximize => window::latest().and_then(window::toggle_maximize),
        Event::Close => window::latest().and_then(window::close),
        Event::Resize(direction) => {
            window::latest().and_then(move |id| window::drag_resize(id, direction))
        }
    }
}

/// The full window: paper chrome, 46 px header, `body` below, and the
/// invisible resize regions stacked on top.
///
/// Generic over the parent's message type: `map` lifts chrome [`Event`]s
/// into it, so this module never needs to know about app messages.
pub fn view<'a, M: Clone + 'a>(
    theme: &'a Theme,
    title: &'a str,
    body: Element<'a, M>,
    map: impl Fn(Event) -> M + 'a,
) -> Element<'a, M> {
    let t = theme;

    // ── Header ──────────────────────────────────────────────────────────
    // Title on the left, close pill on the right. The close button sits
    // inside the drag mouse_area, but buttons capture their own presses, so
    // clicking it closes rather than dragging.
    //
    let close = button(icons::icon(
        Icon::X,
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    ))
    .style(style::button::bare(t, Surface::Paper))
    .padding([6, 12])
    .on_press(map(Event::Close));

    let header_row = row![
        text(title)
            .font(convert::ui_font(t))
            .size(t.typography.size.body)
            .color(convert::ColorExt::into_iced(t.on_paper.secondary)),
        Space::new().width(Fill),
        close,
    ]
    .align_y(iced::Center)
    .width(Fill);

    let header = mouse_area(
        container(header_row)
            .height(t.sizes.window_header)
            .padding([0, 18])
            .align_y(iced::Center),
    )
    .on_press(map(Event::Drag))
    .on_double_click(map(Event::ToggleMaximize));

    // ── Frame ───────────────────────────────────────────────────────────
    let frame = container(column![header, body].width(Fill).height(Fill))
        .style(style::container::paper_window(t))
        .width(Fill)
        .height(Fill);

    // ── Resize regions ──────────────────────────────────────────────────
    // Invisible strips along each edge and squares in each corner, stacked
    // over the frame. They paint nothing; they exist to catch presses and
    // show the right resize cursor. Corners come after edges in the stack
    // so they win where the two overlap.
    use iced::Length;
    use mouse::Interaction::{
        ResizingDiagonallyDown, ResizingDiagonallyUp, ResizingHorizontally, ResizingVertically,
    };
    use window::Direction::{East, North, NorthEast, NorthWest, South, SouthEast, SouthWest, West};

    // A press-catching strip of the given size, pinned to a window edge or
    // corner by the alignment pair, mapped to a resize direction.
    let grip = |width: Length,
                height: Length,
                x: iced::alignment::Horizontal,
                y: iced::alignment::Vertical,
                direction: window::Direction,
                cursor: mouse::Interaction| {
        container(
            mouse_area(Space::new().width(width).height(height))
                .on_press(map(Event::Resize(direction)))
                .interaction(cursor),
        )
        .width(Fill)
        .height(Fill)
        .align_x(x)
        .align_y(y)
    };

    let edge = Length::Fixed(RESIZE_EDGE);
    let corner = Length::Fixed(RESIZE_CORNER);

    iced::widget::stack![
        frame,
        // Edges: full-length strips along each side.
        grip(
            edge,
            Fill,
            iced::Left,
            iced::alignment::Vertical::Center,
            West,
            ResizingHorizontally
        ),
        grip(
            edge,
            Fill,
            iced::Right,
            iced::alignment::Vertical::Center,
            East,
            ResizingHorizontally
        ),
        grip(
            Fill,
            edge,
            iced::alignment::Horizontal::Center,
            iced::Top,
            North,
            ResizingVertically
        ),
        grip(
            Fill,
            edge,
            iced::alignment::Horizontal::Center,
            iced::Bottom,
            South,
            ResizingVertically
        ),
        // Corners, on top of the edges.
        grip(
            corner,
            corner,
            iced::Left,
            iced::Top,
            NorthWest,
            ResizingDiagonallyDown
        ),
        grip(
            corner,
            corner,
            iced::Right,
            iced::Top,
            NorthEast,
            ResizingDiagonallyUp
        ),
        grip(
            corner,
            corner,
            iced::Left,
            iced::Bottom,
            SouthWest,
            ResizingDiagonallyUp
        ),
        grip(
            corner,
            corner,
            iced::Right,
            iced::Bottom,
            SouthEast,
            ResizingDiagonallyDown
        ),
    ]
    .into()
}
