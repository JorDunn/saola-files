//! Breadcrumb pills for the active [`DirectoryView`]'s location, and the
//! Ctrl+L "edit as text" mode that swaps them for an editable path/URI
//! [`text_input`].
//!
//! Lives outside `ui::dirview`'s module tree (unlike `dirview::list`/
//! `dirview::grid`, which sit inside it and can reach `DirectoryView`'s
//! private fields): this only ever reads `DirectoryView`'s public surface
//! (`location()`/`path_edit()`) and constructs its `Message` values, the
//! same boundary `ui::explorer`/`ui::header` already respect.

use std::path::{Component, PathBuf};

use iced::widget::{button, container, row, text, text_input};
use iced::{Center, Element, Fill};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style, widget};

use crate::core::vfs::Location;
use crate::keymap::Action;
use crate::ui::dirview::{self, DirectoryView};

/// Widget id for the path/URI editor. `ui::dirview`'s `Action::EditPath`
/// handler focuses and select-alls this exact id when Ctrl+L (or the
/// header's edit-pencil button) fires — the id has to be shared between
/// "who builds the widget" (here) and "who issues the focus/select_all
/// operations" (`DirectoryView::apply_action`), so it's `pub` rather than
/// private to this module.
pub const PATH_INPUT_ID: &str = "saola-files-breadcrumb-path-input";

/// Renders either the breadcrumb pill trail or (while `state.path_edit()`
/// is `Some`) the editable field it swaps for.
pub fn view<'a>(t: &'a Theme, state: &'a DirectoryView) -> Element<'a, dirview::Message> {
    match state.path_edit() {
        Some(buffer) => editor(t, buffer),
        None => pills(t, state.location()),
    }
}

fn editor<'a>(t: &'a Theme, buffer: &'a str) -> Element<'a, dirview::Message> {
    text_input("scheme://host/path or /a/local/path", buffer)
        .id(PATH_INPUT_ID)
        .on_input(dirview::Message::PathInputChanged)
        .on_submit(dirview::Message::PathSubmitted)
        .style(style::text_input::rest(t, Surface::Paper))
        .font(convert::mono_font(t))
        .size(t.typography.size.secondary)
        .width(Fill)
        .into()
}

/// Stage 12: each pill's styling is now `style::button::breadcrumb` (the
/// upstreamed promotion of this function's own hand-rolled `active`/`bare`
/// branch) with `paddings.breadcrumb`, separated by a `chevron-right` glyph
/// in the `quaternary` role — the exact recipe `widget::breadcrumb`
/// documents. This crate can't call that composite constructor directly:
/// its crumb labels are computed fresh every `view()` from `Location`
/// components (`to_string_lossy`/`format!` all produce owned `String`s), and
/// `widget::breadcrumb` needs `&'a str` slices that outlive the returned
/// `Element<'a, _>` — a bound only `'static`/pre-stored labels can satisfy.
/// So `pill` stays a local per-crumb builder (owning its `text(label)` the
/// same way the pre-Stage-12 version did) while still adopting the shared
/// style/padding/separator pieces.
fn pills<'a>(t: &'a Theme, location: &'a Location) -> Element<'a, dirview::Message> {
    let mut segments: Vec<Element<'a, dirview::Message>> = Vec::new();

    let root = Location {
        scheme: location.scheme.clone(),
        authority: location.authority.clone(),
        path: PathBuf::from("/"),
    };

    // A remote location gets a leading identity pill ("sftp://host") ahead
    // of the ordinary path trail — the style guide's "renders an authority
    // pill for remote locations" requirement. It jumps to that authority's
    // root, same as the plain "/" pill below.
    if let Some(authority) = &location.authority {
        push_pill(
            t,
            &mut segments,
            format!("{}://{authority}", location.scheme),
            root.clone(),
            false,
        );
    }

    let components: Vec<_> = location
        .path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();

    let is_root = components.is_empty();
    push_pill(t, &mut segments, "/".to_owned(), root, is_root);

    let mut cumulative = PathBuf::from("/");
    let last_index = components.len().saturating_sub(1);
    for (index, part) in components.iter().enumerate() {
        cumulative.push(part);
        let target = Location {
            scheme: location.scheme.clone(),
            authority: location.authority.clone(),
            path: cumulative.clone(),
        };
        // `to_string_lossy` here is the same sanctioned display-time
        // conversion `FileEntry::display_name` uses — a breadcrumb label
        // is inherently human-facing text, not the path identity itself
        // (which stays in `target.path` as a real `PathBuf`).
        push_pill(
            t,
            &mut segments,
            part.to_string_lossy().into_owned(),
            target,
            index == last_index,
        );
    }

    let trail = row(segments).spacing(t.sizes.pill_gap).align_y(Center);

    // The edit-pencil gives a mouse-only path into `Action::EditPath` —
    // Ctrl+L is the primary trigger, this is parity for anyone not
    // driving the keyboard.
    let edit = widget::icon_button(
        t,
        Surface::Paper,
        Icon::Pencil,
        None,
        t.on_paper.primary.into_iced(),
        Some(dirview::Message::Action(Action::EditPath)),
    );

    row![container(trail).width(Fill), edit]
        .align_y(Center)
        .width(Fill)
        .into()
}

/// Pushes one crumb pill onto `segments`, followed by a
/// `Icon::ChevronRight` separator glyph in the `quaternary` role when
/// another crumb follows — `widget::breadcrumb`'s exact separator recipe
/// (see its doc comment), inlined here since the composite constructor
/// itself can't be called (see [`pills`]'s doc comment).
fn push_pill<'a>(
    t: &'a Theme,
    segments: &mut Vec<Element<'a, dirview::Message>>,
    label: String,
    target: Location,
    is_current: bool,
) {
    if !segments.is_empty() {
        let separator_tint = t.on_paper.quaternary.into_iced();
        segments.push(icon::icon(Icon::ChevronRight, t.sizes.icon_bar, separator_tint).into());
    }
    segments.push(pill(t, label, target, is_current));
}

fn pill<'a>(
    t: &'a Theme,
    label: String,
    target: Location,
    is_current: bool,
) -> Element<'a, dirview::Message> {
    let content = text(label)
        .size(t.typography.size.secondary)
        .font(convert::ui_font(t));
    button(content)
        .style(style::button::breadcrumb(t, Surface::Paper, is_current))
        .padding(t.paddings.breadcrumb)
        .on_press(dirview::Message::BreadcrumbClicked(target))
        .into()
}

// `pills`/`editor` build `Element`s directly and have no pure logic of
// their own to unit test in isolation (no iced test harness renders a
// tree here) — the behavior they wire up (breadcrumb click ->
// `Event::OpenDirectory`, path submit -> parse -> navigate, the Ctrl+L
// round trip) is exercised in `ui::dirview::mod`'s tests instead, where
// `DirectoryView::update`/`apply_action` are directly reachable without
// constructing a real render tree.
