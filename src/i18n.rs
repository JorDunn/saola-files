//! The message catalogue: every user-visible string in `src/ui/` and
//! `src/app/` is an id looked up here, never a literal at the call site.
//!
//! English only for now (`i18n/en-US/files.ftl`, embedded in the binary).
//! The survey behind the choice of Fluent is in `docs/decisions/i18n.md`.
//!
//! # Using it
//!
//! ```ignore
//! use saola_files::tr;
//!
//! tr!("menu-copy");                                     // plain message
//! tr!("conflict-title", name = name.to_string_lossy()); // with arguments
//! tr!("items-count", count = n);                        // plural-aware count
//! tr_static!("menu-copy");                              // `&'static str`, see below
//! ```
//!
//! Ids must be string *literals* so the tests can find them: one test
//! checks every id used in the source exists in the catalogue, another
//! fails if a user-visible literal appears in `src/ui/` or `src/app/`.
//! Where an id has to live in a `static` table (the context menu), wrap it
//! in [`msg!`] — it returns the literal unchanged but marks it for the
//! tests — and resolve it later with [`lookup`].
//!
//! `saola-theme` widgets that take a `&'a str` label (`menu_row`,
//! `section_label`, `empty_state`) need a string that outlives the view;
//! use [`tr_static!`] for those. Everywhere else `tr!` (an owned `String`).
//!
//! # Filenames
//!
//! Filenames are never part of the catalogue. They stay `OsString`/`PathBuf`
//! until the view, and are passed as arguments after `to_string_lossy()`.
//!
//! # Totality
//!
//! Lookup never panics. A missing id renders as the id itself, so a typo is
//! visible on screen instead of fatal. A missing argument renders the way
//! Fluent renders it (the variable name in braces).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use fluent_bundle::FluentResource;
use fluent_bundle::concurrent::FluentBundle;
use unic_langid::LanguageIdentifier;

/// Named arguments for a message (`name = value` pairs). Re-exported so call
/// sites go through the [`tr!`] macro and never name the Fluent crate.
pub use fluent_bundle::FluentArgs as Args;

/// The English catalogue, compiled into the binary.
const EN_US: &str = include_str!("../i18n/en-US/files.ftl");

// Teaching note: `OnceLock` is a cell that can be written exactly once and
// then read from any thread without locking. The bundle is built the first
// time anything asks for a string and lives for the rest of the process. The
// `concurrent` flavour of `FluentBundle` is the thread-safe one (its plural
// rule cache sits behind a `Mutex`), which a `static` requires.
static BUNDLE: OnceLock<FluentBundle<FluentResource>> = OnceLock::new();

fn bundle() -> &'static FluentBundle<FluentResource> {
    BUNDLE.get_or_init(|| build_bundle(EN_US))
}

/// Builds the en-US bundle from `source`. Separate from [`bundle`] so tests
/// can feed it a deliberately broken catalogue.
fn build_bundle(source: &str) -> FluentBundle<FluentResource> {
    // "en-US" always parses; the default identifier is the unreachable
    // fallback that keeps this path free of `unwrap`.
    let locale: LanguageIdentifier = "en-US".parse().unwrap_or_default();
    let mut bundle = FluentBundle::new_concurrent(vec![locale]);
    // Fluent wraps every interpolated argument in Unicode bidi isolation
    // marks (U+2068/U+2069) by default. English needs none, and they would
    // change the rendered text (and make it unequal to the old literals).
    bundle.set_use_isolating(false);
    // A parse error still yields a resource holding every entry that did
    // parse; use it rather than showing nothing. The catalogue test asserts
    // there are no errors, so this only matters if that test is skipped.
    let resource = match FluentResource::try_new(source.to_owned()) {
        Ok(resource) | Err((resource, _)) => resource,
    };
    // `add_resource` only errors on duplicate ids; the first definition wins.
    let _ = bundle.add_resource(resource);
    bundle
}

/// Looks up `id` and formats it with `args`. See the module docs for the
/// fallback behaviour; prefer the [`tr!`] macro at call sites.
pub fn lookup(id: &str, args: Option<&Args<'_>>) -> String {
    format_with(bundle(), id, args)
}

fn format_with(bundle: &FluentBundle<FluentResource>, id: &str, args: Option<&Args<'_>>) -> String {
    let Some(pattern) = bundle.get_message(id).and_then(|message| message.value()) else {
        return id.to_owned();
    };
    // Formatting errors (e.g. a missing argument) are collected here and
    // dropped: Fluent still returns the best-effort text.
    let mut errors = Vec::new();
    bundle
        .format_pattern(pattern, args, &mut errors)
        .into_owned()
}

/// Looks up an argument-less message and returns it as a `&'static str`.
///
/// Some `saola-theme` widgets (`menu_row`, `section_label`, `empty_state`)
/// borrow their label as `&'a str` for as long as the returned `Element`
/// lives, so a freshly built `String` cannot be handed to them. Messages
/// without arguments never change, so each one is formatted once, kept for
/// the rest of the process, and handed out by reference.
///
/// Teaching note: `Box::leak` turns an owned `String` into a reference that
/// is never freed. That is normally a leak; here it is bounded, because the
/// cache holds at most one entry per catalogue id (there are a few hundred
/// of them, ever) and `id` must itself be `'static`, so it cannot be fed
/// user data. Use [`tr!`] when you need an owned `String`, or have arguments.
pub fn lookup_static(id: &'static str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    // A poisoned lock means another thread panicked mid-insert; the id
    // itself is the same fallback a missing message gets.
    let Ok(mut cache) = cache.lock() else {
        return id;
    };
    cache
        .entry(id)
        .or_insert_with(|| Box::leak(lookup(id, None).into_boxed_str()))
}

/// Whether the shipped catalogue defines `id`. Used by the tests.
#[cfg(test)]
fn has_message(id: &str) -> bool {
    bundle().has_message(id)
}

/// Looks up a message, optionally with named arguments.
///
/// `tr!("id")`, or `tr!("id", name = value, count = n)`. Values are anything
/// Fluent accepts: strings (`&str`, `String`, `Cow<str>`) and integers.
/// Integers are what plural selectors in the catalogue switch on.
#[macro_export]
macro_rules! tr {
    ($id:literal $(,)?) => {
        $crate::i18n::lookup($id, ::core::option::Option::None)
    };
    ($id:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        // Teaching note: `$(...)+` repeats once per `name = value` pair.
        // `stringify!($name)` turns the identifier into the argument's name.
        let mut args = $crate::i18n::Args::new();
        $( args.set(::core::stringify!($name), $value); )+
        $crate::i18n::lookup($id, ::core::option::Option::Some(&args))
    }};
}

/// Like `tr!("id")` but returns a `&'static str`, for widgets that borrow
/// their label (see [`lookup_static`]). Takes no arguments.
#[macro_export]
macro_rules! tr_static {
    ($id:literal $(,)?) => {
        $crate::i18n::lookup_static($id)
    };
}

/// Marks a message id that is stored rather than looked up on the spot (a
/// `static` table row). Expands to the literal itself; resolve it later with
/// [`lookup`]. The id-coverage test treats it like [`tr!`].
#[macro_export]
macro_rules! msg {
    ($id:literal $(,)?) => {
        $id
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- catalogue ---

    #[test]
    fn catalogue_parses_without_errors() {
        // `try_new` hands back the partial resource *and* the errors;
        // here any error fails the test.
        let parsed = FluentResource::try_new(EN_US.to_owned());
        assert!(
            parsed.is_ok(),
            "files.ftl has syntax errors: {:?}",
            parsed.err().map(|(_, errors)| errors)
        );
    }

    #[test]
    fn broken_catalogue_still_serves_the_entries_that_parsed() {
        let bundle = build_bundle("good = Fine\n!!! not a message\n\nalso-good = Also fine\n");
        assert_eq!(format_with(&bundle, "good", None), "Fine");
        assert_eq!(format_with(&bundle, "also-good", None), "Also fine");
    }

    // --- missing ids ---

    #[test]
    fn missing_id_renders_the_id_itself() {
        assert_eq!(tr!("no-such-message-id"), "no-such-message-id");
        assert_eq!(tr!("no-such-message-id", name = "x"), "no-such-message-id");
    }

    #[test]
    fn static_lookup_is_cached_and_falls_back_to_the_id() {
        let first = tr_static!("no-such-static-id");
        assert_eq!(first, "no-such-static-id");
        // Same id twice hands out the same leaked allocation.
        assert!(std::ptr::eq(first, tr_static!("no-such-static-id")));
    }

    #[test]
    fn empty_id_does_not_panic() {
        assert_eq!(lookup("", None), "");
    }

    // --- the shipped catalogue keeps the wording the literals had ---

    #[test]
    fn plain_messages_keep_their_original_wording() {
        let expected = [
            ("window-title", "Files"),
            ("action-cancel", "Cancel"),
            ("action-close", "Close"),
            ("status-loading", "Loading\u{2026}"),
            ("header-hidden", "Hidden"),
            (
                "breadcrumb-path-placeholder",
                "scheme://host/path or /a/local/path",
            ),
            ("sidebar-removable", "REMOVABLE"),
            ("sidebar-connect", "Connect to Server\u{2026}"),
            ("dirview-empty", "This folder is empty"),
            ("dirview-column-name", "Name"),
            ("dirview-column-size", "Size"),
            ("dirview-column-modified", "Date modified"),
            ("dirview-rename-placeholder", "Name"),
            ("dirview-rename-invalid", "Enter a valid name"),
            ("dirview-new-folder-name", "New Folder"),
            ("dirview-new-file-name", "New File"),
            ("menu-open", "Open"),
            ("menu-open-with", "Open with\u{2026}"),
            ("menu-open-in-terminal", "Open in terminal"),
            ("menu-copy", "Copy"),
            ("menu-cut", "Cut"),
            ("menu-paste", "Paste"),
            ("menu-rename", "Rename\u{2026}"),
            ("menu-move-to-trash", "Move to Trash"),
            ("menu-delete", "Delete"),
            ("menu-new-folder", "New Folder"),
            ("menu-new-file", "New File"),
            ("menu-properties", "Properties"),
            ("menu-nothing-selected", "Nothing selected"),
            ("menu-no-known-app", "No known app for this file type"),
            ("conflict-keep-both", "Keep both"),
            ("conflict-overwrite", "Overwrite"),
            ("conflict-skip", "Skip"),
            (
                "conflict-apply-to-all",
                "Apply to all conflicts in this operation",
            ),
            ("connect-title", "Connect to Server"),
            ("connect-connecting", "Connecting\u{2026}"),
            ("connect-uri-placeholder", "sftp://user@host/path"),
            (
                "connect-enter-uri",
                "Enter a server URI, e.g. sftp://user@host/path",
            ),
            (
                "connect-no-saved-servers",
                "No saved servers yet \u{2014} add a [[server]] entry to files.toml.",
            ),
            ("connect-saved-servers", "SAVED SERVERS"),
            ("connect-connect", "Connect"),
            ("connect-trust", "Trust & Connect"),
            ("connect-dont-trust", "Don't Trust"),
            ("connect-passphrase-placeholder", "Passphrase"),
            ("connect-password-placeholder", "Password"),
            ("connect-submit", "Submit"),
            ("connect-skip-key", "Skip This Key"),
            ("properties-calculating", "Calculating\u{2026}"),
            ("properties-location", "Location"),
            ("properties-kind", "Kind"),
            ("properties-size", "Size"),
            ("properties-modified", "Modified"),
            ("properties-permissions", "Permissions"),
            ("properties-kind-symlink", "Symlink"),
            ("properties-kind-folder", "Folder"),
            ("undo-button", "Undo"),
            ("trash-title", "Trash"),
            ("trash-empty-state", "Trash is empty"),
            ("trash-empty-button", "Empty Trash"),
            (
                "trash-confirm-empty",
                "Permanently delete everything in the trash?",
            ),
            ("trash-restore", "Restore"),
        ];
        for (id, text) in expected {
            assert_eq!(lookup(id, None), text, "{id}");
        }
    }

    #[test]
    fn messages_with_arguments_keep_their_original_wording() {
        assert_eq!(
            tr!("conflict-title", name = "a b.txt"),
            "\"a b.txt\" already exists"
        );
        assert_eq!(
            tr!("conflict-body", dest = "/d/x", source = "/s/x"),
            "/d/x is already there. What should happen to /s/x?"
        );
        assert_eq!(
            tr!("error-no-backend", scheme = "ftp"),
            "no backend for scheme \"ftp\""
        );
        assert_eq!(
            tr!("error-create-failed", location = "/tmp/x"),
            "creating /tmp/x failed"
        );
        assert_eq!(
            tr!("error-trash-internal", detail = "boom"),
            "internal error talking to the trash: boom"
        );
        assert_eq!(
            tr!("dirview-name-with-counter", base = "New Folder", n = 2u32),
            "New Folder (2)"
        );
        assert_eq!(
            tr!("connect-host-key-title", host = "example.org", port = 22u16),
            "example.org:22 presented a new host key"
        );
        assert_eq!(
            tr!(
                "connect-host-key-detail",
                key_type = "ssh-ed25519",
                fingerprint = "SHA256:abc"
            ),
            "ssh-ed25519 SHA256:abc\nThis server has never been connected to before. \
             Only trust it if you recognize this fingerprint."
        );
        assert_eq!(
            tr!("connect-passphrase-for", path = "/k/id"),
            "Passphrase for /k/id"
        );
        assert_eq!(tr!("connect-password-for", user = "jo"), "Password for jo");
        assert_eq!(
            tr!("progress-copying", done = 1usize, total = 3usize),
            "Copying\u{2026} 1 of 3"
        );
        assert_eq!(
            tr!(
                "progress-moving-named",
                name = "a.txt",
                done = 2usize,
                total = 3usize
            ),
            "Moving a.txt\u{2026} 2 of 3"
        );
        assert_eq!(
            tr!("properties-items-selected", count = 5usize),
            "5 items selected"
        );
        assert_eq!(
            tr!("properties-mode", symbolic = "rwxr-xr-x", octal = "755"),
            "rwxr-xr-x (755)"
        );
    }

    #[test]
    fn properties_size_pluralises_item_for_zero_one_and_many() {
        let size = |count: usize| tr!("properties-size-and-count", size = "12.3 MB", count = count);
        assert_eq!(size(0), "12.3 MB \u{2014} 0 items");
        assert_eq!(size(1), "12.3 MB \u{2014} 1 item");
        assert_eq!(size(4), "12.3 MB \u{2014} 4 items");
        assert_eq!(size(12_345), "12.3 MB \u{2014} 12345 items");
    }

    // --- arguments and plurals ---

    fn test_bundle() -> FluentBundle<FluentResource> {
        build_bundle(
            "greet = Hello, { $name }.\n\
             count =\n    { $n ->\n        [one] { $n } item\n       *[other] { $n } items\n    }\n",
        )
    }

    #[test]
    fn arguments_are_interpolated_without_bidi_marks() {
        let bundle = test_bundle();
        let mut args = Args::new();
        args.set("name", "report.txt");
        assert_eq!(
            format_with(&bundle, "greet", Some(&args)),
            "Hello, report.txt."
        );
    }

    #[test]
    fn plural_forms_for_zero_one_and_many() {
        let bundle = test_bundle();
        let count = |n: usize| {
            let mut args = Args::new();
            args.set("n", n);
            format_with(&bundle, "count", Some(&args))
        };
        assert_eq!(count(0), "0 items");
        assert_eq!(count(1), "1 item");
        assert_eq!(count(2), "2 items");
        assert_eq!(count(1234), "1234 items");
    }

    #[test]
    fn missing_argument_does_not_panic() {
        let bundle = test_bundle();
        let rendered = format_with(&bundle, "greet", None);
        assert!(rendered.starts_with("Hello, "), "got {rendered:?}");
    }

    #[test]
    fn filenames_pass_through_verbatim_including_fluent_syntax() {
        let bundle = test_bundle();
        let name = std::ffi::OsString::from("{ $n } -> [one] \u{fffd}.txt");
        let mut args = Args::new();
        args.set("name", name.to_string_lossy());
        assert_eq!(
            format_with(&bundle, "greet", Some(&args)),
            "Hello, { $n } -> [one] \u{fffd}.txt."
        );
    }

    // --- source scans: ids exist, no stray literals ---

    use std::path::{Path, PathBuf};

    /// One string literal found in a source file.
    struct Literal {
        /// Contents between the quotes, escapes left as written.
        text: String,
        /// 1-based line the literal starts on.
        line: usize,
        /// The source text before the literal (for context checks).
        before: String,
    }

    /// Finds the string literals in Rust source, skipping comments and char
    /// literals. Not a full lexer: enough for this repo's style, and it errs
    /// on the side of reporting too much.
    fn literals(source: &str) -> Vec<Literal> {
        let chars: Vec<char> = source.chars().collect();
        let mut found = Vec::new();
        let (mut i, mut line) = (0, 1);
        while i < chars.len() {
            match chars[i] {
                '\n' => {
                    line += 1;
                    i += 1;
                }
                '/' if chars.get(i + 1) == Some(&'/') => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if chars.get(i + 1) == Some(&'*') => {
                    let mut depth = 0;
                    while i < chars.len() {
                        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                            depth += 1;
                            i += 2;
                        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            if chars[i] == '\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                    }
                }
                '\'' => {
                    // Char literal ('x', '\n', '"') or a lifetime ('a).
                    if chars.get(i + 1) == Some(&'\\') {
                        i += 2;
                        while i < chars.len() && chars[i] != '\'' {
                            i += 1;
                        }
                        i += 1;
                    } else if chars.get(i + 2) == Some(&'\'') {
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
                'r' if matches!(chars.get(i + 1), Some('"' | '#'))
                    && !chars
                        .get(i.wrapping_sub(1))
                        .is_some_and(|c| c.is_alphanumeric() || *c == '_') =>
                {
                    // Raw string: r"..." or r#"..."#.
                    let mut j = i + 1;
                    let mut hashes = 0;
                    while chars.get(j) == Some(&'#') {
                        hashes += 1;
                        j += 1;
                    }
                    if chars.get(j) != Some(&'"') {
                        i += 1;
                        continue;
                    }
                    let start = j + 1;
                    let mut end = start;
                    while end < chars.len() {
                        if chars[end] == '"'
                            && (0..hashes).all(|k| chars.get(end + 1 + k) == Some(&'#'))
                        {
                            break;
                        }
                        end += 1;
                    }
                    let end = end.min(chars.len());
                    let text: String = chars[start..end].iter().collect();
                    let lines_in = text.matches('\n').count();
                    found.push(Literal {
                        before: chars[..i].iter().collect(),
                        text,
                        line,
                    });
                    line += lines_in;
                    i = end + 1 + hashes;
                }
                '"' => {
                    let start = i + 1;
                    let mut end = start;
                    while end < chars.len() && chars[end] != '"' {
                        end += if chars[end] == '\\' { 2 } else { 1 };
                    }
                    let end = end.min(chars.len());
                    let text: String = chars[start..end].iter().collect();
                    let lines_in = text.matches('\n').count();
                    found.push(Literal {
                        before: chars[..i].iter().collect(),
                        text,
                        line,
                    });
                    line += lines_in;
                    i = end + 1;
                }
                _ => i += 1,
            }
        }
        found
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    fn files_under(relative: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        rust_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(relative),
            &mut out,
        );
        out.sort();
        out
    }

    /// Net `{` minus `}` on a line, ignoring braces inside string and char
    /// literals (good enough to find where a test-only item ends).
    fn brace_balance(line: &str) -> i32 {
        let mut balance = 0;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    while let Some(inner) = chars.next() {
                        match inner {
                            '\\' => {
                                chars.next();
                            }
                            '"' => break,
                            _ => {}
                        }
                    }
                }
                '/' if chars.peek() == Some(&'/') => break,
                '{' => balance += 1,
                '}' => balance -= 1,
                _ => {}
            }
        }
        balance
    }

    /// The production part of a file, with test-only code blanked out.
    ///
    /// - A column-0 `#[cfg(test)]` directly followed by `mod tests` is the
    ///   trailing test module: it and everything after it is dropped.
    /// - Any other `#[cfg(test)]` item (a `mod x;` line, or a fn/impl/mod
    ///   with a body) is dropped on its own; production code after it is
    ///   still scanned.
    ///
    /// Dropped lines become empty lines, so line numbers stay true.
    fn production_part(source: &str) -> String {
        let lines: Vec<&str> = source.lines().collect();
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim() != "#[cfg(test)]" {
                out.push(lines[i]);
                i += 1;
                continue;
            }
            let trailing_tests = lines[i] == "#[cfg(test)]"
                && lines
                    .get(i + 1)
                    .is_some_and(|next| next.trim_start().starts_with("mod tests"));
            if trailing_tests {
                out.extend(std::iter::repeat_n("", lines.len() - i));
                break;
            }
            // Skip the attribute, then the item: up to a `;` line, or until
            // the braces opened by the item close again.
            let mut depth = 0;
            let mut seen_open = false;
            loop {
                let line = lines[i];
                out.push("");
                i += 1;
                depth += brace_balance(line);
                seen_open |= line.contains('{');
                let ends_item = if seen_open {
                    depth <= 0
                } else {
                    line.trim_end().ends_with(';')
                };
                if ends_item || i >= lines.len() {
                    break;
                }
            }
        }
        out.join("\n")
    }

    /// Whether the text before a literal ends with one of `openers`
    /// (`tr!(`, `eprintln!(`, ...), ignoring whitespace. The opener must
    /// start a token: `str!(` does not count as `tr!(`.
    fn follows(literal: &Literal, openers: &[&str]) -> bool {
        let before = literal.before.trim_end();
        openers.iter().any(|opener| {
            before.strip_suffix(opener).is_some_and(|rest| {
                !rest
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        })
    }

    #[test]
    fn production_part_drops_only_test_code() {
        let source = "\
fn before() { let a = \"kept before\"; }
#[cfg(test)]
mod test_support;
fn after_mod() { let b = \"Totally visible text\"; }
struct S;
impl S {
    #[cfg(test)]
    fn only_in_tests(&self) {
        let c = \"test only {\";
        if true {
            let _ = 1;
        }
    }
    fn production(&self) { let d = \"kept after fn\"; }
}
#[cfg(test)]
mod tests {
    fn t() { let e = \"dropped\"; }
}
";
        let kept = production_part(source);
        let texts: Vec<String> = literals(&kept).into_iter().map(|l| l.text).collect();
        assert_eq!(
            texts,
            ["kept before", "Totally visible text", "kept after fn"]
        );
        // Line numbers survive, so reports point at the right line.
        let visible = literals(&kept)
            .into_iter()
            .find(|l| l.text == "Totally visible text");
        assert_eq!(visible.map(|l| l.line), Some(4));
    }

    #[test]
    fn follows_matches_whole_macro_names_only() {
        let lit = |before: &str| Literal {
            text: String::new(),
            line: 1,
            before: before.to_owned(),
        };
        assert!(follows(&lit("x = tr!("), &["tr!("]));
        assert!(follows(&lit("(\n  tr!(\n"), &["tr!("]));
        assert!(follows(&lit("tr!("), &["tr!("]));
        assert!(!follows(&lit("let s = str!("), &["tr!("]));
        assert!(!follows(&lit("my_tr!("), &["tr!("]));
    }

    #[test]
    fn every_message_id_used_in_code_exists_in_the_catalogue() {
        let mut checked = 0;
        let mut missing = Vec::new();
        for file in files_under("src") {
            if file.ends_with("i18n.rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            for literal in literals(&source) {
                if follows(&literal, &["tr!(", "tr_static!(", "msg!("]) {
                    checked += 1;
                    if !has_message(&literal.text) {
                        missing.push(format!(
                            "{}:{}: {}",
                            file.display(),
                            literal.line,
                            literal.text
                        ));
                    }
                }
            }
        }
        assert!(
            checked > 0,
            "found no tr!/msg! uses; is the scanner broken?"
        );
        assert!(
            missing.is_empty(),
            "ids missing from files.ftl:\n{}",
            missing.join("\n")
        );
    }

    /// Literals that are deliberately not catalogue messages: protocol
    /// names, environment variables and filename templates. Each entry is
    /// (file suffix, literal, reason). Adding to this list needs a reason a
    /// translator would agree with.
    const ALLOWED_LITERALS: &[(&str, &str, &str)] = &[(
        "src/app/mod.rs",
        "HOME",
        "environment variable name, not text shown to a user",
    )];

    /// Heuristic for "a human reads this": it has a letter, and either a
    /// space, a leading capital, or a typographic character. Identifiers
    /// ("inode/directory", "saola-files-rename-input") pass; "Name", "Open
    /// with…" and "No known app" do not.
    fn looks_user_visible(text: &str) -> bool {
        let has_letter = text.chars().any(|c| c.is_alphabetic());
        let first_letter_upper = text
            .chars()
            .find(|c| c.is_alphabetic())
            .is_some_and(|c| c.is_uppercase());
        has_letter
            && (text.contains(' ')
                || first_letter_upper
                || text.contains('\u{2026}')
                || text.contains("\\u{"))
    }

    #[test]
    fn no_user_visible_literals_in_ui_or_app() {
        let mut stray = Vec::new();
        for file in files_under("src/ui")
            .into_iter()
            .chain(files_under("src/app"))
        {
            if file
                .file_name()
                .is_some_and(|name| name == "test_support.rs")
            {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            let shown = file.to_string_lossy().into_owned();
            for literal in literals(&production_part(&source)) {
                // Log text, id markers and embedded assets are not
                // user-visible.
                if follows(
                    &literal,
                    &[
                        "tr!(",
                        "tr_static!(",
                        "msg!(",
                        "eprintln!(",
                        "println!(",
                        "include_bytes!(",
                        "include_str!(",
                    ],
                ) || !looks_user_visible(&literal.text)
                {
                    continue;
                }
                let line_start = literal.before.rfind('\n').map_or(0, |at| at + 1);
                if literal.before[line_start..].trim_start().starts_with("#[") {
                    continue;
                }
                let allowed = ALLOWED_LITERALS
                    .iter()
                    .any(|(suffix, text, _)| shown.ends_with(suffix) && *text == literal.text);
                if !allowed {
                    stray.push(format!("{shown}:{}: {:?}", literal.line, literal.text));
                }
            }
        }
        assert!(
            stray.is_empty(),
            "user-visible literals must go through tr!/msg! and i18n/en-US/files.ftl:\n{}",
            stray.join("\n")
        );
    }

    #[test]
    fn scanner_skips_comments_chars_and_finds_strings() {
        let source = "// \"commented\"\nlet c = '\"'; let s = \"real one\"; /* \"block\" */\nlet r = r#\"raw \"q\"\"#;\n";
        let texts: Vec<String> = literals(source).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, ["real one", "raw \"q\""]);
    }

    #[test]
    fn user_visible_heuristic_separates_prose_from_identifiers() {
        for prose in [
            "Name",
            "Open with\u{2026}",
            "no backend here",
            "Rename\\u{2026}",
        ] {
            assert!(looks_user_visible(prose), "{prose}");
        }
        for ident in [
            "inode/directory",
            "saola-files-rename-input",
            "{}:{}",
            "/",
            "sftp",
        ] {
            assert!(!looks_user_visible(ident), "{ident}");
        }
    }
}
