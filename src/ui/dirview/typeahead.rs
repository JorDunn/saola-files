//! Type-ahead ("type to select"): typing printable characters while a
//! directory view has keyboard focus jumps the cursor to the first visible
//! entry whose name starts with what's been typed, case-insensitively —
//! the classic Explorer/Nautilus/Finder "type to select" behavior, not a
//! search box.
//!
//! **Signal, never poll** (CLAUDE.md): the buffer's timeout is a
//! *timestamp comparison made at the next keypress*, not a ticking clock —
//! nothing subscribes to a timer, and a stale buffer just sits there
//! (invisible; there's no on-screen indicator to update) until either a
//! fresh keystroke arrives and resets it, or [`TypeAhead::clear`] is
//! called explicitly (`DirectoryView` does this on every navigation, the
//! same way it resets selection/scroll).

use std::ffi::OsStr;
use std::time::{Duration, Instant};

/// How long a gap between keystrokes may be before the buffer restarts
/// rather than extends. 900ms sits in the same 700–1200ms range every
/// mainstream file manager's type-ahead uses.
const TIMEOUT: Duration = Duration::from_millis(900);

/// The accumulated "what's been typed recently" buffer plus a timestamp of
/// the last keystroke, so [`TypeAhead::feed`] can tell a fast second
/// keystroke (extend the search) from a stale one (start over).
#[derive(Debug, Default)]
pub struct TypeAhead {
    buffer: String,
    last_key_at: Option<Instant>,
}

impl TypeAhead {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the buffer — called on every navigation (a fresh directory has
    /// nothing to do with whatever was being typed in the last one).
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.last_key_at = None;
    }

    /// Feed one typed character at `now` and search `names` (in `visible`
    /// order) for the first whose display name starts with the resulting
    /// buffer, case-insensitively.
    ///
    /// If `now` is within [`TIMEOUT`] of the last keystroke, this first
    /// tries *extending* the existing buffer with `ch`; if that extended
    /// buffer doesn't match anything, it falls back to restarting the
    /// search from `ch` alone (typing "do" then a stray "z" with nothing
    /// matching "doz" starts over at "z", the same way Explorer does,
    /// rather than getting stuck matching nothing until the timeout).
    /// Returns the matched index into `names`, if any.
    pub fn feed<'a>(
        &mut self,
        ch: char,
        now: Instant,
        names: impl Iterator<Item = &'a OsStr> + Clone,
    ) -> Option<usize> {
        let is_fresh = match self.last_key_at {
            // `saturating_duration_since` rather than `duration_since`:
            // this is a wall-clock read at keypress time, not a value this
            // module controls the monotonicity of end-to-end, and the
            // no-panic rule bans a subtraction that could underflow on any
            // runtime path — worst case a non-monotonic clock reads as
            // "no time passed", which just means "extend", the safe
            // default.
            Some(last) => now.saturating_duration_since(last) > TIMEOUT,
            None => true,
        };
        self.last_key_at = Some(now);

        if !is_fresh {
            let mut extended = self.buffer.clone();
            extended.push(ch);
            if let Some(index) = find_match(names.clone(), &extended) {
                self.buffer = extended;
                return Some(index);
            }
        }

        self.buffer.clear();
        self.buffer.push(ch);
        find_match(names, &self.buffer)
    }

    #[cfg(test)]
    fn buffer(&self) -> &str {
        &self.buffer
    }
}

/// First name in `names` starting with `buffer`, case-insensitively.
/// `to_string_lossy`/`to_lowercase` here is the same sanctioned
/// display-time conversion `FileEntry::display_name` uses — type-ahead
/// matching is inherently a human-facing text operation, not a byte-exact
/// one (CLAUDE.md's OsString discipline governs storage/comparison
/// identity, not "what should typing look for").
fn find_match<'a>(names: impl Iterator<Item = &'a OsStr>, buffer: &str) -> Option<usize> {
    let needle = buffer.to_lowercase();
    names
        .enumerate()
        .find(|(_, name)| name.to_string_lossy().to_lowercase().starts_with(&needle))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn names(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn as_refs(values: &[OsString]) -> impl Iterator<Item = &OsStr> + Clone {
        values.iter().map(OsString::as_os_str)
    }

    #[test]
    fn a_single_keystroke_matches_a_prefix_case_insensitively() {
        let mut ta = TypeAhead::new();
        let entries = names(&["Alpha", "beta", "Gamma"]);
        let now = Instant::now();
        assert_eq!(ta.feed('g', now, as_refs(&entries)), Some(2));
        assert_eq!(ta.buffer(), "g");
    }

    #[test]
    fn quick_successive_keystrokes_extend_the_buffer() {
        let mut ta = TypeAhead::new();
        let entries = names(&["document.txt", "downloads", "draft.md"]);
        let t0 = Instant::now();
        assert_eq!(ta.feed('d', t0, as_refs(&entries)), Some(0));
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(ta.feed('o', t1, as_refs(&entries)), Some(0));
        assert_eq!(ta.buffer(), "do");
        let t2 = t1 + Duration::from_millis(200);
        assert_eq!(ta.feed('w', t2, as_refs(&entries)), Some(1)); // "dow" -> "downloads"
        assert_eq!(ta.buffer(), "dow");
    }

    #[test]
    fn a_stale_gap_restarts_the_buffer_instead_of_extending() {
        let mut ta = TypeAhead::new();
        let entries = names(&["alpha", "beta"]);
        let t0 = Instant::now();
        assert_eq!(ta.feed('a', t0, as_refs(&entries)), Some(0));
        let t1 = t0 + TIMEOUT + Duration::from_millis(1);
        // Too slow to extend "a" into "ab" — starts over at "b" alone.
        assert_eq!(ta.feed('b', t1, as_refs(&entries)), Some(1));
        assert_eq!(ta.buffer(), "b");
    }

    #[test]
    fn an_extension_that_matches_nothing_restarts_from_the_new_character() {
        let mut ta = TypeAhead::new();
        let entries = names(&["document.txt", "zzz"]);
        let t0 = Instant::now();
        assert_eq!(ta.feed('d', t0, as_refs(&entries)), Some(0));
        let t1 = t0 + Duration::from_millis(200);
        // "dz" matches nothing; falls back to just "z".
        assert_eq!(ta.feed('z', t1, as_refs(&entries)), Some(1));
        assert_eq!(ta.buffer(), "z");
    }

    #[test]
    fn no_match_anywhere_returns_none_and_still_records_the_buffer() {
        let mut ta = TypeAhead::new();
        let entries = names(&["alpha", "beta"]);
        assert_eq!(ta.feed('q', Instant::now(), as_refs(&entries)), None);
        assert_eq!(ta.buffer(), "q");
    }

    #[test]
    fn clear_drops_the_buffer_and_forces_a_restart() {
        let mut ta = TypeAhead::new();
        let entries = names(&["alpha", "beta"]);
        let t0 = Instant::now();
        let _ = ta.feed('a', t0, as_refs(&entries));
        ta.clear();
        assert_eq!(ta.buffer(), "");
        // Even immediately after `clear`, this reads as a fresh start.
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(ta.feed('b', t1, as_refs(&entries)), Some(1));
        assert_eq!(ta.buffer(), "b");
    }
}
