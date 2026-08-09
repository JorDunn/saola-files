//! The application core: the VFS trait and the pure data/comparators that
//! travel through it. iced-free (see CLAUDE.md's layering rule) — `ui/`
//! may depend on `core/`, never the other way around.

pub mod fs;
pub mod vfs;
