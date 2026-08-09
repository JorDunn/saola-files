//! App-level chrome that isn't scoped to one directory view: the ops
//! progress strip and the conflict-resolution dialog (Stage 8).
//!
//! Both are driven entirely by `core::fs::ops` state living on `App`
//! (`active_op`/`active_op_progress`/`pending_conflict` in `main.rs`), not
//! by `DirectoryView` — a paste can still be streaming after the human
//! navigates the active view somewhere else, so this chrome must not live
//! inside the view whose row triggered it. Composed at `App::view`'s top
//! level, outside `ui::explorer`, the same "portal-seam stays free of
//! app-window concerns" split that already keeps window chrome
//! (`ui::window`) and now this out of `ui::explorer`.

pub mod conflict;
pub mod progress;
