//! A wrapper that reports where a right-click landed, and whether anything
//! inside it claimed the click.
//!
//! iced 0.14's `mouse_area(..).on_right_press(msg)` carries no position,
//! and its `on_move` would send a message on every pointer move (a full
//! `update` + redraw per mouse event) just to remember a point we only need
//! at click time. This widget reads the position at the moment of the press
//! instead: `cursor.position_in(bounds)`, relative to this widget's own
//! top-left, which is the same frame the popover overlay is placed in
//! (`menus::overlay` stacks both over the same bounds).
//!
//! Message order on one right press, which the handlers rely on:
//!
//! 1. `on_press(point)` — published *before* the child sees the event;
//! 2. whatever the child publishes (a row's own `RowPressed(index)`);
//! 3. `on_empty` — published only if the child did **not** capture the
//!    event, i.e. the click landed on empty space.
//!
//! iced's `Shell::publish` appends to a queue, so these arrive in that
//! order.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector, mouse};

pub struct RightClick<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_press: Box<dyn Fn(Point) -> Message + 'a>,
    on_empty: Message,
}

impl<'a, Message, Theme, Renderer> RightClick<'a, Message, Theme, Renderer> {
    pub fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        on_press: impl Fn(Point) -> Message + 'a,
        on_empty: Message,
    ) -> Self {
        Self {
            content: content.into(),
            on_press: Box::new(on_press),
            on_empty,
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for RightClick<'_, Message, Theme, Renderer>
where
    Message: Clone,
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
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Same size and position as the child: this wrapper adds no chrome.
        let node = self.content.as_widget_mut().layout(tree, renderer, limits);
        layout::Node::with_children(node.size(), vec![node])
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
        let Some(child) = layout.children().next() else {
            return;
        };

        let position = match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))
                if !shell.is_event_captured() =>
            {
                cursor.position_in(layout.bounds())
            }
            _ => None,
        };
        if let Some(point) = position {
            shell.publish((self.on_press)(point));
        }

        self.content.as_widget_mut().update(
            tree, event, child, cursor, renderer, clipboard, shell, viewport,
        );

        if position.is_some() && !shell.is_event_captured() {
            shell.publish(self.on_empty.clone());
            shell.capture_event();
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
        if let Some(child) = layout.children().next() {
            self.content
                .as_widget()
                .draw(tree, renderer, theme, style, child, cursor, viewport);
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

impl<'a, Message, Theme, Renderer> From<RightClick<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(area: RightClick<'a, Message, Theme, Renderer>) -> Self {
        Element::new(area)
    }
}
