//! The portal seam: composes the navigation toolbar (`ui::header`, which
//! itself renders `ui::breadcrumbs`) above the active directory view
//! behind one function, free of app-window concerns (no window title bar,
//! no close button — that's `ui::window`'s job). This is the surface a
//! future saola-portal embeds directly as the file picker.
//!
//! A places sidebar (Stage 7) still isn't built; this stage fills in the
//! breadcrumb bar the Stage 3 module docs left room for.
//!
//! State ownership stays on the app (`main.rs` holds `Vec<DirectoryView> +
//! active` — the tabs seam), not here: this module is deliberately
//! stateless so a portal embedding it doesn't inherit tab bookkeeping it
//! doesn't want. `ui::header`/`ui::breadcrumbs` are equally stateless —
//! everything they need (history stacks, the path-edit buffer, view mode)
//! lives on the `DirectoryView` they're passed.

use iced::widget::column;
use iced::{Element, Fill};
use saola_theme::Theme;

use crate::core::apps::AppsDb;
use crate::core::mime::MimeDb;
use crate::ui::dirview::{self, DirectoryView};
use crate::ui::header;

/// Render `active` (the app's currently-shown `DirectoryView`) with its
/// toolbar, lifting messages into the caller's `M` via `map`.
///
/// `mime_db`/`apps_db` are the App-level shared caches (CLAUDE.md: "Shared
/// caches (thumbs, mime, apps, …) live on the App, never per-view") this
/// stage introduces — threaded straight through to `active.view` for row
/// glyph selection and the context menu/Open-with popover, never built or
/// cached here.
///
/// Built as one `Element<'a, dirview::Message>` tree first and mapped
/// exactly once at the end, rather than calling `.map(map)` on the header
/// and body separately — `map` is `impl Fn(...) -> M`, not required to be
/// `Copy`, so it can only be consumed once.
pub fn view<'a, M: 'a>(
    theme: &'a Theme,
    active: &'a DirectoryView,
    mime_db: &'a MimeDb,
    apps_db: &'a AppsDb,
    map: impl Fn(dirview::Message) -> M + 'a,
) -> Element<'a, M> {
    let content: Element<'a, dirview::Message> = column![
        header::view(theme, active),
        active.view(theme, mime_db, apps_db)
    ]
    .width(Fill)
    .height(Fill)
    .into();
    content.map(map)
}
