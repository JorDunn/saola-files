//! Bulk rename: the rename *plan* (old name -> new name for every selected
//! item, with validation), the safe *order* to carry it out in, and the
//! executor that runs it through a [`Backend`] and prepares its undo.
//!
//! Everything above [`apply`] is pure: no I/O, no iced, so the hard parts are
//! unit-tested directly.
//!
//! # Names are bytes (AGENTS.md "OsString discipline")
//!
//! A filename on Unix is a byte string, not necessarily UTF-8. Every
//! transform here works on those bytes (through `OsStrExt`/`OsStringExt`), so
//! **the bytes outside the part a transform edits are always preserved**:
//!
//! - find/replace (literal or regex) uses `regex::bytes`, which searches a
//!   `&[u8]` and copies unmatched bytes through untouched;
//! - prefix/suffix and numbering concatenate bytes;
//! - case changes walk the name in valid-UTF-8 chunks
//!   (`<[u8]>::utf8_chunks`), change the case of each valid chunk and copy
//!   the invalid bytes through verbatim.
//!
//! So there is no "can't represent this name" refusal and no lossy
//! conversion: a plan never contains a name that is not exactly the bytes the
//! transform produced. The text the person *types* (pattern, find,
//! replacement) is UTF-8 because it comes from a text field; only the file
//! names are arbitrary bytes. `to_string_lossy` happens at view time, in the
//! dialog, for display only.
//!
//! # Validation (before anything is renamed)
//!
//! [`Plan::compute`] gives every row a [`Status`]:
//!
//! - `Unchanged`: the transform left the name alone. Skipped, not an error.
//! - `Invalid`: empty, `.`/`..`, contains `/` or NUL, or longer than
//!   [`NAME_MAX`] bytes.
//! - `Duplicate`: two rows would end up with the same name (an unchanged
//!   item keeps its name and counts as holding it).
//! - `Exists`: the new name is already taken by an entry that is *not* in
//!   the selection.
//! - `Ready`: will be renamed.
//!
//! A new name equal to another *selected* item's old name is fine: that is a
//! swap or a chain, handled by [`schedule`].
//!
//! # Order and cycles
//!
//! `rename(2)` silently replaces an existing file, so the order matters. Each
//! target name is unique and each source name is unique, so the moves form
//! disjoint chains (`a->b, b->c`: do `b->c` first) and cycles (`a->b, b->a`).
//! A cycle is broken by parking one item under a temporary name
//! (`.saola-tmp-N`, chosen so it collides with nothing in the folder or the
//! plan), moving the rest, then moving the parked item to its final name.
//! Every step also checks, right before it runs, that its target is free, so
//! a name that appears mid-batch is an error, never an overwrite.
//!
//! # Partial failure
//!
//! The executor stops at the first failed step and **rolls back** every step
//! it had completed (in reverse), so a failure normally leaves the folder
//! exactly as it was and there is nothing to undo. If a rollback step itself
//! fails, that item stays under whatever name it has at that moment;
//! [`ApplyReport::moved`] lists exactly the items that are not under their
//! original name (current name, original name), so the undo entry the caller
//! pushes restores precisely what changed, and [`ApplyReport::stuck`] says
//! which rollback steps failed.
//!
//! # Case-only renames
//!
//! Names are compared byte for byte, so `a.txt` -> `A.txt` is an ordinary
//! rename on a case-sensitive filesystem. On a case-insensitive one the
//! backend decides: if it refuses, the step fails and the whole batch rolls
//! back with the backend's own error. Not special-cased (out of scope).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use regex::bytes::{NoExpand, Regex, RegexBuilder};

use crate::core::vfs::{Backend, Location, VfsError};

/// The longest file name, in bytes, that Linux filesystems accept.
pub const NAME_MAX: usize = 255;

/// The widest zero padding `{n:W}` accepts. Far beyond any real batch; it
/// only exists so a typo like `{n:99999999}` can't allocate gigabytes.
const MAX_PAD_WIDTH: usize = 32;

// ── Settings ───────────────────────────────────────────────────────────────

/// Which part of the name a transform sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// The name without its extension (`photo` of `photo.jpg`); the
    /// extension is re-attached unchanged. The default: bulk renames almost
    /// never mean to touch `.jpg`.
    #[default]
    Stem,
    /// The whole name, extension included.
    FullName,
}

/// The new case for [`Transform::Case`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseKind {
    #[default]
    Lower,
    Upper,
    /// First letter of each word upper, the rest lower. Words are separated
    /// by whitespace, `_`, `-` or `.`.
    Title,
}

/// What to do to each name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transform {
    /// Replace every match of `find`. With `regex` off, `find` is literal
    /// text and `replace` is used as typed; with it on, `find` is a regular
    /// expression and `replace` may use `$1`/`${name}` groups.
    FindReplace {
        find: String,
        replace: String,
        regex: bool,
        match_case: bool,
    },
    /// Put `prefix` before and `suffix` after. With [`Scope::Stem`] the
    /// suffix lands before the extension.
    Affix { prefix: String, suffix: String },
    /// Build each name from `pattern`: `{n}` is the counter, `{n:03}` the
    /// counter zero-padded to 3 digits, `{name}` the item's old name (per
    /// scope), `{{`/`}}` literal braces. Item `i` (in the order the plan was
    /// given) gets `start + i`.
    Numbering { pattern: String, start: u64 },
    /// Change letter case.
    Case(CaseKind),
}

impl Default for Transform {
    fn default() -> Self {
        Transform::FindReplace {
            find: String::new(),
            replace: String::new(),
            regex: false,
            match_case: true,
        }
    }
}

/// Everything the dialog's controls say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    pub transform: Transform,
    pub scope: Scope,
}

/// Why the settings themselves can't produce names (shown beside the
/// control, in words, instead of per-row problems).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// The regular expression doesn't compile; carries the engine's text.
    BadRegex(String),
    /// A `{...}` in a numbering pattern isn't `{n}`, `{n:W}` or `{name}`.
    UnknownToken(String),
    /// A `{` with no closing `}`.
    UnclosedBrace,
    /// A lone `}` (write `}}` for a literal one).
    StrayBrace,
    /// `{n:W}` where W isn't a number from 1 to the maximum width.
    BadWidth(String),
}

// ── Name helpers (bytes) ───────────────────────────────────────────────────

/// Splits a name into (stem, extension including its dot). A dot at the very
/// start (a hidden file such as `.bashrc`) or no dot at all means no
/// extension. Only the last dot counts (`a.tar.gz` -> `a.tar` + `.gz`).
fn split_extension(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().rposition(|&b| b == b'.') {
        Some(0) | None => name.split_at(name.len()),
        Some(dot) => name.split_at(dot),
    }
}

/// Why a new name can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    Empty,
    /// `.` or `..`.
    DotName,
    /// Contains `/` (a name is one path component) or a NUL byte.
    BadCharacter,
    /// Longer than [`NAME_MAX`] bytes.
    TooLong,
}

fn check_name(name: &[u8]) -> Result<(), Invalid> {
    if name.is_empty() {
        Err(Invalid::Empty)
    } else if name == b"." || name == b".." {
        Err(Invalid::DotName)
    } else if name.iter().any(|&b| b == b'/' || b == 0) {
        Err(Invalid::BadCharacter)
    } else if name.len() > NAME_MAX {
        Err(Invalid::TooLong)
    } else {
        Ok(())
    }
}

// ── Transforms ─────────────────────────────────────────────────────────────

/// One piece of a parsed numbering pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(Vec<u8>),
    /// `{n}` / `{n:W}`; width 0 = no padding.
    Counter {
        width: usize,
    },
    /// `{name}`.
    OldName,
}

fn parse_pattern(pattern: &str) -> Result<Vec<Piece>, SettingsError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(SettingsError::StrayBrace),
            '{' => {
                let mut token = String::new();
                let mut closed = false;
                // `by_ref()` lends the iterator out so the outer `while let`
                // resumes after the closing brace.
                for t in chars.by_ref() {
                    if t == '}' {
                        closed = true;
                        break;
                    }
                    token.push(t);
                }
                if !closed {
                    return Err(SettingsError::UnclosedBrace);
                }
                if !literal.is_empty() {
                    pieces.push(Piece::Literal(std::mem::take(&mut literal).into_bytes()));
                }
                pieces.push(parse_token(&token)?);
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal.into_bytes()));
    }
    Ok(pieces)
}

fn parse_token(token: &str) -> Result<Piece, SettingsError> {
    if token == "name" {
        return Ok(Piece::OldName);
    }
    if token == "n" {
        return Ok(Piece::Counter { width: 0 });
    }
    if let Some(spec) = token.strip_prefix("n:") {
        return match spec.parse::<usize>() {
            Ok(width) if (1..=MAX_PAD_WIDTH).contains(&width) => Ok(Piece::Counter { width }),
            _ => Err(SettingsError::BadWidth(spec.to_owned())),
        };
    }
    Err(SettingsError::UnknownToken(token.to_owned()))
}

/// A transform with everything expensive (regex compile, pattern parse)
/// done once, ready to run on each name.
enum Compiled {
    /// Nothing typed yet: every name stays as it is.
    Nothing,
    Replace {
        re: Regex,
        /// True = the replacement is literal text (no `$` expansion).
        literal: bool,
        replacement: Vec<u8>,
    },
    Affix {
        prefix: Vec<u8>,
        suffix: Vec<u8>,
    },
    Numbering {
        pieces: Vec<Piece>,
        start: u64,
    },
    Case(CaseKind),
}

fn compile(transform: &Transform) -> Result<Compiled, SettingsError> {
    Ok(match transform {
        Transform::FindReplace {
            find,
            replace,
            regex,
            match_case,
        } => {
            if find.is_empty() {
                return Ok(Compiled::Nothing);
            }
            // A literal search is a regex of the escaped text, so both
            // modes share one byte-oriented engine and one code path.
            let source = if *regex {
                find.clone()
            } else {
                regex::escape(find)
            };
            let re = RegexBuilder::new(&source)
                .case_insensitive(!*match_case)
                .build()
                .map_err(|err| SettingsError::BadRegex(error_summary(&err.to_string())))?;
            Compiled::Replace {
                re,
                literal: !*regex,
                replacement: replace.clone().into_bytes(),
            }
        }
        Transform::Affix { prefix, suffix } => {
            if prefix.is_empty() && suffix.is_empty() {
                Compiled::Nothing
            } else {
                Compiled::Affix {
                    prefix: prefix.clone().into_bytes(),
                    suffix: suffix.clone().into_bytes(),
                }
            }
        }
        Transform::Numbering { pattern, start } => {
            if pattern.is_empty() {
                Compiled::Nothing
            } else {
                Compiled::Numbering {
                    pieces: parse_pattern(pattern)?,
                    start: *start,
                }
            }
        }
        Transform::Case(kind) => Compiled::Case(*kind),
    })
}

/// The regex crate's multi-line errors draw a diagram; the `error:` line is
/// the actual message ("error: unclosed group").
fn error_summary(text: &str) -> String {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("error:"))
        .map(|rest| rest.trim().to_owned())
        .unwrap_or_else(|| text.lines().next().unwrap_or_default().to_owned())
}

impl Compiled {
    /// The new text for the part of the name `part` (the stem or the whole
    /// name, per scope). `index` is the item's position, for numbering.
    fn run(&self, part: &[u8], index: usize) -> Vec<u8> {
        match self {
            Compiled::Nothing => part.to_vec(),
            Compiled::Replace {
                re,
                literal,
                replacement,
            } => {
                if *literal {
                    re.replace_all(part, NoExpand(replacement.as_slice()))
                        .into_owned()
                } else {
                    re.replace_all(part, replacement.as_slice()).into_owned()
                }
            }
            Compiled::Affix { prefix, suffix } => {
                let mut out = Vec::with_capacity(prefix.len() + part.len() + suffix.len());
                out.extend_from_slice(prefix);
                out.extend_from_slice(part);
                out.extend_from_slice(suffix);
                out
            }
            Compiled::Numbering { pieces, start } => {
                let counter = start.saturating_add(index as u64);
                let mut out = Vec::new();
                for piece in pieces {
                    match piece {
                        Piece::Literal(bytes) => out.extend_from_slice(bytes),
                        Piece::OldName => out.extend_from_slice(part),
                        Piece::Counter { width } => {
                            out.extend_from_slice(format!("{counter:0width$}").as_bytes());
                        }
                    }
                }
                out
            }
            Compiled::Case(kind) => change_case(part, *kind),
        }
    }
}

/// Case change that keeps invalid-UTF-8 bytes exactly as they are.
///
/// Teaching note: `utf8_chunks()` splits a byte slice into runs of valid
/// UTF-8 (`valid()`, a `&str`) each followed by a run of invalid bytes
/// (`invalid()`, a `&[u8]`). Case mapping only makes sense on the valid text,
/// so we map those and copy the invalid bytes straight through.
fn change_case(bytes: &[u8], kind: CaseKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    // Title case needs to remember, across chunk boundaries, whether the
    // next letter starts a word. Invalid bytes count as "inside a word".
    let mut at_word_start = true;
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        match kind {
            CaseKind::Lower => out.extend_from_slice(valid.to_lowercase().as_bytes()),
            CaseKind::Upper => out.extend_from_slice(valid.to_uppercase().as_bytes()),
            CaseKind::Title => {
                for c in valid.chars() {
                    let separator = c.is_whitespace() || matches!(c, '_' | '-' | '.');
                    let mapped: String = if at_word_start && !separator {
                        c.to_uppercase().collect()
                    } else {
                        c.to_lowercase().collect()
                    };
                    out.extend_from_slice(mapped.as_bytes());
                    at_word_start = separator;
                }
            }
        }
        if !chunk.invalid().is_empty() {
            out.extend_from_slice(chunk.invalid());
            at_word_start = false;
        }
    }
    out
}

/// The new name for `old` (equal to `old` when the settings leave it alone).
fn transform_name(compiled: &Compiled, scope: Scope, old: &OsStr, index: usize) -> OsString {
    let old_bytes = old.as_bytes();
    let (stem, ext) = match scope {
        Scope::Stem => split_extension(old_bytes),
        Scope::FullName => old_bytes.split_at(old_bytes.len()),
    };
    let mut new = compiled.run(stem, index);
    new.extend_from_slice(ext);
    OsString::from_vec(new)
}

// ── The plan ───────────────────────────────────────────────────────────────

/// What will happen to one selected item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The new name equals the old one: skipped.
    Unchanged,
    /// Will be renamed.
    Ready,
    /// The new name can't be used at all.
    Invalid(Invalid),
    /// Another item (selected, or unchanged and keeping its name) ends up
    /// with the same new name.
    Duplicate,
    /// The new name belongs to an existing entry that is not selected.
    Exists,
}

impl Status {
    /// True for the statuses that block Apply.
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            Status::Invalid(_) | Status::Duplicate | Status::Exists
        )
    }
}

/// One line of the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub old: OsString,
    pub new: OsString,
    pub status: Status,
}

/// The whole preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub rows: Vec<Row>,
    /// Set when the settings themselves are unusable (a bad regex): every
    /// row is then `Unchanged` and Apply is off.
    pub error: Option<SettingsError>,
}

impl Plan {
    /// Computes the plan.
    ///
    /// - `selected`: the names to rename, **in the order the view shows
    ///   them**; numbering follows this order.
    /// - `others`: every other name in the folder, so a collision with an
    ///   unselected entry is caught.
    pub fn compute(selected: &[OsString], others: &HashSet<OsString>, settings: &Settings) -> Plan {
        let compiled = match compile(&settings.transform) {
            Ok(compiled) => compiled,
            Err(error) => {
                return Plan {
                    rows: selected
                        .iter()
                        .map(|name| Row {
                            old: name.clone(),
                            new: name.clone(),
                            status: Status::Unchanged,
                        })
                        .collect(),
                    error: Some(error),
                };
            }
        };

        let mut rows: Vec<Row> = selected
            .iter()
            .enumerate()
            .map(|(index, old)| {
                let new = transform_name(&compiled, settings.scope, old, index);
                let status = if new == *old {
                    Status::Unchanged
                } else {
                    match check_name(new.as_bytes()) {
                        Ok(()) => Status::Ready,
                        Err(why) => Status::Invalid(why),
                    }
                };
                Row {
                    old: old.clone(),
                    new,
                    status,
                }
            })
            .collect();

        // How many rows claim each final name. An unchanged row claims its
        // own name; an invalid row claims nothing (it can't be written).
        let mut claims: HashMap<&OsStr, usize> = HashMap::new();
        for row in &rows {
            match row.status {
                Status::Unchanged => *claims.entry(row.old.as_os_str()).or_default() += 1,
                Status::Ready => *claims.entry(row.new.as_os_str()).or_default() += 1,
                _ => {}
            }
        }
        // Owned copies, so the borrow of `rows` ends before we edit it.
        let duplicated: HashSet<OsString> = claims
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(name, _)| (*name).to_owned())
            .collect();

        for row in &mut rows {
            if row.status != Status::Ready {
                continue;
            }
            if duplicated.contains(&row.new) {
                row.status = Status::Duplicate;
            } else if others.contains(&row.new) {
                row.status = Status::Exists;
            }
        }

        Plan { rows, error: None }
    }

    /// How many items will actually be renamed.
    pub fn change_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Ready)
            .count()
    }

    /// How many rows have a problem that blocks Apply.
    pub fn problem_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status.is_problem())
            .count()
    }

    /// The first blocking row, for the one-line reason beside Apply.
    pub fn first_problem(&self) -> Option<&Row> {
        self.rows.iter().find(|row| row.status.is_problem())
    }

    /// Apply is allowed: usable settings, at least one change, no problems.
    pub fn can_apply(&self) -> bool {
        self.error.is_none() && self.change_count() > 0 && self.problem_count() == 0
    }

    /// The `(old, new)` pairs to carry out, skipping unchanged rows.
    pub fn moves(&self) -> Vec<(OsString, OsString)> {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Ready)
            .map(|row| (row.old.clone(), row.new.clone()))
            .collect()
    }
}

// ── Ordering ───────────────────────────────────────────────────────────────

/// One `rename` call: names within the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub from: OsString,
    pub to: OsString,
}

/// Hands out temporary names that collide with nothing.
struct TempNames {
    forbidden: HashSet<OsString>,
    counter: u64,
}

impl TempNames {
    fn new(taken: &HashSet<OsString>, moves: &[(OsString, OsString)]) -> Self {
        let mut forbidden = taken.clone();
        for (from, to) in moves {
            forbidden.insert(from.clone());
            forbidden.insert(to.clone());
        }
        TempNames {
            forbidden,
            counter: 0,
        }
    }

    fn next(&mut self) -> OsString {
        loop {
            self.counter += 1;
            let candidate = OsString::from(format!(".saola-tmp-{}", self.counter));
            // `insert` returns true when the name was not already present.
            if self.forbidden.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

/// Orders `moves` (`(from, to)` name pairs) into steps that never rename
/// onto a name that is still occupied, parking one item under a temporary
/// name for each cycle. `taken` is every name currently in the folder (used
/// only to pick temp names that don't collide).
///
/// Requires unique `from`s and unique `to`s, which [`Plan`] validation
/// guarantees. Identity moves (`from == to`) are dropped.
pub fn schedule(moves: &[(OsString, OsString)], taken: &HashSet<OsString>) -> Vec<Step> {
    let moves: Vec<(OsString, OsString)> = moves
        .iter()
        .filter(|(from, to)| from != to)
        .cloned()
        .collect();
    let by_from: HashMap<&OsStr, usize> = moves
        .iter()
        .enumerate()
        .map(|(i, (from, _))| (from.as_os_str(), i))
        .collect();
    // The move that must happen *before* move `i`: the one whose source is
    // the name `i` wants to take over.
    let blocker = |i: usize| -> Option<usize> {
        moves
            .get(i)
            .and_then(|(_, to)| by_from.get(to.as_os_str()).copied())
    };
    let step = |i: usize| -> Option<Step> {
        moves.get(i).map(|(from, to)| Step {
            from: from.clone(),
            to: to.clone(),
        })
    };

    let mut temps = TempNames::new(taken, &moves);
    let mut done: HashSet<usize> = HashSet::new();
    let mut steps = Vec::new();

    for start in 0..moves.len() {
        if done.contains(&start) {
            continue;
        }
        // Follow "must go first" links until something is free to move.
        let mut path = vec![start];
        let mut is_cycle = false;
        while let Some(&last) = path.last() {
            match blocker(last) {
                None => break,
                // That one already moved away, so its old name is free.
                Some(next) if done.contains(&next) => break,
                Some(next) if next == start => {
                    is_cycle = true;
                    break;
                }
                // Defensive: a repeat that is not `start` means duplicate
                // targets, which validation rules out. Stop rather than
                // loop forever; `apply`'s per-step check still refuses to
                // overwrite anything.
                Some(next) if path.contains(&next) => break,
                Some(next) => path.push(next),
            }
        }

        if is_cycle {
            // Park the first item, run the rest back to front, then land it.
            let Some((head, rest)) = path.split_first() else {
                continue;
            };
            let Some(first) = step(*head) else { continue };
            let temp = temps.next();
            steps.push(Step {
                from: first.from,
                to: temp.clone(),
            });
            for &i in rest.iter().rev() {
                steps.extend(step(i));
                done.insert(i);
            }
            steps.push(Step {
                from: temp,
                to: first.to,
            });
            done.insert(*head);
        } else {
            // The far end of the chain is free: move it first, then back up.
            for &i in path.iter().rev() {
                steps.extend(step(i));
                done.insert(i);
            }
        }
    }
    steps
}

// ── Executing ──────────────────────────────────────────────────────────────

/// A step that failed, named for the person (the item's original name and
/// final name, never a temporary one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub name: OsString,
    pub target: OsString,
    pub error: VfsError,
}

/// What [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Items now under a name other than their original, as
    /// `(current name, original name)`. After a success: every item. After a
    /// failure that rolled back cleanly: empty. This is exactly what an undo
    /// entry must restore.
    pub moved: Vec<(OsString, OsString)>,
    /// The step that stopped the batch, if any.
    pub failure: Option<StepFailure>,
    /// Rollback steps that failed (those items stay where they ended up and
    /// are listed in `moved`).
    pub stuck: Vec<StepFailure>,
}

/// Renames `step.from` to `step.to` in `dir`, refusing to land on a name
/// that exists (`rename(2)` would silently replace it).
async fn rename_step(backend: &dyn Backend, dir: &Location, step: &Step) -> Result<(), VfsError> {
    let to = dir.join(&step.to);
    // Only a successful stat means "occupied". NotFound is the expected
    // answer; any other error is left for `rename` to report in its own words.
    if backend.metadata(&to).await.is_ok() {
        return Err(VfsError::AlreadyExists {
            location: to.to_string(),
        });
    }
    backend.rename(&dir.join(&step.from), &to).await
}

/// Checks the plan against the folder as it is *now* (it may have changed
/// since the dialog opened): every source must still exist and every target
/// must be free or be another item's source.
fn preflight(
    dir: &Location,
    moves: &[(OsString, OsString)],
    present: &HashSet<OsString>,
) -> Option<StepFailure> {
    let sources: HashSet<&OsStr> = moves.iter().map(|(from, _)| from.as_os_str()).collect();
    for (from, to) in moves {
        let error = if !present.contains(from) {
            VfsError::NotFound {
                location: dir.join(from).to_string(),
            }
        } else if present.contains(to) && !sources.contains(to.as_os_str()) {
            VfsError::AlreadyExists {
                location: dir.join(to).to_string(),
            }
        } else {
            continue;
        };
        return Some(StepFailure {
            name: from.clone(),
            target: to.clone(),
            error,
        });
    }
    None
}

/// Carries out `moves` (`(old, new)` names in `dir`) as one batch: check,
/// order ([`schedule`]), run, and on the first failure roll back. See the
/// module docs, "Partial failure".
pub async fn apply(
    backend: &dyn Backend,
    dir: &Location,
    moves: &[(OsString, OsString)],
) -> ApplyReport {
    if moves.is_empty() {
        return ApplyReport::default();
    }
    let present: HashSet<OsString> = match backend.list(dir).await {
        Ok(entries) => entries.into_iter().map(|entry| entry.name).collect(),
        Err(error) => {
            let (name, target) = moves.first().cloned().unwrap_or_default();
            return ApplyReport {
                failure: Some(StepFailure {
                    name,
                    target,
                    error,
                }),
                ..ApplyReport::default()
            };
        }
    };
    if let Some(failure) = preflight(dir, moves, &present) {
        return ApplyReport {
            failure: Some(failure),
            ..ApplyReport::default()
        };
    }

    let steps = schedule(moves, &present);
    // Where every item is right now: (current name, original name).
    let mut state: Vec<(OsString, OsString)> = moves
        .iter()
        .map(|(from, _)| (from.clone(), from.clone()))
        .collect();
    let final_name = |original: &OsStr| -> OsString {
        moves
            .iter()
            .find(|(from, _)| from == original)
            .map(|(_, to)| to.clone())
            .unwrap_or_default()
    };

    let mut executed: Vec<&Step> = Vec::new();
    let mut failure = None;
    for step in &steps {
        match rename_step(backend, dir, step).await {
            Ok(()) => {
                executed.push(step);
                retarget(&mut state, &step.from, &step.to);
            }
            Err(error) => {
                let original = original_of(&state, &step.from);
                failure = Some(StepFailure {
                    target: final_name(&original),
                    name: original,
                    error,
                });
                break;
            }
        }
    }

    let mut stuck = Vec::new();
    if failure.is_some() {
        // Undo what this batch did, newest first. A raw rename is enough:
        // each reverse step puts a name back on a spot its forward step just
        // vacated, so it cannot collide.
        for step in executed.iter().rev() {
            let back = backend
                .rename(&dir.join(&step.to), &dir.join(&step.from))
                .await;
            match back {
                Ok(()) => retarget(&mut state, &step.to, &step.from),
                Err(error) => {
                    let original = original_of(&state, &step.to);
                    stuck.push(StepFailure {
                        target: final_name(&original),
                        name: original,
                        error,
                    });
                }
            }
        }
    }

    ApplyReport {
        moved: state
            .into_iter()
            .filter(|(current, original)| current != original)
            .collect(),
        failure,
        stuck,
    }
}

/// The item currently called `current` is now called `new`.
fn retarget(state: &mut [(OsString, OsString)], current: &OsStr, new: &OsStr) {
    for (name, _) in state.iter_mut() {
        if name == current {
            *name = new.to_owned();
        }
    }
}

/// The original name of the item currently called `current`.
fn original_of(state: &[(OsString, OsString)], current: &OsStr) -> OsString {
    state
        .iter()
        .find(|(name, _)| name == current)
        .map(|(_, original)| original.clone())
        .unwrap_or_else(|| current.to_owned())
}

// ── Undo ───────────────────────────────────────────────────────────────────

/// Why an item can't be put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The renamed item is no longer there (deleted or renamed again).
    Missing,
    /// Something else now has the original name.
    Occupied,
}

/// What an undo will do: the moves still safe to make, and the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    /// `(current name, original name)` pairs to restore.
    pub moves: Vec<(OsString, OsString)>,
    /// `(current name, why)` for items left alone.
    pub skipped: Vec<(OsString, SkipReason)>,
}

/// Decides which of an entry's `(current, original)` pairs can be restored
/// given the folder's `present` names now. A pair is dropped when its item is
/// gone, or when its original name is held by something that is not itself
/// being restored. Dropping one pair can strand another (its target was held
/// by the dropped item), so this repeats until nothing changes.
pub fn plan_undo(moves: &[(OsString, OsString)], present: &HashSet<OsString>) -> UndoPlan {
    let mut kept: Vec<(OsString, OsString)> = moves.to_vec();
    let mut skipped = Vec::new();
    loop {
        let sources: HashSet<OsString> = kept.iter().map(|(current, _)| current.clone()).collect();
        let (ok, bad): (Vec<_>, Vec<_>) = kept.into_iter().partition(|(current, original)| {
            present.contains(current) && (!present.contains(original) || sources.contains(original))
        });
        for (current, _) in &bad {
            let reason = if present.contains(current) {
                SkipReason::Occupied
            } else {
                SkipReason::Missing
            };
            skipped.push((current.clone(), reason));
        }
        kept = ok;
        if bad.is_empty() {
            break;
        }
    }
    UndoPlan {
        moves: kept,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fs::entry::{EntryKind, FileEntry};
    use crate::core::vfs::{Caps, DirEvent, ReadStream, WriteSink};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::Mutex;
    use std::time::SystemTime;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(|s| os(s)).collect()
    }

    fn set(list: &[&str]) -> HashSet<OsString> {
        list.iter().map(|s| os(s)).collect()
    }

    fn bytes_name(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }

    fn settings(transform: Transform) -> Settings {
        Settings {
            transform,
            scope: Scope::Stem,
        }
    }

    fn full_name(transform: Transform) -> Settings {
        Settings {
            transform,
            scope: Scope::FullName,
        }
    }

    fn find_replace(find: &str, replace: &str, regex: bool) -> Transform {
        Transform::FindReplace {
            find: find.to_owned(),
            replace: replace.to_owned(),
            regex,
            match_case: true,
        }
    }

    fn new_names(plan: &Plan) -> Vec<OsString> {
        plan.rows.iter().map(|row| row.new.clone()).collect()
    }

    // ── extension splitting ─────────────────────────────────────────────

    #[test]
    fn extension_splits_on_the_last_dot_but_not_a_leading_one() {
        assert_eq!(split_extension(b"photo.jpg"), (&b"photo"[..], &b".jpg"[..]));
        assert_eq!(split_extension(b"a.tar.gz"), (&b"a.tar"[..], &b".gz"[..]));
        assert_eq!(split_extension(b".bashrc"), (&b".bashrc"[..], &b""[..]));
        assert_eq!(split_extension(b"README"), (&b"README"[..], &b""[..]));
    }

    // ── transforms ──────────────────────────────────────────────────────

    #[test]
    fn literal_find_replace_keeps_the_extension() {
        let plan = Plan::compute(
            &names(&["IMG_1.jpg", "IMG_2.jpg"]),
            &HashSet::new(),
            &settings(find_replace("IMG", "Holiday", false)),
        );
        assert_eq!(new_names(&plan), names(&["Holiday_1.jpg", "Holiday_2.jpg"]));
        assert!(plan.can_apply());
    }

    #[test]
    fn full_name_scope_can_edit_the_extension() {
        let plan = Plan::compute(
            &names(&["a.jpeg"]),
            &HashSet::new(),
            &full_name(find_replace(".jpeg", ".jpg", false)),
        );
        assert_eq!(new_names(&plan), names(&["a.jpg"]));
    }

    #[test]
    fn literal_mode_does_not_interpret_regex_or_dollar_signs() {
        let plan = Plan::compute(
            &names(&["a.b"]),
            &HashSet::new(),
            &full_name(find_replace(".", "$1", false)),
        );
        // `.` is a literal dot, and `$1` is written out as typed.
        assert_eq!(new_names(&plan), names(&["a$1b"]));
    }

    #[test]
    fn regex_mode_supports_groups() {
        let plan = Plan::compute(
            &names(&["2024-05-01 trip.jpg"]),
            &HashSet::new(),
            &settings(find_replace(r"(\d+)-(\d+)-(\d+)", "$3.$2.$1", true)),
        );
        assert_eq!(new_names(&plan), names(&["01.05.2024 trip.jpg"]));
    }

    #[test]
    fn case_insensitive_find() {
        let plan = Plan::compute(
            &names(&["Photo.png"]),
            &HashSet::new(),
            &settings(Transform::FindReplace {
                find: "photo".into(),
                replace: "pic".into(),
                regex: false,
                match_case: false,
            }),
        );
        assert_eq!(new_names(&plan), names(&["pic.png"]));
    }

    #[test]
    fn a_bad_regex_is_a_settings_error_not_a_panic() {
        let plan = Plan::compute(
            &names(&["a"]),
            &HashSet::new(),
            &settings(find_replace("(", "x", true)),
        );
        assert!(matches!(plan.error, Some(SettingsError::BadRegex(_))));
        assert!(!plan.can_apply());
        assert_eq!(plan.rows[0].status, Status::Unchanged);
    }

    #[test]
    fn nothing_typed_changes_nothing() {
        let plan = Plan::compute(&names(&["a", "b"]), &HashSet::new(), &Settings::default());
        assert!(plan.rows.iter().all(|r| r.status == Status::Unchanged));
        assert_eq!(plan.error, None);
        assert!(!plan.can_apply());
    }

    #[test]
    fn prefix_and_suffix_put_the_suffix_before_the_extension() {
        let plan = Plan::compute(
            &names(&["a.txt", "b"]),
            &HashSet::new(),
            &settings(Transform::Affix {
                prefix: "old_".into(),
                suffix: "_v2".into(),
            }),
        );
        assert_eq!(new_names(&plan), names(&["old_a_v2.txt", "old_b_v2"]));
    }

    #[test]
    fn case_changes() {
        let run = |kind, name: &str| {
            let plan = Plan::compute(
                &names(&[name]),
                &HashSet::new(),
                &settings(Transform::Case(kind)),
            );
            plan.rows[0].new.clone()
        };
        assert_eq!(run(CaseKind::Upper, "my file.txt"), os("MY FILE.txt"));
        assert_eq!(run(CaseKind::Lower, "MY File.TXT"), os("my file.TXT"));
        assert_eq!(
            run(CaseKind::Title, "my holiday_photo-1.jpg"),
            os("My Holiday_Photo-1.jpg")
        );
        // Non-ASCII letters change case too.
        assert_eq!(run(CaseKind::Upper, "straße é.txt"), os("STRASSE É.txt"));
    }

    // ── numbering ───────────────────────────────────────────────────────

    #[test]
    fn numbering_twenty_files_pads_and_follows_the_given_order() {
        // The names sort in the opposite order to the list: numbering must
        // follow the order it is given (the view's), not the names.
        let selected: Vec<OsString> = (0..20)
            .map(|i| os(&format!("IMG_{:04}.jpg", 100 - i)))
            .collect();
        let plan = Plan::compute(
            &selected,
            &HashSet::new(),
            &settings(Transform::Numbering {
                pattern: "Holiday {n:03}".into(),
                start: 1,
            }),
        );
        let expected: Vec<OsString> = (1..=20)
            .map(|n| os(&format!("Holiday {n:03}.jpg")))
            .collect();
        assert_eq!(new_names(&plan), expected);
        assert!(plan.can_apply());
        assert_eq!(plan.change_count(), 20);
    }

    #[test]
    fn numbering_start_width_and_old_name_tokens() {
        let plan = Plan::compute(
            &names(&["a.txt", "b.txt"]),
            &HashSet::new(),
            &settings(Transform::Numbering {
                pattern: "{n:2}-{name}".into(),
                start: 9,
            }),
        );
        assert_eq!(new_names(&plan), names(&["09-a.txt", "10-b.txt"]));
    }

    #[test]
    fn numbering_literal_braces() {
        let plan = Plan::compute(
            &names(&["a"]),
            &HashSet::new(),
            &settings(Transform::Numbering {
                pattern: "{{x}} {n}".into(),
                start: 1,
            }),
        );
        assert_eq!(new_names(&plan), names(&["{x} 1"]));
    }

    #[test]
    fn numbering_pattern_errors_are_worded_settings_errors() {
        let err = |pattern: &str| {
            Plan::compute(
                &names(&["a"]),
                &HashSet::new(),
                &settings(Transform::Numbering {
                    pattern: pattern.into(),
                    start: 1,
                }),
            )
            .error
        };
        assert_eq!(err("{x}"), Some(SettingsError::UnknownToken("x".into())));
        assert_eq!(err("{n"), Some(SettingsError::UnclosedBrace));
        assert_eq!(err("n}"), Some(SettingsError::StrayBrace));
        assert_eq!(err("{n:0}"), Some(SettingsError::BadWidth("0".into())));
        assert_eq!(err("{n:abc}"), Some(SettingsError::BadWidth("abc".into())));
        assert_eq!(
            err("{n:99999999}"),
            Some(SettingsError::BadWidth("99999999".into()))
        );
    }

    // ── non-UTF-8 names: bytes outside the edit survive ─────────────────

    #[test]
    fn non_utf8_names_keep_their_bytes_in_every_transform() {
        // 0xFF is never valid UTF-8.
        let weird = bytes_name(b"IMG\xFF_1.jpg");
        let run = |name: &OsString, transform: Transform| {
            Plan::compute(
                std::slice::from_ref(name),
                &HashSet::new(),
                &settings(transform),
            )
            .rows[0]
                .new
                .clone()
        };

        // find/replace: only the matched text changes.
        assert_eq!(
            run(&weird, find_replace("IMG", "pic", false)).as_bytes(),
            b"pic\xFF_1.jpg"
        );
        // regex matches across the invalid byte without disturbing it.
        assert_eq!(
            run(&weird, find_replace(r"\d", "N", true)).as_bytes(),
            b"IMG\xFF_N.jpg"
        );
        // affix.
        assert_eq!(
            run(
                &weird,
                Transform::Affix {
                    prefix: "a_".into(),
                    suffix: "_z".into()
                }
            )
            .as_bytes(),
            b"a_IMG\xFF_1_z.jpg"
        );
        // case, both directions.
        assert_eq!(
            run(&weird, Transform::Case(CaseKind::Lower)).as_bytes(),
            b"img\xFF_1.jpg"
        );
        assert_eq!(
            run(
                &bytes_name(b"img\xFF_1.jpg"),
                Transform::Case(CaseKind::Upper)
            )
            .as_bytes(),
            b"IMG\xFF_1.jpg"
        );
        // numbering keeps the {name} bytes verbatim.
        assert_eq!(
            run(
                &weird,
                Transform::Numbering {
                    pattern: "{name}-{n}".into(),
                    start: 1
                }
            )
            .as_bytes(),
            b"IMG\xFF_1-1.jpg"
        );
    }

    #[test]
    fn title_case_does_not_start_a_new_word_after_an_invalid_byte() {
        let plan = Plan::compute(
            &[bytes_name(b"ab\xFFcd ef")],
            &HashSet::new(),
            &full_name(Transform::Case(CaseKind::Title)),
        );
        assert_eq!(plan.rows[0].new.as_bytes(), b"Ab\xFFcd Ef");
    }

    #[test]
    fn a_non_utf8_name_the_transform_does_not_touch_stays_unchanged() {
        let weird = bytes_name(b"\xFF\xFE");
        let plan = Plan::compute(
            std::slice::from_ref(&weird),
            &HashSet::new(),
            &settings(find_replace("zzz", "y", false)),
        );
        assert_eq!(plan.rows[0].status, Status::Unchanged);
        assert_eq!(plan.rows[0].new, weird);
    }

    // ── validation ──────────────────────────────────────────────────────

    #[test]
    fn invalid_names_are_flagged() {
        let check = |new: &str| {
            Plan::compute(
                &names(&["x"]),
                &HashSet::new(),
                &full_name(find_replace("x", new, false)),
            )
            .rows[0]
                .status
        };
        assert_eq!(check("a/b"), Status::Invalid(Invalid::BadCharacter));
        assert_eq!(check("a\0b"), Status::Invalid(Invalid::BadCharacter));
        assert_eq!(check(".."), Status::Invalid(Invalid::DotName));
        assert_eq!(check("."), Status::Invalid(Invalid::DotName));
        assert_eq!(check(""), Status::Invalid(Invalid::Empty));
        assert_eq!(
            check(&"n".repeat(NAME_MAX + 1)),
            Status::Invalid(Invalid::TooLong)
        );
        assert_eq!(check(&"n".repeat(NAME_MAX)), Status::Ready);
    }

    #[test]
    fn two_items_mapping_to_one_name_are_both_duplicates() {
        let plan = Plan::compute(
            &names(&["a1", "a2"]),
            &HashSet::new(),
            &settings(find_replace(r"\d", "", true)),
        );
        assert_eq!(plan.rows[0].status, Status::Duplicate);
        assert_eq!(plan.rows[1].status, Status::Duplicate);
        assert!(!plan.can_apply());
        assert_eq!(plan.problem_count(), 2);
    }

    #[test]
    fn a_new_name_equal_to_an_unchanged_selected_name_is_a_duplicate() {
        // `b` is unchanged (no "a" in it) and keeps its name; `a` -> `b`.
        let plan = Plan::compute(
            &names(&["a", "b"]),
            &HashSet::new(),
            &full_name(find_replace("a", "b", false)),
        );
        assert_eq!(plan.rows[0].status, Status::Duplicate);
        assert_eq!(plan.rows[1].status, Status::Unchanged);
    }

    #[test]
    fn collision_with_an_unselected_entry_blocks_apply() {
        let plan = Plan::compute(
            &names(&["a.txt"]),
            &set(&["b.txt"]),
            &settings(find_replace("a", "b", false)),
        );
        assert_eq!(plan.rows[0].status, Status::Exists);
        assert!(!plan.can_apply());
        assert_eq!(
            plan.first_problem().map(|r| r.new.clone()),
            Some(os("b.txt"))
        );
    }

    #[test]
    fn unchanged_rows_are_skipped_not_errors() {
        let plan = Plan::compute(
            &names(&["a1", "b"]),
            &HashSet::new(),
            &settings(find_replace("1", "2", false)),
        );
        assert_eq!(plan.rows[1].status, Status::Unchanged);
        assert!(plan.can_apply());
        assert_eq!(plan.moves(), vec![(os("a1"), os("a2"))]);
    }

    #[test]
    fn handing_one_selected_item_anothers_old_name_is_allowed() {
        // Numbering gives "2" the name "1" and "1" the name "2": a swap.
        let plan = Plan::compute(
            &names(&["2", "1"]),
            &HashSet::new(),
            &full_name(Transform::Numbering {
                pattern: "{n}".into(),
                start: 1,
            }),
        );
        assert_eq!(new_names(&plan), names(&["1", "2"]));
        assert!(plan.can_apply());
    }

    // ── scheduling ──────────────────────────────────────────────────────

    /// Replays `steps` on a folder: every step must take an existing name to
    /// a free one. Returns the resulting names.
    fn simulate(start: &[&str], steps: &[Step]) -> HashSet<OsString> {
        let mut folder = set(start);
        for step in steps {
            assert!(folder.remove(&step.from), "{:?} missing", step.from);
            assert!(folder.insert(step.to.clone()), "{:?} overwritten", step.to);
        }
        folder
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter().map(|(a, b)| (os(a), os(b))).collect()
    }

    #[test]
    fn a_chain_runs_back_to_front() {
        let moves = pairs(&[("a", "b"), ("b", "c")]);
        let steps = schedule(&moves, &set(&["a", "b"]));
        assert_eq!(simulate(&["a", "b"], &steps), set(&["b", "c"]));
        assert_eq!(steps.len(), 2, "a chain needs no temp name");
    }

    #[test]
    fn a_swap_uses_one_temp_name() {
        let moves = pairs(&[("a", "b"), ("b", "a")]);
        let steps = schedule(&moves, &set(&["a", "b"]));
        assert_eq!(simulate(&["a", "b"], &steps), set(&["a", "b"]));
        assert_eq!(steps.len(), 3);
        // The temp name is hidden and is gone at the end.
        let temp = &steps[0].to;
        assert!(temp.as_bytes().starts_with(b".saola-tmp-"));
        assert_eq!(&steps[2].from, temp);
    }

    #[test]
    fn a_three_cycle_and_an_independent_chain_together() {
        let moves = pairs(&[("a", "b"), ("b", "c"), ("c", "a"), ("x", "y"), ("y", "z")]);
        let start = ["a", "b", "c", "x", "y"];
        let steps = schedule(&moves, &set(&start));
        assert_eq!(simulate(&start, &steps), set(&["a", "b", "c", "y", "z"]));
        // Follow each item through the steps: it must end where it was sent.
        let mut at: HashMap<OsString, OsString> = start.iter().map(|n| (os(n), os(n))).collect();
        for step in &steps {
            for current in at.values_mut() {
                if *current == step.from {
                    *current = step.to.clone();
                }
            }
        }
        for (from, to) in &moves {
            assert_eq!(at.get(from), Some(to));
        }
    }

    #[test]
    fn temp_names_avoid_existing_and_target_names() {
        let moves = pairs(&[("a", "b"), ("b", "a")]);
        // The first two candidates are already taken in the folder.
        let taken = set(&["a", "b", ".saola-tmp-1", ".saola-tmp-2"]);
        let steps = schedule(&moves, &taken);
        assert_eq!(steps[0].to, os(".saola-tmp-3"));
    }

    #[test]
    fn identity_moves_are_dropped() {
        assert!(schedule(&pairs(&[("a", "a")]), &set(&["a"])).is_empty());
    }

    #[test]
    fn two_disjoint_swaps_each_get_their_own_temp_name() {
        let moves = pairs(&[("a", "b"), ("b", "a"), ("c", "d"), ("d", "c")]);
        let start = ["a", "b", "c", "d"];
        let steps = schedule(&moves, &set(&start));
        assert_eq!(simulate(&start, &steps), set(&start));
        let temps: HashSet<_> = steps
            .iter()
            .filter(|s| s.to.as_bytes().starts_with(b".saola-tmp-"))
            .map(|s| s.to.clone())
            .collect();
        assert_eq!(temps.len(), 2);
    }

    // ── undo planning ───────────────────────────────────────────────────

    #[test]
    fn undo_restores_everything_when_nothing_moved_on() {
        let moves = pairs(&[("b", "a"), ("a", "b")]);
        let plan = plan_undo(&moves, &set(&["a", "b"]));
        assert_eq!(plan.moves, moves);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn undo_skips_missing_and_occupied_items_only() {
        // `new1`->`old1` is fine, `new2` was deleted, `old3` is now held by
        // something else.
        let moves = pairs(&[("new1", "old1"), ("new2", "old2"), ("new3", "old3")]);
        let plan = plan_undo(&moves, &set(&["new1", "new3", "old3"]));
        assert_eq!(plan.moves, pairs(&[("new1", "old1")]));
        assert!(plan.skipped.contains(&(os("new2"), SkipReason::Missing)));
        assert!(plan.skipped.contains(&(os("new3"), SkipReason::Occupied)));
    }

    #[test]
    fn dropping_one_undo_pair_strands_the_pair_that_wanted_its_name() {
        // Item `x` goes back to `y`, item `y` goes back to `z`. `z` is held
        // by an outsider, so (y -> z) is dropped; `y` then stays put, which
        // leaves (x -> y) with nowhere to go.
        let moves = pairs(&[("x", "y"), ("y", "z")]);
        let plan = plan_undo(&moves, &set(&["x", "y", "z"]));
        assert!(plan.moves.is_empty());
        assert_eq!(plan.skipped.len(), 2);
    }

    // ── executing, against an in-memory backend ─────────────────────────

    /// A folder of names whose `rename` really moves them, failing on chosen
    /// calls. A rename onto an existing name replaces it (as rename(2)
    /// does), which shows up as a missing name in the assertions.
    struct MemBackend {
        names: Mutex<Vec<OsString>>,
        calls: Mutex<usize>,
        /// 1-based indexes of `rename` calls that fail.
        fail_calls: Vec<usize>,
    }

    impl MemBackend {
        fn new(names: &[&str], fail_calls: &[usize]) -> Self {
            MemBackend {
                names: Mutex::new(names.iter().map(|n| os(n)).collect()),
                calls: Mutex::new(0),
                fail_calls: fail_calls.to_vec(),
            }
        }

        fn names(&self) -> HashSet<OsString> {
            self.names
                .lock()
                .map(|n| n.iter().cloned().collect())
                .unwrap_or_default()
        }
    }

    fn entry(name: OsString) -> FileEntry {
        FileEntry {
            name,
            kind: EntryKind::File,
            size: 0,
            modified: None,
            is_symlink: false,
            mode: None,
        }
    }

    fn other(message: &str) -> VfsError {
        VfsError::Other {
            message: message.into(),
        }
    }

    #[async_trait]
    impl Backend for MemBackend {
        fn scheme(&self) -> &'static str {
            "file"
        }
        fn caps(&self) -> Caps {
            Caps::RENAME_IN_PLACE
        }
        async fn list(&self, _location: &Location) -> Result<Vec<FileEntry>, VfsError> {
            Ok(self.names().into_iter().map(entry).collect())
        }
        async fn metadata(&self, location: &Location) -> Result<FileEntry, VfsError> {
            let name = location.path.file_name().map(OsStr::to_os_string);
            match name {
                Some(name) if self.names().contains(&name) => Ok(entry(name)),
                _ => Err(VfsError::NotFound {
                    location: location.to_string(),
                }),
            }
        }
        async fn read(&self, _l: &Location) -> Result<ReadStream, VfsError> {
            Err(other("unsupported"))
        }
        async fn write(&self, _l: &Location) -> Result<WriteSink, VfsError> {
            Err(other("unsupported"))
        }
        async fn mkdir(&self, _l: &Location) -> Result<(), VfsError> {
            Err(other("unsupported"))
        }
        async fn rename(&self, from: &Location, to: &Location) -> Result<(), VfsError> {
            let call = {
                let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
                *calls += 1;
                *calls
            };
            if self.fail_calls.contains(&call) {
                return Err(VfsError::PermissionDenied {
                    location: from.to_string(),
                });
            }
            let (Some(f), Some(t)) = (from.path.file_name(), to.path.file_name()) else {
                return Err(other("bad path"));
            };
            let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
            if !names.iter().any(|n| n == f) {
                return Err(VfsError::NotFound {
                    location: from.to_string(),
                });
            }
            names.retain(|n| n != t);
            if let Some(slot) = names.iter_mut().find(|n| *n == f) {
                *slot = t.to_os_string();
            }
            Ok(())
        }
        async fn remove(&self, _l: &Location) -> Result<(), VfsError> {
            Err(other("unsupported"))
        }
        async fn set_times(
            &self,
            _l: &Location,
            _a: Option<SystemTime>,
            _m: Option<SystemTime>,
        ) -> Result<(), VfsError> {
            Err(other("unsupported"))
        }
        fn watch(&self, _l: &Location) -> Option<BoxStream<'static, DirEvent>> {
            None
        }
    }

    fn dir() -> Location {
        Location::local("/scratch")
    }

    #[tokio::test]
    async fn apply_swaps_two_names() {
        let backend = MemBackend::new(&["a", "b"], &[]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "b"), ("b", "a")])).await;
        assert_eq!(report.failure, None);
        assert_eq!(backend.names(), set(&["a", "b"]));
        let mut moved = report.moved.clone();
        moved.sort();
        // (current, original): the item now called "a" was "b", and back.
        assert_eq!(moved, pairs(&[("a", "b"), ("b", "a")]));
    }

    #[tokio::test]
    async fn apply_runs_a_chain() {
        let backend = MemBackend::new(&["a", "b"], &[]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "b"), ("b", "c")])).await;
        assert_eq!(report.failure, None);
        assert_eq!(backend.names(), set(&["b", "c"]));
    }

    #[tokio::test]
    async fn apply_refuses_when_a_target_appeared_after_the_preview() {
        let backend = MemBackend::new(&["a", "b"], &[]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "b")])).await;
        let failure = report.failure.expect("must refuse to overwrite b");
        assert!(matches!(failure.error, VfsError::AlreadyExists { .. }));
        assert_eq!(backend.names(), set(&["a", "b"]));
        assert!(report.moved.is_empty());
    }

    #[tokio::test]
    async fn apply_refuses_when_a_source_vanished_and_touches_nothing() {
        let backend = MemBackend::new(&["a"], &[]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "x"), ("gone", "y")])).await;
        let failure = report.failure.expect("gone is missing");
        assert_eq!(failure.name, os("gone"));
        assert!(matches!(failure.error, VfsError::NotFound { .. }));
        assert_eq!(backend.names(), set(&["a"]));
    }

    #[tokio::test]
    async fn a_failure_in_the_middle_rolls_everything_back() {
        // Item 3 of 5 fails: the first two renames are undone.
        let moves = pairs(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")]);
        let start = ["a", "b", "c", "d", "e"];
        let backend = MemBackend::new(&start, &[3]);
        let report = apply(&backend, &dir(), &moves).await;

        let failure = report.failure.expect("step 3 fails");
        assert_eq!(failure.name, os("c"));
        assert_eq!(failure.target, os("C"));
        assert!(report.stuck.is_empty());
        assert!(report.moved.is_empty(), "rolled back: no undo entry");
        assert_eq!(backend.names(), set(&start));
    }

    #[tokio::test]
    async fn a_failure_inside_a_swap_restores_the_originals_and_leaves_no_temp() {
        // Steps: a->tmp (1), b->a (2), tmp->b (3, fails).
        let backend = MemBackend::new(&["a", "b"], &[3]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "b"), ("b", "a")])).await;
        let failure = report.failure.expect("step 3 fails");
        // Named by the item, not by the temporary name.
        assert_eq!(failure.name, os("a"));
        assert_eq!(failure.target, os("b"));
        assert_eq!(backend.names(), set(&["a", "b"]));
        assert!(report.moved.is_empty());
        assert!(report.stuck.is_empty());
    }

    #[tokio::test]
    async fn if_rollback_also_fails_the_report_names_exactly_what_changed() {
        // Forward: a->A (1), b->B (2), c->C (3, fails). Rollback: B->b (4,
        // fails), A->a (5, ok).
        let backend = MemBackend::new(&["a", "b", "c"], &[3, 4]);
        let report = apply(
            &backend,
            &dir(),
            &pairs(&[("a", "A"), ("b", "B"), ("c", "C")]),
        )
        .await;
        assert_eq!(
            report.failure.as_ref().map(|f| f.name.clone()),
            Some(os("c"))
        );
        assert_eq!(report.stuck.len(), 1);
        assert_eq!(report.stuck[0].name, os("b"));
        // Only b is still renamed, and the report says so (current, original).
        assert_eq!(report.moved, pairs(&[("B", "b")]));
        assert_eq!(backend.names(), set(&["a", "B", "c"]));
    }

    #[tokio::test]
    async fn undoing_a_swap_puts_both_names_back() {
        let backend = MemBackend::new(&["a", "b"], &[]);
        let report = apply(&backend, &dir(), &pairs(&[("a", "b"), ("b", "a")])).await;
        let plan = plan_undo(&report.moved, &backend.names());
        assert!(plan.skipped.is_empty());
        let back = apply(&backend, &dir(), &plan.moves).await;
        assert_eq!(back.failure, None);
        assert_eq!(backend.names(), set(&["a", "b"]));
    }

    // ── executing, against the real local backend in a temp directory ───

    fn tempdir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "saola-files-bulk-rename-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn a_real_swap_moves_contents_with_the_names_and_undo_restores_them() {
        let root = tempdir();
        std::fs::write(root.join("a"), "contents of a").expect("write a");
        std::fs::write(root.join("b"), "contents of b").expect("write b");
        let location = Location::local(&root);
        let backend = crate::modules::local::LocalBackend::new();

        let report = apply(&backend, &location, &pairs(&[("a", "b"), ("b", "a")])).await;
        assert_eq!(report.failure, None);
        assert_eq!(
            std::fs::read_to_string(root.join("b")).expect("read b"),
            "contents of a"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a")).expect("read a"),
            "contents of b"
        );
        let present: HashSet<OsString> = std::fs::read_dir(&root)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(present.len(), 2, "no temp file left: {present:?}");

        let undo = plan_undo(&report.moved, &present);
        let back = apply(&backend, &location, &undo.moves).await;
        assert_eq!(back.failure, None);
        assert_eq!(
            std::fs::read_to_string(root.join("a")).expect("read a"),
            "contents of a"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b")).expect("read b"),
            "contents of b"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_real_non_utf8_name_is_renamed_byte_exact() {
        let root = tempdir();
        let old = bytes_name(b"IMG\xFF_1.jpg");
        std::fs::write(root.join(&old), "x").expect("write");
        let plan = Plan::compute(
            std::slice::from_ref(&old),
            &HashSet::new(),
            &settings(find_replace("IMG", "pic", false)),
        );
        let backend = crate::modules::local::LocalBackend::new();
        let report = apply(&backend, &Location::local(&root), &plan.moves()).await;
        assert_eq!(report.failure, None);
        assert!(root.join(bytes_name(b"pic\xFF_1.jpg")).exists());
        assert!(!root.join(&old).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
