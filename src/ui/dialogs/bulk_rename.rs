//! The bulk-rename dialog: F2 (or "Rename selected…") with several items
//! selected. Four ways to build the new names (find and replace, prefix and
//! suffix, numbering, change case), a live old -> new preview, and Apply
//! that stays off while anything would collide or be invalid, with the
//! reason in words.
//!
//! The naming rules live in `core::fs::bulk_rename` (pure, tested there);
//! this module only holds the controls' state, recomputes a
//! [`bulk_rename::Plan`] after every change, and draws it. `App` owns the
//! dialog (`app::bulk_rename`), the same split `conflict`/`properties` use.
//!
//! Style: a dialog is ivory whatever the window's surface, so everything is
//! drawn on `Surface::Paper` with `style::dialog::surface`, exactly like the
//! other four modal dialogs. No colour carries severity: a problem row says
//! what is wrong in a sentence.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

use iced::widget::{Space, button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Center, Element, Fill, Length};
use saola_theme::icon::{self, Icon};
use saola_theme::{Chrome, ColorExt, Surface, Theme, convert, style, widget};

use crate::core::fs::bulk_rename::{
    self, CaseKind, Invalid, Plan, Scope, Settings, SettingsError, Status, Transform,
};
use crate::ui::dirview::BulkRenameRequest;
use crate::{tr, tr_static};

/// How many preview rows are drawn. A selection can run to thousands of
/// items and iced has no virtualised list, so the preview shows a window
/// onto the start of the plan; Apply and the problem line always consider
/// every item.
const PREVIEW_ROWS: usize = 200;

/// The preview list's fixed height and the dialog's width relative to the
/// standard dialog: layout choices for this dialog's own body, not
/// design-system sizes (same distinction `connect::SERVER_LIST_HEIGHT`
/// draws).
const PREVIEW_HEIGHT: f32 = 220.0;
const WIDTH_FACTOR: f32 = 1.4;

/// Widget ids, so `App` can focus the right field when the dialog opens or
/// the mode changes.
pub const FIND_INPUT_ID: &str = "saola-files-bulk-find";
pub const PREFIX_INPUT_ID: &str = "saola-files-bulk-prefix";
pub const PATTERN_INPUT_ID: &str = "saola-files-bulk-pattern";

/// Which set of controls is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    FindReplace,
    Affix,
    Numbering,
    Case,
}

#[derive(Debug, Clone)]
pub enum Message {
    ModeSelected(Mode),
    FindChanged(String),
    ReplaceChanged(String),
    RegexToggled(bool),
    MatchCaseToggled(bool),
    PrefixChanged(String),
    SuffixChanged(String),
    PatternChanged(String),
    StartChanged(String),
    CaseSelected(CaseKind),
    KeepExtensionToggled(bool),
    /// The primary button, or Enter in a field; ignored while the plan has
    /// problems.
    ApplyRequested,
    CancelRequested,
}

/// What the owner should do after [`BulkRename::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Close the dialog, nothing renamed.
    Cancel,
    /// Run these `(old, new)` renames in the dialog's folder.
    Apply(Vec<(OsString, OsString)>),
}

/// The dialog's state: the request it was opened with, the controls, and the
/// plan those controls currently produce.
pub struct BulkRename {
    request: BulkRenameRequest,
    others: HashSet<OsString>,
    mode: Mode,
    find: String,
    replace: String,
    regex: bool,
    match_case: bool,
    prefix: String,
    suffix: String,
    pattern: String,
    start_text: String,
    case: CaseKind,
    keep_extension: bool,
    plan: Plan,
}

impl BulkRename {
    pub fn new(request: BulkRenameRequest) -> Self {
        let others = request.others.iter().cloned().collect();
        let mut dialog = BulkRename {
            request,
            others,
            mode: Mode::FindReplace,
            find: String::new(),
            replace: String::new(),
            regex: false,
            match_case: true,
            prefix: String::new(),
            suffix: String::new(),
            pattern: String::new(),
            start_text: "1".to_owned(),
            case: CaseKind::Lower,
            keep_extension: true,
            plan: Plan {
                rows: Vec::new(),
                error: None,
            },
        };
        dialog.recompute();
        dialog
    }

    /// The folder the renames happen in.
    pub fn dir(&self) -> &crate::core::vfs::Location {
        &self.request.dir
    }

    /// The first field to focus for the current mode, if there is a field.
    pub fn focus_target(&self) -> Option<&'static str> {
        match self.mode {
            Mode::FindReplace => Some(FIND_INPUT_ID),
            Mode::Affix => Some(PREFIX_INPUT_ID),
            Mode::Numbering => Some(PATTERN_INPUT_ID),
            Mode::Case => None,
        }
    }

    /// The numbering start, when it parses.
    fn start(&self) -> Option<u64> {
        self.start_text.trim().parse().ok()
    }

    fn settings(&self) -> Settings {
        let transform = match self.mode {
            Mode::FindReplace => Transform::FindReplace {
                find: self.find.clone(),
                replace: self.replace.clone(),
                regex: self.regex,
                match_case: self.match_case,
            },
            Mode::Affix => Transform::Affix {
                prefix: self.prefix.clone(),
                suffix: self.suffix.clone(),
            },
            Mode::Numbering => Transform::Numbering {
                pattern: self.pattern.clone(),
                // A bad start is reported beside the field and blocks
                // Apply (see `can_apply`); 0 only keeps the preview alive.
                start: self.start().unwrap_or(0),
            },
            Mode::Case => Transform::Case(self.case),
        };
        Settings {
            transform,
            scope: if self.keep_extension {
                Scope::Stem
            } else {
                Scope::FullName
            },
        }
    }

    fn recompute(&mut self) {
        self.plan = Plan::compute(&self.request.selected, &self.others, &self.settings());
    }

    /// Whether the numbering start field is currently wrong.
    fn start_is_bad(&self) -> bool {
        self.mode == Mode::Numbering && self.start().is_none()
    }

    /// Apply is allowed.
    pub fn can_apply(&self) -> bool {
        self.plan.can_apply() && !self.start_is_bad()
    }

    /// The current plan, for tests and the view.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Applies one control change; `Some` when the dialog should close.
    pub fn update(&mut self, message: Message) -> Option<Outcome> {
        match message {
            Message::ModeSelected(mode) => self.mode = mode,
            Message::FindChanged(value) => self.find = value,
            Message::ReplaceChanged(value) => self.replace = value,
            Message::RegexToggled(value) => self.regex = value,
            Message::MatchCaseToggled(value) => self.match_case = value,
            Message::PrefixChanged(value) => self.prefix = value,
            Message::SuffixChanged(value) => self.suffix = value,
            Message::PatternChanged(value) => self.pattern = value,
            Message::StartChanged(value) => self.start_text = value,
            Message::CaseSelected(kind) => self.case = kind,
            Message::KeepExtensionToggled(value) => self.keep_extension = value,
            Message::CancelRequested => return Some(Outcome::Cancel),
            Message::ApplyRequested => {
                return self.can_apply().then(|| Outcome::Apply(self.plan.moves()));
            }
        }
        self.recompute();
        None
    }
}

// ── Wording ────────────────────────────────────────────────────────────────

fn lossy(name: &OsStr) -> String {
    name.to_string_lossy().into_owned()
}

/// The sentence for a row's problem, or `None` for a row without one.
fn problem_text(row: &bulk_rename::Row) -> Option<String> {
    let name = lossy(&row.new);
    match row.status {
        Status::Unchanged | Status::Ready => None,
        Status::Invalid(Invalid::Empty) => {
            Some(tr!("bulk-rename-problem-empty", name = lossy(&row.old)))
        }
        Status::Invalid(Invalid::DotName) => Some(tr!("bulk-rename-problem-dot", name = name)),
        Status::Invalid(Invalid::BadCharacter) => {
            Some(tr!("bulk-rename-problem-character", name = name))
        }
        Status::Invalid(Invalid::TooLong) => Some(tr!("bulk-rename-problem-long", name = name)),
        Status::Duplicate => Some(tr!("bulk-rename-problem-duplicate", name = name)),
        Status::Exists => Some(tr!("bulk-rename-problem-exists", name = name)),
    }
}

fn settings_error_text(error: &SettingsError) -> String {
    match error {
        SettingsError::BadRegex(detail) => tr!("bulk-rename-error-regex", detail = detail.as_str()),
        SettingsError::UnknownToken(token) => {
            tr!("bulk-rename-error-unknown-token", token = token.as_str())
        }
        SettingsError::UnclosedBrace => tr!("bulk-rename-error-unclosed"),
        SettingsError::StrayBrace => tr!("bulk-rename-error-stray"),
        SettingsError::BadWidth(width) => tr!("bulk-rename-error-width", width = width.as_str()),
    }
}

/// The one line under the preview: what is wrong, or what will happen.
fn status_line(dialog: &BulkRename) -> String {
    if let Some(error) = &dialog.plan.error {
        return settings_error_text(error);
    }
    if dialog.start_is_bad() {
        return tr!("bulk-rename-error-start");
    }
    if let Some(row) = dialog.plan.first_problem() {
        let reason = problem_text(row).unwrap_or_default();
        return tr!(
            "bulk-rename-problem-count",
            count = dialog.plan.problem_count(),
            reason = reason
        );
    }
    match dialog.plan.change_count() {
        0 => tr!("bulk-rename-status-nothing"),
        count => tr!("bulk-rename-status-ready", count = count),
    }
}

// ── View ───────────────────────────────────────────────────────────────────

/// Renders the dialog card. `App::view` supplies the scrim and centring.
pub fn view<'a>(t: &'a Theme, dialog: &'a BulkRename) -> Element<'a, Message> {
    let title = text(tr!(
        "bulk-rename-title",
        count = dialog.request.selected.len()
    ))
    .size(t.typography.size.dialog_title)
    .font(convert::display_font(t))
    .color(t.on_paper.primary.into_iced());

    let tabs = row![
        mode_tab(
            t,
            dialog,
            Mode::FindReplace,
            tr_static!("bulk-rename-mode-find")
        ),
        mode_tab(t, dialog, Mode::Affix, tr_static!("bulk-rename-mode-affix")),
        mode_tab(
            t,
            dialog,
            Mode::Numbering,
            tr_static!("bulk-rename-mode-number")
        ),
        mode_tab(t, dialog, Mode::Case, tr_static!("bulk-rename-mode-case")),
    ]
    .spacing(t.sizes.pill_gap / 2.0)
    .width(Fill);

    let keep_extension = checkbox(dialog.keep_extension)
        .label(tr_static!("bulk-rename-keep-extension"))
        .on_toggle(Message::KeepExtensionToggled)
        .style(style::toggles::checkbox(t, Surface::Paper))
        .text_size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t));

    let content = column![
        title,
        tabs,
        controls(t, dialog),
        keep_extension,
        preview(t, dialog),
        status(t, dialog),
        footer(t, dialog),
    ]
    .spacing(t.sizes.popover_padding / 2.0)
    .width(Length::Fixed(t.sizes.dialog_width * WIDTH_FACTOR));

    container(content)
        .style(style::dialog::surface(t))
        .padding(t.sizes.popover_padding)
        .into()
}

fn mode_tab<'a>(
    t: &'a Theme,
    dialog: &'a BulkRename,
    mode: Mode,
    label: &'a str,
) -> Element<'a, Message> {
    button(
        text(label)
            .size(t.typography.size.secondary)
            .font(convert::ui_font(t)),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        dialog.mode == mode,
    ))
    .padding(t.paddings.dialog_button)
    .on_press(Message::ModeSelected(mode))
    .into()
}

/// A single-line field in the dialog's one text style. `rejected` swaps in
/// the theme's rejected-field ring (a tint of the accent, not a new colour).
fn field<'a>(
    t: &'a Theme,
    placeholder: &'a str,
    value: &'a str,
    on_input: fn(String) -> Message,
    rejected: bool,
) -> text_input::TextInput<'a, Message> {
    let input = text_input(placeholder, value)
        .on_input(on_input)
        .on_submit(Message::ApplyRequested)
        .font(convert::mono_font(t))
        .size(t.typography.size.secondary)
        .width(Fill);
    if rejected {
        input.style(style::text_input::rejected(t, Surface::Paper))
    } else {
        input.style(style::text_input::rest(t, Surface::Paper))
    }
}

fn hint<'a>(t: &'a Theme, words: String) -> Element<'a, Message> {
    text(words)
        .size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t))
        .color(t.on_paper.secondary.into_iced())
        .into()
}

fn controls<'a>(t: &'a Theme, dialog: &'a BulkRename) -> Element<'a, Message> {
    let gap = t.sizes.pill_gap;
    match dialog.mode {
        Mode::FindReplace => {
            let bad_regex = matches!(dialog.plan.error, Some(SettingsError::BadRegex(_)));
            let toggles = row![
                checkbox(dialog.regex)
                    .label(tr_static!("bulk-rename-regex"))
                    .on_toggle(Message::RegexToggled)
                    .style(style::toggles::checkbox(t, Surface::Paper))
                    .text_size(t.typography.size.secondary)
                    .font(convert::ui_font_regular(t)),
                checkbox(dialog.match_case)
                    .label(tr_static!("bulk-rename-match-case"))
                    .on_toggle(Message::MatchCaseToggled)
                    .style(style::toggles::checkbox(t, Surface::Paper))
                    .text_size(t.typography.size.secondary)
                    .font(convert::ui_font_regular(t)),
            ]
            .spacing(gap * 2.0);
            let mut fields = column![
                field(
                    t,
                    tr_static!("bulk-rename-find-placeholder"),
                    &dialog.find,
                    Message::FindChanged,
                    bad_regex
                )
                .id(FIND_INPUT_ID),
                field(
                    t,
                    tr_static!("bulk-rename-replace-placeholder"),
                    &dialog.replace,
                    Message::ReplaceChanged,
                    false
                ),
            ];
            // Regex replacements expand groups; literal ones never do.
            if dialog.regex {
                fields = fields.push(hint(t, tr!("bulk-rename-regex-hint")));
            }
            fields = fields.push(toggles);
            fields.spacing(gap).into()
        }
        Mode::Affix => column![
            field(
                t,
                tr_static!("bulk-rename-prefix-placeholder"),
                &dialog.prefix,
                Message::PrefixChanged,
                false
            )
            .id(PREFIX_INPUT_ID),
            field(
                t,
                tr_static!("bulk-rename-suffix-placeholder"),
                &dialog.suffix,
                Message::SuffixChanged,
                false
            ),
        ]
        .spacing(gap)
        .into(),
        Mode::Numbering => {
            let bad_pattern = dialog.plan.error.is_some();
            column![
                field(
                    t,
                    tr_static!("bulk-rename-pattern-placeholder"),
                    &dialog.pattern,
                    Message::PatternChanged,
                    bad_pattern
                )
                .id(PATTERN_INPUT_ID),
                row![
                    text(tr!("bulk-rename-start-label"))
                        .size(t.typography.size.secondary)
                        .font(convert::ui_font_regular(t))
                        .color(t.on_paper.secondary.into_iced()),
                    field(
                        t,
                        "",
                        &dialog.start_text,
                        Message::StartChanged,
                        dialog.start_is_bad()
                    ),
                ]
                .spacing(gap)
                .align_y(Center),
                hint(t, tr!("bulk-rename-number-hint")),
            ]
            .spacing(gap)
            .into()
        }
        Mode::Case => row![
            case_button(
                t,
                dialog,
                CaseKind::Lower,
                tr_static!("bulk-rename-case-lower")
            ),
            case_button(
                t,
                dialog,
                CaseKind::Upper,
                tr_static!("bulk-rename-case-upper")
            ),
            case_button(
                t,
                dialog,
                CaseKind::Title,
                tr_static!("bulk-rename-case-title")
            ),
        ]
        .spacing(gap / 2.0)
        .into(),
    }
}

fn case_button<'a>(
    t: &'a Theme,
    dialog: &'a BulkRename,
    kind: CaseKind,
    label: &'a str,
) -> Element<'a, Message> {
    button(
        text(label)
            .size(t.typography.size.secondary)
            .font(convert::ui_font(t)),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        dialog.case == kind,
    ))
    .padding(t.paddings.dialog_button)
    .on_press(Message::CaseSelected(kind))
    .into()
}

/// The old -> new list: two short lines per item (old name, then an arrow
/// and the new name) so long names have the dialog's full width, and a
/// worded problem under any row that can't be applied.
fn preview<'a>(t: &'a Theme, dialog: &'a BulkRename) -> Element<'a, Message> {
    let mut rows: Vec<Element<'a, Message>> = vec![widget::section_label(
        t,
        Surface::Paper,
        tr_static!("bulk-rename-preview-label"),
    )];
    for row_data in dialog.plan.rows.iter().take(PREVIEW_ROWS) {
        rows.push(preview_row(t, row_data));
    }
    // Validation covers every item; say so when the list is cut short.
    let hidden = dialog.plan.rows.len().saturating_sub(PREVIEW_ROWS);
    if hidden > 0 {
        rows.push(hint(t, tr!("bulk-rename-preview-more", count = hidden)));
    }
    container(
        scrollable(column(rows).spacing(t.sizes.gap_tight).width(Fill))
            .style(style::scrollable::rest(t, Surface::Paper))
            .width(Fill)
            .height(Length::Fixed(PREVIEW_HEIGHT)),
    )
    .style(style::container::inset(t, Surface::Paper))
    .padding(t.sizes.gap_tight)
    .into()
}

fn preview_row<'a>(t: &'a Theme, data: &'a bulk_rename::Row) -> Element<'a, Message> {
    let old = text(lossy(&data.old))
        .size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t))
        .color(t.on_paper.secondary.into_iced());

    let new_text = match data.status {
        Status::Unchanged => tr!("bulk-rename-unchanged"),
        _ => lossy(&data.new),
    };
    let new_color = match data.status {
        Status::Unchanged => t.on_paper.secondary.into_iced(),
        _ => t.on_paper.primary.into_iced(),
    };
    let new = row![
        icon::icon(Icon::ArrowRight, t.sizes.icon_row, new_color),
        text(new_text)
            .size(t.typography.size.secondary)
            .font(convert::ui_font(t))
            .color(new_color)
            .width(Fill),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);

    let mut lines = column![old, new];
    if let Some(problem) = problem_text(data) {
        lines = lines.push(
            text(problem)
                .size(t.typography.size.secondary)
                .font(convert::ui_font_regular(t))
                .color(t.on_paper.primary.into_iced())
                .width(Fill),
        );
    }
    lines.into()
}

fn status<'a>(t: &'a Theme, dialog: &'a BulkRename) -> Element<'a, Message> {
    text(status_line(dialog))
        .size(t.typography.size.secondary)
        .font(convert::ui_font_regular(t))
        .color(t.on_paper.secondary.into_iced())
        .width(Fill)
        .into()
}

fn footer<'a>(t: &'a Theme, dialog: &'a BulkRename) -> Element<'a, Message> {
    let cancel = button(
        text(tr!("bulk-rename-cancel"))
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper, Chrome::Window))
    .padding(t.paddings.dialog_button)
    .on_press(Message::CancelRequested);

    // Without `.on_press` iced draws the button Disabled, which is exactly
    // the "can't apply yet" look; the reason is in the status line above.
    let mut apply = button(
        row![
            icon::icon(Icon::Pencil, t.sizes.icon_row, t.palette.paper.into_iced()),
            text(tr!("bulk-rename-apply", count = dialog.plan.change_count()))
                .size(t.typography.size.body)
                .font(convert::ui_font(t)),
        ]
        .spacing(t.sizes.pill_gap)
        .align_y(Center),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        true,
    ))
    .padding(t.paddings.dialog_button);
    if dialog.can_apply() {
        apply = apply.on_press(Message::ApplyRequested);
    }

    widget::footer_strip(
        t,
        Surface::Paper,
        row![cancel, Space::new().width(Fill), apply]
            .width(Fill)
            .align_y(Center),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vfs::Location;

    fn request(selected: &[&str], others: &[&str]) -> BulkRenameRequest {
        BulkRenameRequest {
            dir: Location::local("/scratch"),
            selected: selected.iter().map(OsString::from).collect(),
            others: others.iter().map(OsString::from).collect(),
        }
    }

    fn typed(dialog: &mut BulkRename, messages: Vec<Message>) {
        for message in messages {
            assert_eq!(dialog.update(message), None);
        }
    }

    #[test]
    fn nothing_typed_means_apply_is_off() {
        let dialog = BulkRename::new(request(&["a", "b"], &[]));
        assert!(!dialog.can_apply());
        assert_eq!(status_line(&dialog), "Nothing to rename yet.");
    }

    #[test]
    fn numbering_through_the_dialog_gives_padded_names_in_order() {
        let selected: Vec<String> = (1..=20)
            .map(|n| format!("IMG_{:04}.jpg", 100 - n))
            .collect();
        let refs: Vec<&str> = selected.iter().map(String::as_str).collect();
        let mut dialog = BulkRename::new(request(&refs, &[]));
        typed(
            &mut dialog,
            vec![
                Message::ModeSelected(Mode::Numbering),
                Message::PatternChanged("Holiday {n:03}".into()),
            ],
        );
        assert!(dialog.can_apply());
        let moves = match dialog.update(Message::ApplyRequested) {
            Some(Outcome::Apply(moves)) => moves,
            other => panic!("expected Apply, got {other:?}"),
        };
        assert_eq!(moves.len(), 20);
        assert_eq!(moves[0].1, OsString::from("Holiday 001.jpg"));
        assert_eq!(moves[19].1, OsString::from("Holiday 020.jpg"));
    }

    #[test]
    fn a_collision_blocks_apply_with_a_worded_reason() {
        let mut dialog = BulkRename::new(request(&["a.txt"], &["b.txt"]));
        typed(
            &mut dialog,
            vec![
                Message::FindChanged("a".into()),
                Message::ReplaceChanged("b".into()),
            ],
        );
        assert!(!dialog.can_apply());
        assert_eq!(
            status_line(&dialog),
            "Nothing is renamed until this is fixed: “b.txt” already exists in this folder."
        );
        // Apply while blocked does nothing.
        assert_eq!(dialog.update(Message::ApplyRequested), None);
    }

    #[test]
    fn duplicates_are_counted_in_the_status_line() {
        let mut dialog = BulkRename::new(request(&["a1", "a2"], &[]));
        typed(
            &mut dialog,
            vec![
                Message::RegexToggled(true),
                Message::FindChanged(r"\d".into()),
            ],
        );
        assert!(!dialog.can_apply());
        assert!(status_line(&dialog).starts_with("Nothing is renamed until these 2 problems"));
    }

    #[test]
    fn a_bad_regex_is_worded_and_blocks_apply() {
        let mut dialog = BulkRename::new(request(&["a"], &[]));
        typed(
            &mut dialog,
            vec![
                Message::RegexToggled(true),
                Message::FindChanged("(".into()),
            ],
        );
        assert!(!dialog.can_apply());
        assert!(status_line(&dialog).starts_with("That isn't a valid regular expression"));
    }

    #[test]
    fn a_bad_numbering_start_blocks_apply() {
        let mut dialog = BulkRename::new(request(&["a", "b"], &[]));
        typed(
            &mut dialog,
            vec![
                Message::ModeSelected(Mode::Numbering),
                Message::PatternChanged("{n}".into()),
                Message::StartChanged("x".into()),
            ],
        );
        assert!(!dialog.can_apply());
        assert_eq!(
            status_line(&dialog),
            "The starting number must be a whole number."
        );
    }

    #[test]
    fn cancel_closes_without_renaming() {
        let mut dialog = BulkRename::new(request(&["a", "b"], &[]));
        assert_eq!(
            dialog.update(Message::CancelRequested),
            Some(Outcome::Cancel)
        );
    }

    #[test]
    fn keep_extension_off_lets_the_extension_change() {
        let mut dialog = BulkRename::new(request(&["a.jpeg", "b.jpeg"], &[]));
        typed(
            &mut dialog,
            vec![
                Message::KeepExtensionToggled(false),
                Message::FindChanged(".jpeg".into()),
                Message::ReplaceChanged(".jpg".into()),
            ],
        );
        assert_eq!(
            dialog.plan().moves(),
            vec![
                (OsString::from("a.jpeg"), OsString::from("a.jpg")),
                (OsString::from("b.jpeg"), OsString::from("b.jpg")),
            ]
        );
    }

    #[test]
    fn every_mode_draws_without_panicking() {
        let t = Theme::saola();
        let mut dialog = BulkRename::new(request(&["a.txt", "b.txt"], &["c.txt"]));
        for mode in [Mode::FindReplace, Mode::Affix, Mode::Numbering, Mode::Case] {
            typed(&mut dialog, vec![Message::ModeSelected(mode)]);
            let _ = view(&t, &dialog);
        }
    }
}
