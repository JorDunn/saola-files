//! Opening the context menu where the user pointed: right-click on a row,
//! tile or empty space, and the Menu key / Shift+F10 at the cursor row.
//!
//! The menu itself (its rows, its handlers) is `ui::menus` and `menu.rs`;
//! this file only decides *what the selection is* and *where the menu
//! goes*:
//!
//! - right-click an **unselected** entry: it becomes the only selection
//!   first, so the menu acts on what was clicked;
//! - right-click a **selected** entry: the selection is kept (the menu acts
//!   on all of it);
//! - right-click **empty space**: the selection is cleared and the menu is
//!   the folder menu (the same table with nothing selected);
//! - **Menu key / Shift+F10**: the cursor row is treated like a
//!   right-click on it, and the menu opens beside that row.
//!
//! Where the pointer was comes from `ui::right_click::RightClick`, which
//! sends [`Message::PointerPressed`] *before* the row's own
//! [`Message::RowPressed`] (or [`Message::EmptyPressed`]); `last_press`
//! carries the point from the first to the second. Placement itself (flip
//! at the window edge) happens at draw time in `ui::anchored`, because only
//! then is the menu's size known.

#[cfg(test)]
use iced::keyboard;
use iced::{Point, Rectangle, Size, Task};
use saola_theme::tokens::Sizes;

use crate::config::View;

use super::{DirectoryView, Event};

/// The view's own message type; inside this file plain `Message` is the
/// context-menu enum below (the same naming trick `menu.rs` uses).
type ViewMessage = super::Message;

/// Nested in the view's messages as `dirview::Message::Context`.
#[derive(Debug, Clone)]
pub enum Message {
    /// A right button press landed at this point, in the view's own frame
    /// (top-left of the directory area = origin). Sent first.
    PointerPressed(Point),
    /// That press landed on `visible[index]` (a row or a tile).
    RowPressed(usize),
    /// That press landed on empty space (nothing inside claimed it).
    EmptyPressed,
}

/// What an open popover hangs from. `None` on the view means the header's
/// overflow button, which keeps its legacy top-right anchoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuAnchor {
    /// Where the pointer was when the user right-clicked.
    Pointer(Point),
    /// The cursor row (Menu key / Shift+F10); resolved to a rectangle at
    /// view time because the row's position needs the theme's sizes.
    CursorRow,
}

impl DirectoryView {
    /// `update`'s `Message::Context` arm.
    pub(super) fn on_context(&mut self, message: Message) -> (Task<ViewMessage>, Option<Event>) {
        match message {
            Message::PointerPressed(point) => {
                self.last_press = Some(point);
            }
            Message::RowPressed(index) => self.on_context_row_pressed(index),
            Message::EmptyPressed => self.on_context_empty_pressed(),
        }
        (Task::none(), None)
    }

    /// Whether a right-click may open the menu right now: not while a rename
    /// or the path editor owns the pointer/keyboard, and not while a popover
    /// is already open (the scrim closes it instead). Ignoring a right-click
    /// during an inline rename is deliberate: it matches the keyboard guard
    /// in `handle_keyboard` (one input surface owns input at a time), and
    /// opening a menu would act on a selection the rename is still editing.
    fn context_menu_allowed(&self) -> bool {
        self.rename.is_none() && self.path_edit.is_none() && !self.menu_open && !self.open_with_open
    }

    fn on_context_row_pressed(&mut self, index: usize) {
        let press = self.last_press.take();
        if !self.context_menu_allowed() {
            return;
        }
        let Some(name) = self.entry_at(index).map(|entry| entry.name.clone()) else {
            return;
        };
        // A right-click is not half of a double-click.
        self.last_click = None;
        if self.selection.is_selected(&name) {
            // Keep the whole selection; just let the cursor follow so the
            // Menu key afterwards starts from here.
            self.selection.set_cursor(Some(index));
        } else {
            self.selection.click(index, name);
        }
        self.open_context_menu(press.map_or(MenuAnchor::CursorRow, MenuAnchor::Pointer));
    }

    fn on_context_empty_pressed(&mut self) {
        let press = self.last_press.take();
        if !self.context_menu_allowed() {
            return;
        }
        self.last_click = None;
        self.selection.clear();
        self.open_context_menu(press.map_or(MenuAnchor::CursorRow, MenuAnchor::Pointer));
    }

    /// `Action::OpenContextMenu` (Menu key, Shift+F10): the cursor row, made
    /// the selection if it is not already part of it, with the menu beside
    /// it. With no cursor row (an empty folder, or nothing visited yet) it
    /// is the folder menu at the top-left of the listing.
    pub(super) fn open_context_menu_at_cursor(&mut self) {
        if !self.context_menu_allowed() {
            return;
        }
        if let Some(index) = self.selection.cursor()
            && let Some(name) = self.entry_at(index).map(|entry| entry.name.clone())
            && !self.selection.is_selected(&name)
        {
            self.selection.click(index, name);
        }
        self.open_context_menu(MenuAnchor::CursorRow);
    }

    /// Forgets the anchor once no popover is open, so one opened some other
    /// way can never inherit it. Called from `update` after every message.
    pub(super) fn drop_stale_menu_anchor(&mut self) {
        if !self.menu_open && !self.open_with_open {
            self.menu_anchor = None;
        }
    }

    fn open_context_menu(&mut self, anchor: MenuAnchor) {
        self.menu_open = true;
        self.open_with_open = false;
        self.menu_anchor = Some(anchor);
    }

    /// Where the open popover is anchored, in the view's own frame, or
    /// `None` for the header's overflow button (legacy top-right placement).
    /// `ui::menus::overlay` calls this once per frame while a popover is
    /// open.
    pub fn menu_anchor_rect(&self, sizes: &Sizes) -> Option<Rectangle> {
        match self.menu_anchor? {
            MenuAnchor::Pointer(point) => Some(Rectangle::new(point, Size::ZERO)),
            MenuAnchor::CursorRow => Some(cursor_row_anchor(
                self.view_mode,
                self.selection.cursor(),
                self.scroll
                    .map_or(0.0, |viewport| viewport.absolute_offset().y.max(0.0)),
                super::grid::columns_for_scroll(self.scroll),
                sizes,
            )),
        }
    }
}

/// The anchor rectangle for the cursor row: zero wide, as tall as the row
/// or tile, at the row's left edge — so [`place`](crate::ui::menu_placement::place)
/// opens the menu just below it and flips above it, never over it.
///
/// Mirrors the geometry `list.rs` and `grid.rs` lay out (list: a header
/// band then fixed-height rows; grid: padded rows of `columns` tiles), the
/// same fixed-row-height assumption their own spacer math already makes.
/// With no cursor the anchor is the listing's top-left corner, zero tall.
pub(super) fn cursor_row_anchor(
    mode: View,
    cursor: Option<usize>,
    scroll_y: f32,
    columns: usize,
    sizes: &Sizes,
) -> Rectangle {
    let (x, top, height) = match mode {
        View::List => {
            let index = cursor.unwrap_or(0) as f32;
            // One `list_row`-tall header band sits above the rows.
            let top = sizes.list_row + index * sizes.list_row - scroll_y;
            let height = if cursor.is_some() {
                sizes.list_row
            } else {
                0.0
            };
            (sizes.pill_gap, top, height)
        }
        View::Grid => {
            let index = cursor.unwrap_or(0);
            let columns = columns.max(1);
            let (row, column) = (index / columns, index % columns);
            let tile_height = sizes.grid_tile + sizes.grid_tile_label;
            let x = sizes.grid_tile_gap + column as f32 * (sizes.grid_tile + sizes.grid_tile_gap);
            let top =
                sizes.grid_tile_gap + row as f32 * (tile_height + sizes.grid_tile_gap) - scroll_y;
            let height = if cursor.is_some() { tile_height } else { 0.0 };
            (x, top, height)
        }
    };
    Rectangle::new(Point::new(x, top), Size::new(0.0, height))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::ffi::OsString;

    fn selected_names(view: &DirectoryView) -> Vec<OsString> {
        let mut names: Vec<OsString> = view.selection.selected_names().cloned().collect();
        names.sort();
        names
    }

    /// Drives one right-click the way the widgets do: position first, then
    /// the row (or empty) message.
    fn right_click(view: &mut DirectoryView, at: Point, row: Option<usize>) {
        let _ = view.update(ViewMessage::Context(Message::PointerPressed(at)));
        let follow_up = row.map_or(Message::EmptyPressed, Message::RowPressed);
        let _ = view.update(ViewMessage::Context(follow_up));
    }

    #[test]
    fn right_click_on_an_unselected_row_selects_only_it_and_opens_at_the_pointer() {
        let mut view = listed_view(vec![file("a"), file("b"), file("c")]);
        view.selection.click(0, OsString::from("a"));
        let at = Point::new(120.0, 80.0);

        right_click(&mut view, at, Some(2));

        assert_eq!(selected_names(&view), [OsString::from("c")]);
        assert_eq!(view.selection.cursor(), Some(2));
        assert!(view.menu_open());
        assert_eq!(view.menu_anchor, Some(MenuAnchor::Pointer(at)));
    }

    #[test]
    fn right_click_on_a_selected_row_keeps_the_whole_selection() {
        let mut view = listed_view(vec![file("a"), file("b"), file("c")]);
        view.selection.click(0, OsString::from("a"));
        view.selection.toggle_click(1, OsString::from("b"));

        right_click(&mut view, Point::new(10.0, 10.0), Some(0));

        assert_eq!(
            selected_names(&view),
            [OsString::from("a"), OsString::from("b")]
        );
        assert!(view.menu_open());
    }

    #[test]
    fn right_click_on_empty_space_clears_the_selection_and_opens_the_folder_menu() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        view.selection.click(1, OsString::from("b"));
        let at = Point::new(300.0, 500.0);

        right_click(&mut view, at, None);

        assert_eq!(view.selected_count(), 0);
        assert!(view.menu_open());
        assert_eq!(view.menu_anchor, Some(MenuAnchor::Pointer(at)));
    }

    #[test]
    fn right_click_on_a_stale_row_index_does_nothing() {
        let mut view = listed_view(vec![file("a")]);
        right_click(&mut view, Point::new(1.0, 1.0), Some(9));
        assert!(!view.menu_open());
        assert_eq!(view.selected_count(), 0);
    }

    #[test]
    fn right_click_does_not_pair_with_a_previous_click_into_a_double_click() {
        let mut view = listed_view(vec![dir("docs")]);
        let _ = view.update(ViewMessage::RowClicked(0));
        assert!(view.last_click.is_some());
        right_click(&mut view, Point::new(1.0, 1.0), Some(0));
        assert!(view.last_click.is_none());
    }

    #[test]
    fn right_click_is_ignored_while_renaming_or_while_a_menu_is_open() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        view.menu_open = true;
        view.menu_anchor = Some(MenuAnchor::CursorRow);
        right_click(&mut view, Point::new(1.0, 1.0), Some(1));
        // Untouched: still the old anchor, nothing selected.
        assert_eq!(view.menu_anchor, Some(MenuAnchor::CursorRow));
        assert_eq!(view.selected_count(), 0);

        view.menu_open = false;
        view.path_edit = Some(String::new());
        right_click(&mut view, Point::new(1.0, 1.0), Some(1));
        assert!(!view.menu_open());
    }

    #[test]
    fn menu_key_selects_an_unselected_cursor_row_and_anchors_to_it() {
        let mut view = listed_view(vec![file("a"), file("b")]);
        view.selection.set_cursor(Some(1));
        assert_eq!(view.selected_count(), 0);

        view.open_context_menu_at_cursor();

        assert_eq!(selected_names(&view), [OsString::from("b")]);
        assert!(view.menu_open());
        assert_eq!(view.menu_anchor, Some(MenuAnchor::CursorRow));
    }

    #[test]
    fn menu_key_keeps_a_multi_selection_that_includes_the_cursor_row() {
        let mut view = listed_view(vec![file("a"), file("b"), file("c")]);
        view.selection.click(0, OsString::from("a"));
        view.selection.toggle_click(2, OsString::from("c"));

        view.open_context_menu_at_cursor();

        assert_eq!(view.selected_count(), 2);
        assert!(view.menu_open());
    }

    #[test]
    fn menu_key_with_no_cursor_opens_the_folder_menu() {
        let mut view = listed_view(vec![file("a")]);
        view.open_context_menu_at_cursor();
        assert_eq!(view.selected_count(), 0);
        assert!(view.menu_open());
    }

    #[test]
    fn opening_from_the_header_button_drops_a_stale_pointer_anchor() {
        let mut view = listed_view(vec![file("a")]);
        right_click(&mut view, Point::new(5.0, 5.0), None);
        let _ = view.update(ViewMessage::Menu(super::super::menu::Message::Close));
        let _ = view.update(ViewMessage::Menu(super::super::menu::Message::Open));
        assert!(view.menu_open());
        assert_eq!(view.menu_anchor, None);
    }

    #[test]
    fn the_anchor_is_forgotten_whenever_the_popover_closes() {
        let mut view = listed_view(vec![file("a")]);
        right_click(&mut view, Point::new(5.0, 5.0), None);
        assert!(view.menu_anchor.is_some());
        // Close by a path that never touches `menu_anchor` itself: Escape.
        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let _ = view.update(ViewMessage::Keyboard(escape));
        assert!(!view.menu_open());
        assert_eq!(view.menu_anchor, None);
    }

    #[test]
    fn folder_properties_bubbles_this_views_own_location() {
        let mut view = listed_view(vec![file("a")]);
        let message = ViewMessage::Menu(super::super::menu::Message::FolderPropertiesRequested);
        let (_, event) = view.update(message);
        assert!(matches!(
            event,
            Some(Event::FolderPropertiesRequested(location)) if location == crate::core::vfs::Location::local("/home")
        ));
    }

    #[test]
    fn list_cursor_row_anchor_sits_under_the_header_and_follows_the_scroll() {
        let sizes = Sizes::default();
        let rect = cursor_row_anchor(View::List, Some(3), 0.0, 1, &sizes);
        assert_eq!(rect.x, sizes.pill_gap);
        assert_eq!(rect.y, sizes.list_row * 4.0);
        assert_eq!(rect.height, sizes.list_row);
        assert_eq!(rect.width, 0.0);

        let scrolled = cursor_row_anchor(View::List, Some(3), sizes.list_row * 2.0, 1, &sizes);
        assert_eq!(scrolled.y, sizes.list_row * 2.0);
    }

    #[test]
    fn grid_cursor_row_anchor_uses_the_tile_column_and_row() {
        let sizes = Sizes::default();
        // Index 7 in 3 columns: row 2, column 1.
        let rect = cursor_row_anchor(View::Grid, Some(7), 0.0, 3, &sizes);
        let tile_height = sizes.grid_tile + sizes.grid_tile_label;
        assert_eq!(
            rect.x,
            sizes.grid_tile_gap + (sizes.grid_tile + sizes.grid_tile_gap)
        );
        assert_eq!(
            rect.y,
            sizes.grid_tile_gap + 2.0 * (tile_height + sizes.grid_tile_gap)
        );
        assert_eq!(rect.height, tile_height);
    }

    #[test]
    fn a_grid_with_zero_columns_does_not_divide_by_zero() {
        let sizes = Sizes::default();
        let rect = cursor_row_anchor(View::Grid, Some(4), 0.0, 0, &sizes);
        assert!(rect.x.is_finite() && rect.y.is_finite());
    }

    #[test]
    fn no_cursor_anchors_at_the_listing_corner_with_no_height() {
        let sizes = Sizes::default();
        let rect = cursor_row_anchor(View::List, None, 0.0, 1, &sizes);
        assert_eq!(rect.height, 0.0);
        assert_eq!(rect.y, sizes.list_row);
    }
}
