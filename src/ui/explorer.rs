//! The portal seam: composes the active directory view behind one
//! function, free of app-window concerns (no header, no close button —
//! that's `ui::window`'s job). This is the surface a future saola-portal
//! embeds directly as the file picker.
//!
//! Sidebar (Stage 7) and breadcrumbs (Stage 4) aren't built yet; this
//! stage renders only the active `DirectoryView`, leaving the layout room
//! those stages fill in (a places sidebar to the left, a breadcrumb bar
//! above the list).
//!
//! State ownership stays on the app (`main.rs` holds `Vec<DirectoryView> +
//! active` — the tabs seam), not here: this module is deliberately
//! stateless so a portal embedding it doesn't inherit tab bookkeeping it
//! doesn't want.

use iced::Element;
use saola_theme::Theme;

use crate::ui::dirview::{self, DirectoryView};

/// Render `active` (the app's currently-shown `DirectoryView`), lifting
/// its messages into the caller's `M` via `map`.
pub fn view<'a, M: 'a>(
    theme: &'a Theme,
    active: &'a DirectoryView,
    map: impl Fn(dirview::Message) -> M + 'a,
) -> Element<'a, M> {
    active.view(theme).map(map)
}
