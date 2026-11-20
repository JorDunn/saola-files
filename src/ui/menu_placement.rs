//! Where a pointer- or row-anchored popover goes: pure geometry, no iced
//! widgets, so every edge case is a plain unit test.
//!
//! The rule (style guide §6, "Popover"): the popover grows down and to the
//! right from its anchor; on an axis where that would leave the bounds it
//! flips to grow up/left instead; and it never ends up outside the bounds,
//! even when neither direction fits (a window smaller than the menu).
//! `gap` keeps the popover off the anchor itself, so the pointer that
//! opened a menu is never underneath it.

use iced::{Point, Rectangle, Size};

/// The top-left corner for a popover of size `menu`, opened from `anchor`,
/// inside a region of size `bounds` (all in the same coordinate frame, the
/// region's own top-left being the origin).
///
/// `anchor` is the thing the popover must not cover: a zero-size rectangle
/// for a pointer click, and for the Menu key a zero-wide, row-tall one (so
/// the popover clears the cursor row vertically).
pub fn place(anchor: Rectangle, menu: Size, bounds: Size, gap: f32) -> Point {
    Point::new(
        place_axis(
            anchor.x,
            anchor.x + anchor.width,
            menu.width,
            bounds.width,
            gap,
        ),
        place_axis(
            anchor.y,
            anchor.y + anchor.height,
            menu.height,
            bounds.height,
            gap,
        ),
    )
}

/// One axis of [`place`]. `start`/`end` are the anchor's near and far edges
/// along the axis; the popover prefers to begin just after `end`.
fn place_axis(start: f32, end: f32, menu: f32, bounds: f32, gap: f32) -> f32 {
    // The largest origin that keeps the popover inside; 0 when it is bigger
    // than the region (then it sits at the origin and is cut off at the far
    // side, which is the best that can be done).
    let max_origin = (bounds - menu).max(0.0);

    let after = end + gap;
    if after >= 0.0 && after <= max_origin {
        return after;
    }
    let before = start - gap - menu;
    if before >= 0.0 && before <= max_origin {
        return before;
    }
    // Neither side fits inside the region. This also covers an anchor that
    // is itself outside the region (a cursor row scrolled out of view):
    // take the nearest position that is fully visible, so the popover is
    // never placed off-screen. `max`/`min` rather than `clamp`, which
    // panics on a bad range.
    after.max(0.0).min(max_origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: f32 = 4.0;
    const MENU: Size = Size::new(200.0, 300.0);
    const WINDOW: Size = Size::new(1000.0, 800.0);

    fn at(x: f32, y: f32) -> Rectangle {
        Rectangle::new(Point::new(x, y), Size::ZERO)
    }

    #[test]
    fn grows_down_and_right_from_the_pointer_when_there_is_room() {
        let origin = place(at(100.0, 100.0), MENU, WINDOW, GAP);
        assert_eq!(origin, Point::new(104.0, 104.0));
    }

    #[test]
    fn flips_left_at_the_right_edge() {
        let origin = place(at(900.0, 100.0), MENU, WINDOW, GAP);
        // Right edge of the menu sits `gap` left of the pointer.
        assert_eq!(origin, Point::new(900.0 - GAP - 200.0, 104.0));
    }

    #[test]
    fn flips_up_at_the_bottom_edge() {
        let origin = place(at(100.0, 700.0), MENU, WINDOW, GAP);
        assert_eq!(origin, Point::new(104.0, 700.0 - GAP - 300.0));
    }

    #[test]
    fn flips_both_axes_in_the_bottom_right_corner() {
        let origin = place(at(995.0, 795.0), MENU, WINDOW, GAP);
        assert_eq!(origin, Point::new(995.0 - GAP - 200.0, 795.0 - GAP - 300.0));
    }

    #[test]
    fn fits_exactly_against_the_far_edge_without_flipping() {
        // after + menu == bounds: still fits, so no flip.
        let origin = place(
            at(1000.0 - 200.0 - GAP, 800.0 - 300.0 - GAP),
            MENU,
            WINDOW,
            GAP,
        );
        assert_eq!(origin, Point::new(1000.0 - 200.0, 800.0 - 300.0));
    }

    #[test]
    fn a_popover_that_fits_neither_side_is_pinned_inside_the_bounds() {
        // 250 wide region, 200 wide menu, pointer in the middle: 125+4+200
        // overflows right, 125-4-200 is negative on the left.
        let small = Size::new(250.0, 250.0);
        let origin = place(at(125.0, 125.0), MENU, small, GAP);
        assert_eq!(origin.x, 50.0);
        // The menu is 300 tall in a 250 region: top-aligned, as much as fits.
        assert_eq!(origin.y, 0.0);
    }

    #[test]
    fn a_popover_bigger_than_the_bounds_sits_at_the_origin() {
        let origin = place(at(10.0, 10.0), MENU, Size::new(100.0, 100.0), GAP);
        assert_eq!(origin, Point::ORIGIN);
    }

    #[test]
    fn never_covers_the_pointer_when_it_fits() {
        for (x, y) in [(0.0, 0.0), (500.0, 400.0), (999.0, 799.0), (0.0, 799.0)] {
            let origin = place(at(x, y), MENU, WINDOW, GAP);
            let menu = Rectangle::new(origin, MENU);
            assert!(!menu.contains(Point::new(x, y)), "covered ({x}, {y})");
        }
    }

    #[test]
    fn never_leaves_the_window() {
        let window = Rectangle::new(Point::ORIGIN, WINDOW);
        for x in [0.0, 1.0, 499.0, 800.0, 801.0, 999.0, 1000.0] {
            for y in [0.0, 1.0, 399.0, 500.0, 501.0, 799.0, 800.0] {
                let origin = place(at(x, y), MENU, WINDOW, GAP);
                let menu = Rectangle::new(origin, MENU);
                assert!(
                    origin.x >= 0.0
                        && origin.y >= 0.0
                        && menu.x + menu.width <= window.width
                        && menu.y + menu.height <= window.height,
                    "({x}, {y}) -> {origin:?}"
                );
            }
        }
    }

    /// Every axis result must be fully inside, for any anchor at all.
    fn assert_inside(origin: Point, what: &str) {
        assert!(
            origin.x >= 0.0 && origin.x + MENU.width <= WINDOW.width,
            "{what}: {origin:?}"
        );
        assert!(
            origin.y >= 0.0 && origin.y + MENU.height <= WINDOW.height,
            "{what}: {origin:?}"
        );
    }

    #[test]
    fn an_anchor_far_below_the_bounds_still_places_the_menu_inside() {
        // A cursor row scrolled out of view below the viewport.
        let row = Rectangle::new(Point::new(20.0, 2000.0), Size::new(0.0, 38.0));
        let origin = place(row, MENU, WINDOW, GAP);
        assert_inside(origin, "below");
        assert_eq!(origin.y, WINDOW.height - MENU.height);
    }

    #[test]
    fn an_anchor_far_above_the_bounds_still_places_the_menu_inside() {
        let row = Rectangle::new(Point::new(20.0, -2000.0), Size::new(0.0, 38.0));
        let origin = place(row, MENU, WINDOW, GAP);
        assert_inside(origin, "above");
        assert_eq!(origin.y, 0.0);
    }

    #[test]
    fn an_anchor_far_right_or_left_of_the_bounds_still_places_the_menu_inside() {
        let right = place(at(5000.0, 100.0), MENU, WINDOW, GAP);
        assert_inside(right, "right");
        assert_eq!(right.x, WINDOW.width - MENU.width);
        let left = place(at(-5000.0, 100.0), MENU, WINDOW, GAP);
        assert_inside(left, "left");
        assert_eq!(left.x, 0.0);
    }

    #[test]
    fn an_anchor_inside_the_bounds_is_unchanged_by_the_clamp() {
        let origin = place(at(300.0, 200.0), MENU, WINDOW, GAP);
        assert_eq!(origin, Point::new(304.0, 204.0));
    }

    #[test]
    fn a_far_anchor_sweep_never_leaves_the_window() {
        for x in [-9000.0, -1.0, 0.0, 500.0, 1000.0, 1001.0, 9000.0] {
            for y in [-9000.0, -1.0, 0.0, 400.0, 800.0, 801.0, 9000.0] {
                assert_inside(place(at(x, y), MENU, WINDOW, GAP), "sweep");
            }
        }
    }

    #[test]
    fn a_row_anchor_opens_below_the_row_and_flips_above_it() {
        // A row anchor is zero wide (a point on the row's left) and as tall
        // as the row, so the menu clears the row vertically.
        let row = Rectangle::new(Point::new(20.0, 100.0), Size::new(0.0, 38.0));
        let below = place(row, MENU, WINDOW, GAP);
        assert_eq!(below, Point::new(20.0 + GAP, 100.0 + 38.0 + GAP));
        // No room below: sits above the row, never covering it.
        let low = Rectangle::new(Point::new(20.0, 760.0), Size::new(0.0, 38.0));
        let above = place(low, MENU, WINDOW, GAP);
        assert_eq!(above.y, 760.0 - GAP - 300.0);
    }
}
