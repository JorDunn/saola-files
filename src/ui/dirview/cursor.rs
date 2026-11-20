//! Cursor movement and range-extension helpers (`move_cursor*`,
//! `extend_*`), plus `row_step` — how far one vertical step moves in the
//! current presentation.

use crate::config::View;

use super::{DirectoryView, grid};

impl DirectoryView {
    /// How many `visible` positions one Up/Down/PageUp/PageDown step
    /// covers: one item in list view, one full tile-row in grid view — so
    /// "Down" always means "the next thing spatially below", not "the next
    /// name alphabetically", regardless of presentation. Left/Right
    /// (grid-only) always step by exactly one, independent of this.
    ///
    /// The grid arm asks `grid::columns_for_scroll` rather than reading a
    /// constant, so a resized window moves the cursor by however many tiles
    /// are *actually* on a row now. `update()` can't see the `responsive`
    /// closure's measured `Size` that `grid::view` uses, so that helper
    /// takes the width from `self.scroll` instead — see its doc comment for
    /// the one-frame staleness that buys and why it's harmless.
    pub(super) fn row_step(&self) -> isize {
        match self.view_mode {
            View::List => 1,
            View::Grid => grid::columns_for_scroll(self.scroll) as isize,
        }
    }

    // ── cursor/selection helpers ────────────────────────────────────────

    fn clamp_index(&self, index: isize) -> Option<usize> {
        if self.visible.is_empty() {
            return None;
        }
        let max = self.visible.len() as isize - 1;
        Some(index.clamp(0, max) as usize)
    }

    /// The cursor's current position for movement math, as if it sat one
    /// *step* before the first when nothing is selected/cursored yet — not
    /// `0`, which would make the very first `MoveCursorDown` land one step
    /// past row 0 (skipping it) instead of landing on it.
    ///
    /// Parametrized by `step` (the magnitude of the delta about to be
    /// applied) rather than hardcoded to `-1`: in list view a "step" is
    /// one item, so the sentinel `-1` plus a `+1` delta lands on `0` — the
    /// original Stage 3 behavior. In grid view a vertical "step" is a
    /// whole `row_step()`-sized row; using the same `-1` sentinel there
    /// would land the very first `MoveCursorDown` on row *index*
    /// `row_step() - 1` instead of `0` (verified against
    /// `grid_view_steps_the_cursor_by_a_full_row`'s first assertion,
    /// which is what caught this). Using `-step` generalizes both: the
    /// first step in *either* direction always resolves to `0` (a
    /// negative starting point clamps there regardless of which way it
    /// was pushed).
    fn current_cursor_or_before_start(&self, step: isize) -> isize {
        self.selection.cursor().map_or(-step, |c| c as isize)
    }

    pub(super) fn move_cursor(&mut self, delta: isize) {
        self.move_cursor_to_signed(self.current_cursor_or_before_start(delta.abs()) + delta);
    }

    pub(super) fn move_cursor_to(&mut self, index: usize) {
        self.move_cursor_to_signed(index as isize);
    }

    fn move_cursor_to_signed(&mut self, index: isize) {
        let Some(target) = self.clamp_index(index) else {
            return;
        };
        let Some(name) = self.entry_at(target).map(|entry| entry.name.clone()) else {
            return;
        };
        self.selection.click(target, name);
    }

    pub(super) fn extend_cursor(&mut self, delta: isize) {
        self.extend_to_signed(self.current_cursor_or_before_start(delta.abs()) + delta);
    }

    pub(super) fn extend_to(&mut self, index: usize) {
        self.extend_to_signed(index as isize);
    }

    fn extend_to_signed(&mut self, index: isize) {
        let Some(target) = self.clamp_index(index) else {
            return;
        };
        let anchor = self
            .selection
            .anchor()
            .or(self.selection.cursor())
            .unwrap_or(target);
        let (lo, hi) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        let names = (lo..=hi)
            .filter_map(|i| self.entry_at(i).map(|entry| entry.name.clone()))
            .collect::<Vec<_>>();
        self.selection.range_select(target, names);
    }
}
