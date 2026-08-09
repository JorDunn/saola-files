//! Pure, iced-free directory-listing data and ordering: the model
//! ([`entry`]) and the comparators that sort it ([`sort`]). No I/O here —
//! `src/modules/` is where a `FileEntry` gets built from a real backend.

pub mod entry;
pub mod sort;
