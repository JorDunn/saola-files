//! The UI layer: window chrome, the directory view, the navigation
//! toolbar/breadcrumbs, the places sidebar, and the portal-seam explorer
//! composition. Dialogs land in a later stage.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod breadcrumbs;
pub mod dirview;
pub mod explorer;
pub mod header;
pub mod menus;
pub mod sidebar;
pub mod window;
