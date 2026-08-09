//! Pure, iced-free directory-listing data and ordering: the model
//! ([`entry`]) and the comparators that sort it ([`sort`]), plus the async
//! copy/move op engine and in-app clipboard ([`ops`], Stage 8). No blocking
//! disk I/O here directly — `src/modules/` is where a `FileEntry` gets
//! built from a real backend, and `ops` only ever reaches disk through the
//! `Backend` trait, same as everything else in this crate.

pub mod entry;
pub mod ops;
pub mod sort;
