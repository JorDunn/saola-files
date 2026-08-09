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
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::vfs::Location;
use crate::icons::{self, Icon};
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
        segments.push(pill(
            t,
            format!("{}://{authority}", location.scheme),
            root.clone(),
            false,
        ));
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
    segments.push(pill(t, "/".to_owned(), root, is_root));

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
        segments.push(pill(
            t,
            part.to_string_lossy().into_owned(),
            target,
            index == last_index,
        ));
    }

    let trail = row(segments).spacing(4.0).align_y(Center);

    // The edit-pencil gives a mouse-only path into `Action::EditPath` —
    // Ctrl+L is the primary trigger, this is parity for anyone not
    // driving the keyboard.
    let edit = button(icons::icon(
        Icon::Pencil,
        t.sizes.icon_row,
        t.on_paper.primary.into_iced(),
    ))
    .style(style::button::bare(t, Surface::Paper))
    .padding([4.0, 10.0])
    .on_press(dirview::Message::Action(Action::EditPath));

    row![container(trail).width(Fill), edit]
        .align_y(Center)
        .width(Fill)
        .into()
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
    // `button::rest`/`button::active` are two distinct opaque closure
    // types (see saola-theme's `style::segmented` docs for the same
    // constraint) — branch on the whole button, not just the style
    // argument, so each arm's `.style(...)` call picks a single concrete
    // type before `.padding`/`.on_press` unify them back into one
    // `Button<...>`.
    let styled = if is_current {
        button(content).style(style::button::active(t, Surface::Paper))
    } else {
        button(content).style(style::button::bare(t, Surface::Paper))
    };
    styled
        .padding([4.0, 10.0])
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
