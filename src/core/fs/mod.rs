//! Pure, iced-free directory-listing data and ordering: the model
//! ([`entry`]) and the comparators that sort it ([`sort`]), the async
//! copy/move op engine and in-app clipboard ([`ops`], Stage 8), the
//! hand-rolled freedesktop Trash implementation ([`trash`], Stage 9), and
//! the session undo stack ([`undo`], Stage 10). No blocking disk I/O here
//! directly, with one documented exception — `src/modules/` is where a
//! `FileEntry` gets built from a real backend, and `ops`/`undo` only ever
//! reach disk through the `Backend` trait, same as everything else in this
//! crate; `trash` is the second, deliberate exception to that rule (see
//! its own module doc comment for the full reasoning — the short version:
//! freedesktop Trash is inherently a local-filesystem concept with no sane
//! multi-backend abstraction, the `std::fs` counterpart to
//! `core/thumbs.rs`'s documented `iced` exception). `undo` reaches into
//! `trash` for one call (`trash::restore`), never `std::fs` directly.

pub mod entry;
pub mod ops;
pub mod sort;
pub mod trash;
pub mod undo;
