//! The UI layer: window chrome, the directory view, the navigation
//! toolbar/breadcrumbs, and the portal-seam explorer composition. Sidebar
//! and dialogs land in later stages.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod breadcrumbs;
pub mod dirview;
pub mod explorer;
pub mod header;
pub mod menus;
pub mod window;
