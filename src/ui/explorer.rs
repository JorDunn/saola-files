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
use saola_theme::{Surface, Theme};

use crate::core::apps::AppsDb;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
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
/// `s` is the window's ground (`files.toml`'s `surface` knob, resolved on
/// `App` at startup): every region composed here — sidebar, toolbar,
/// listing — is anchored to the window frame, so all three follow it
/// together. It is render context, not state: a portal embedding this seam
/// passes whatever surface *its* own window is drawn on.
///
/// `mime_db`/`thumb_cache`/`apps_db` are the App-level shared caches
/// (CLAUDE.md: "Shared caches (thumbs, mime, apps, …) live on the App,
/// never per-view"), threaded straight through to `active.view` for row
/// glyph/thumbnail selection and the context menu/Open-with popover, never
/// built or cached here.
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
// Stage 11 pushed this from 7 params to 8 (`thumb_cache` joined the other
// App-owned shared caches this seam threads straight through), and the
// `surface` knob makes 9 (`s`, the window's ground — see the doc comment
// just below). `clippy::too_many_arguments`'s default threshold is 7;
// every one of these is a distinct, differently-typed shared cache or
// piece of render context — `theme` and `s` together are *how to draw*,
// the caches are *what to draw from* — and this function's own doc
// comment already explains the purpose of each, so bundling them into a
// single "context" struct would be indirection for its own sake
// (CLAUDE.md: "prefer explicit code over clever abstraction") rather than
// a real simplification — every call site would still have to name each
// field individually to build it.
#[allow(clippy::too_many_arguments)]
pub fn view<'a, M: 'a>(
    theme: &'a Theme,
    s: Surface,
    sidebar: &'a Sidebar,
    active: &'a DirectoryView,
    mime_db: &'a MimeDb,
    thumb_cache: &'a ThumbCache,
    apps_db: &'a AppsDb,
    clipboard_has_contents: bool,
    map: impl Fn(Message) -> M + 'a,
) -> Element<'a, M> {
    let sidebar_view: Element<'a, Message> = sidebar
        .view(theme, s, active.location())
        .map(Message::Sidebar);

    // Region geometry lives here, in the one place that knows all three
    // regions exist. `sizes.island_gap` ("gap between islands") is the gap
    // token the panel already uses between its own free-standing chrome
    // surfaces, and that is exactly the job here: the places sidebar and
    // the toolbar are two inset chrome islands, the directory listing is
    // the window's own ground they float on. One token, used as the gap *between*
    // the regions (row `spacing`), the gap between toolbar and listing
    // (column `spacing`), and the inset from the window's own edges (row
    // `padding`), so every seam in the window measures the same.
    let gap = theme.sizes.island_gap;

    let directory: Element<'a, dirview::Message> = column![
        header::view(theme, s, active),
        active.view(
            theme,
            s,
            mime_db,
            thumb_cache,
            apps_db,
            clipboard_has_contents
        )
    ]
    .spacing(gap)
    .width(Fill)
    .height(Fill)
    .into();
    let directory: Element<'a, Message> = directory.map(Message::Directory);

    let content: Element<'a, Message> = row![sidebar_view, directory]
        .spacing(gap)
        .padding(gap)
        .width(Fill)
        .height(Fill)
        .into();
    content.map(map)
}
