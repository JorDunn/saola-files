//! A single-child widget that places its child next to an anchor rectangle
//! using [`menu_placement::place`](super::menu_placement::place).
//!
//! Why a custom widget: where a popover goes depends on its own size
//! (does it fit below the pointer, or must it flip up?), and a plain
//! `view()` cannot know a widget's laid-out size. A widget's `layout` is
//! the one place in iced where both the available space (`limits`) and the
//! child's measured size (`child.layout(..)`) exist at the same time, so
//! the placement decision is made there.
//!
//! The structure mirrors iced's own `pin` widget (iced_widget 0.14.2,
//! `pin.rs`): fill the parent, lay the child out with loose limits, move
//! the child's node. The only difference is *where* it moves it.
//!
//! Rust note: the `Widget` trait is iced's extension point; every method
//! below that is not `layout` just forwards to the child, using the child's
//! layout node (always the only entry in `layout.children()`). The
//! forwarding uses `if let Some(..)` rather than `unwrap` because the
//! no-panic rule applies to widget code as much as anywhere else.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};

use super::menu_placement::place;

/// Places `content` beside `anchor` (see [`place`]); fills its parent so
/// that "the window" for the flip rule is the parent's bounds.
pub struct Anchored<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    anchor: Rectangle,
    gap: f32,
}

impl<'a, Message, Theme, Renderer> Anchored<'a, Message, Theme, Renderer> {
    /// `anchor` is in the coordinate frame of this widget's own top-left;
    /// `gap` is the clearance kept between anchor and content.
    pub fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        anchor: Rectangle,
        gap: f32,
    ) -> Self {
        Self {
            content: content.into(),
            anchor,
            gap,
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Anchored<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<widget::Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(Length::Fill).height(Length::Fill);
        let bounds = limits.max();

        // Measure the child with everything available, then decide where
        // it goes now that its size is known.
        let node = self.content.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let origin = place(self.anchor, node.size(), bounds, self.gap);
        let node = node.move_to(origin);

        let size = limits.resolve(Length::Fill, Length::Fill, Size::ZERO);
        layout::Node::with_children(size, vec![node])
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        if let Some(child) = layout.children().next() {
            self.content
                .as_widget_mut()
                .operate(tree, child, renderer, operation);
        }
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Some(child) = layout.children().next() {
            self.content.as_widget_mut().update(
                tree, event, child, cursor, renderer, clipboard, shell, viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        layout
            .children()
            .next()
            .map_or(mouse::Interaction::None, |child| {
                self.content
                    .as_widget()
                    .mouse_interaction(tree, child, cursor, viewport, renderer)
            })
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if let Some(child) = layout.children().next()
            && let Some(clipped) = layout.bounds().intersection(viewport)
        {
            self.content
                .as_widget()
                .draw(tree, renderer, theme, style, child, cursor, &clipped);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let child = layout.children().next()?;
        self.content
            .as_widget_mut()
            .overlay(tree, child, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<Anchored<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(anchored: Anchored<'a, Message, Theme, Renderer>) -> Self {
        Element::new(anchored)
    }
}
