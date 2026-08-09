//! The UI layer: window chrome, the directory view, the navigation
//! toolbar/breadcrumbs, the places sidebar, the portal-seam explorer
//! composition, and (Stage 8) the app-level ops progress/conflict dialogs.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod breadcrumbs;
pub mod dialogs;
pub mod dirview;
pub mod explorer;
pub mod header;
pub mod menus;
pub mod sidebar;
pub mod window;
