//! The UI layer: window chrome, the directory view, and the portal-seam
//! explorer composition. Sidebar, breadcrumbs and dialogs land in later
//! stages.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod dirview;
pub mod explorer;
pub mod window;
