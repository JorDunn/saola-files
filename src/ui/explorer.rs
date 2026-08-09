//! The portal seam: composes the places sidebar (`ui::sidebar`) beside the
//! navigation toolbar (`ui::header`, which itself renders
//! `ui::breadcrumbs`) above the active directory view, behind one
//! function, free of app-window concerns (no window title bar, no close
//! button — that's `ui::window`'s job). This is the surface a future
//! saola-portal embeds directly as the file picker.
//!
//! State ownership stays on the app (`main.rs` holds `Vec<DirectoryView> +
//! active`, plus now a `ui::sidebar::Sidebar` — the tabs seam extended,
//! not replaced), not here: this module is deliberately stateless so a
//! portal embedding it doesn't inherit tab/sidebar bookkeeping it doesn't
//! want. `ui::header`/`ui::breadcrumbs`/`ui::sidebar` are equally
//! stateless as far as *this* module is concerned — each owns whatever
//! state it needs on the value it's passed, not here.
//!
//! [`Message`] is this module's own nested-enum seam (CLAUDE.md's
//! messages rule: `Message::Sidebar(sidebar::Message)`) — introduced this
//! stage because there are now two independent child message types
//! (`sidebar::Message`, `dirview::Message`) to route, where Stage 3–6 only
//! ever had the one.

use iced::widget::{column, row};
use iced::{Element, Fill};
use saola_theme::Theme;

use crate::core::apps::AppsDb;
use crate::core::mime::MimeDb;
use crate::ui::dirview::{self, DirectoryView};
use crate::ui::header;
use crate::ui::sidebar::{self, Sidebar};

/// This seam's message type — `main.rs` nests it as
/// `Message::Explorer(explorer::Message)` and its `App::update` delegates
/// by pattern-matching through both layers, the same shape every other
/// per-module `Message` in this crate already follows.
#[derive(Debug, Clone)]
pub enum Message {
    Sidebar(sidebar::Message),
    Directory(dirview::Message),
}

/// Render the sidebar beside `active` (the app's currently-shown
/// `DirectoryView`) with its toolbar, lifting messages into the caller's
/// `M` via `map`.
///
/// `mime_db`/`apps_db` are the App-level shared caches (CLAUDE.md: "Shared
/// caches (thumbs, mime, apps, …) live on the App, never per-view"),
/// threaded straight through to `active.view` for row glyph selection and
/// the context menu/Open-with popover, never built or cached here.
/// `clipboard_has_contents` (Stage 8) is the same shape: `App` owns the
/// actual `core::fs::ops::Clipboard`, this seam only ever reads a bool off
/// it so the context menu's Paste row can be capability-honest about
/// whether there's anything to paste.
///
/// Built as one `Element<'a, Message>` tree first (the sidebar and the
/// header+directory column each mapped into `Message` exactly once) and
/// mapped into `M` exactly once at the end, rather than calling `.map(map)`
/// more than once anywhere — `map` is `impl Fn(...) -> M`, not required to
/// be `Copy`, so it can only be consumed once; the two `Message::Sidebar`/
/// `Message::Directory` constructors used per-subtree are plain `Fn`s and
/// have no such restriction.
pub fn view<'a, M: 'a>(
    theme: &'a Theme,
    sidebar: &'a Sidebar,
    active: &'a DirectoryView,
    mime_db: &'a MimeDb,
    apps_db: &'a AppsDb,
    clipboard_has_contents: bool,
    map: impl Fn(Message) -> M + 'a,
) -> Element<'a, M> {
    let sidebar_view: Element<'a, Message> =
        sidebar.view(theme, active.location()).map(Message::Sidebar);

    let directory: Element<'a, dirview::Message> = column![
        header::view(theme, active),
        active.view(theme, mime_db, apps_db, clipboard_has_contents)
    ]
    .width(Fill)
    .height(Fill)
    .into();
    let directory: Element<'a, Message> = directory.map(Message::Directory);

    let content: Element<'a, Message> = row![sidebar_view, directory]
        .width(Fill)
        .height(Fill)
        .into();
    content.map(map)
}
