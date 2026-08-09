//! The application core: the VFS trait and the pure data/comparators that
//! travel through it. iced-free (see CLAUDE.md's layering rule) — `ui/`
//! may depend on `core/`, never the other way around.

pub mod apps;
pub mod clipboard_interop;
pub mod fs;
pub mod mime;
pub mod places;
pub mod thumbs;
pub mod udisks;
pub mod vfs;
