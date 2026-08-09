//! The UI layer: window chrome now; header, sidebar, directory views and
//! dialogs in later stages.
//!
//! Layering rule (see CLAUDE.md): `ui/` may import `core/`, never
//! `integration/`.

pub mod window;
