//! Pure comparators for ordering `FileEntry` rows: natural-order name
//! comparison plus dirs-first grouping, shared by every sort key. No I/O,
//! no iced — just [`std::cmp::Ordering`], unit-tested inline.
//!
//! Reuses [`crate::config::SortKey`] rather than defining a parallel enum:
//! `config.rs` has no iced dependency either, so a directory view's `sort`
//! field, `files.toml`'s knob, and these comparators all speak the same
//! type end to end.

use std::cmp::Ordering;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::config::SortKey;
use crate::core::fs::entry::{EntryKind, FileEntry};

/// Byte-wise "natural order" comparison: runs of ASCII digits compare by
/// numeric value (so `"img2"` sorts before `"img10"`), everything else
/// compares byte-for-byte. Operates directly on raw bytes — never
/// `to_string_lossy` — so non-UTF-8 names still sort deterministically;
/// see CLAUDE.md's OsString discipline.
///
/// Never indexes `a`/`b` directly (CLAUDE.md's no-panic rule bans
/// indexing on any runtime path): every byte comes from `.get()` or a
/// `split_at`-style helper that hands back an empty slice rather than
/// panicking, even though the loop bounds happen to make an out-of-range
/// access unreachable here too.
pub fn natural_cmp(mut a: &[u8], mut b: &[u8]) -> Ordering {
    loop {
        let (Some(&ca), Some(&cb)) = (a.first(), b.first()) else {
            return a.len().cmp(&b.len());
        };
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let (a_run, a_rest) = split_digit_run(a);
            let (b_run, b_rest) = split_digit_run(b);
            let a_val = trim_leading_zeros(a_run);
            let b_val = trim_leading_zeros(b_run);
            // Numeric value first (length then bytes, since equal-length
            // digit runs with no leading zeros compare the same
            // byte-for-byte as numerically); a run with more leading
            // zeros is the tiebreak loser ("2" sorts before "02").
            match a_val.len().cmp(&b_val.len()).then_with(|| a_val.cmp(b_val)) {
                Ordering::Equal => match a_run.len().cmp(&b_run.len()) {
                    Ordering::Equal => {
                        a = a_rest;
                        b = b_rest;
                    }
                    other => return other,
                },
                other => return other,
            }
        } else if ca != cb {
            return ca.cmp(&cb);
        } else {
            a = a.get(1..).unwrap_or(&[]);
            b = b.get(1..).unwrap_or(&[]);
        }
    }
}

/// Split off the leading run of ASCII digits: `(digits, rest)`. Uses
/// `iter().take_while(...).count()` plus `split_at` (which never panics —
/// the count it's fed is always `<= slice.len()` by construction) rather
/// than tracking a manually-incremented index into the slice.
fn split_digit_run(slice: &[u8]) -> (&[u8], &[u8]) {
    let run_len = slice.iter().take_while(|b| b.is_ascii_digit()).count();
    slice.split_at(run_len)
}

/// A run of ASCII digits with its leading zeros stripped, keeping exactly
/// one digit if the whole run is zeros (`"000"` -> `"0"`).
fn trim_leading_zeros(run: &[u8]) -> &[u8] {
    match run.iter().position(|&b| b != b'0') {
        Some(idx) => run.get(idx..).unwrap_or(&[]),
        None => run.get(run.len().saturating_sub(1)..).unwrap_or(&[]),
    }
}

fn name_bytes(entry: &FileEntry) -> &[u8] {
    entry.name.as_bytes()
}

/// Lowercase-insensitive-free extension bytes (`"tar.gz"` -> `"gz"`, no
/// dot) for [`SortKey::Type`]; entries with no extension sort together at
/// the front via the empty slice.
fn extension_bytes(entry: &FileEntry) -> &[u8] {
    Path::new(&entry.name)
        .extension()
        .map(OsStrExt::as_bytes)
        .unwrap_or(&[])
}

/// Directories rank before everything else, in every sort key and both
/// directions — dirs-first is a grouping, not something "descending"
/// reverses.
fn dir_rank(entry: &FileEntry) -> u8 {
    match entry.kind {
        EntryKind::Directory => 0,
        _ => 1,
    }
}

/// Order two entries per `key`/`descending`, dirs always first.
pub fn compare(a: &FileEntry, b: &FileEntry, key: SortKey, descending: bool) -> Ordering {
    let dir_order = dir_rank(a).cmp(&dir_rank(b));
    if dir_order != Ordering::Equal {
        return dir_order;
    }

    let ord = match key {
        SortKey::Name => natural_cmp(name_bytes(a), name_bytes(b)),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Modified => a.modified.cmp(&b.modified),
        SortKey::Type => extension_bytes(a)
            .cmp(extension_bytes(b))
            .then_with(|| natural_cmp(name_bytes(a), name_bytes(b))),
    };

    if descending { ord.reverse() } else { ord }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn file(name: &str, size: u64, modified: Option<u64>) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size,
            modified: modified
                .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            is_symlink: false,
        }
    }

    fn dir(name: &str) -> FileEntry {
        FileEntry {
            name: OsString::from(name),
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            is_symlink: false,
        }
    }

    #[test]
    fn natural_order_compares_digit_runs_numerically() {
        assert_eq!(natural_cmp(b"img2.png", b"img10.png"), Ordering::Less);
        assert_eq!(natural_cmp(b"img10.png", b"img2.png"), Ordering::Greater);
        assert_eq!(natural_cmp(b"a", b"a"), Ordering::Equal);
        assert_eq!(natural_cmp(b"a1", b"a01"), Ordering::Less); // "1" < "01": fewer leading zeros wins
        assert_eq!(natural_cmp(b"a", b"ab"), Ordering::Less); // shorter prefix sorts first
        assert_eq!(natural_cmp(b"file000", b"file0"), Ordering::Greater); // longer run of zeros
    }

    #[test]
    fn dirs_always_sort_first_regardless_of_key_or_direction() {
        let entries = [file("aaa.txt", 1, None), dir("zzz")];
        assert_eq!(
            compare(&entries[0], &entries[1], SortKey::Name, false),
            Ordering::Greater
        );
        assert_eq!(
            compare(&entries[0], &entries[1], SortKey::Name, true),
            Ordering::Greater
        );
        assert_eq!(
            compare(&entries[0], &entries[1], SortKey::Size, true),
            Ordering::Greater
        );
    }

    #[test]
    fn size_ascending_and_descending() {
        let small = file("a", 10, None);
        let big = file("b", 1000, None);
        assert_eq!(compare(&small, &big, SortKey::Size, false), Ordering::Less);
        assert_eq!(
            compare(&small, &big, SortKey::Size, true),
            Ordering::Greater
        );
    }

    #[test]
    fn modified_orders_unknown_times_consistently() {
        let unknown = file("a", 0, None);
        let known = file("b", 0, Some(1000));
        // `Option<SystemTime>`'s derived `Ord` puts `None` first; whichever
        // way that falls, ascending/descending must disagree with each
        // other on the same pair.
        let ascending = compare(&unknown, &known, SortKey::Modified, false);
        let descending = compare(&unknown, &known, SortKey::Modified, true);
        assert_ne!(ascending, descending);
    }

    #[test]
    fn type_sort_groups_by_extension_then_name() {
        let a = file("b.txt", 0, None);
        let b = file("a.txt", 0, None);
        let c = file("a.zip", 0, None);
        assert_eq!(compare(&b, &a, SortKey::Type, false), Ordering::Less); // same ext, name breaks tie
        assert_eq!(compare(&a, &c, SortKey::Type, false), Ordering::Less); // "txt" < "zip"
    }
}
