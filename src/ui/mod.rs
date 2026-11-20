//! The UI layer: window chrome, the directory view, the navigation
//! toolbar/breadcrumbs, the places sidebar, the portal-seam explorer
//! composition, and (Stage 8) the app-level ops progress/conflict dialogs.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod anchored;
pub mod breadcrumbs;
pub mod dialogs;
pub mod dirview;
pub mod explorer;
pub mod header;
pub mod menu_placement;
pub mod menus;
pub mod right_click;
pub mod sidebar;
pub mod statusbar;
pub mod trashview;
pub mod window;
