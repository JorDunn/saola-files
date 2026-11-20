//! The portal seam: composes the places sidebar (`ui::sidebar`) beside the
//! navigation toolbar (`ui::header`, which itself renders
//! `ui::breadcrumbs`) above the active directory view, behind one
//! function, free of app-window concerns (no window title bar, no close
//! button — that's `ui::window`'s job). This is the surface a future
//! saola-portal embeds directly as the file picker.
//!
//! State ownership stays on the app (`src/app/mod.rs` holds
//! `Vec<(ViewId, DirectoryView)>` plus the focused [`ViewId`], and a
//! `ui::sidebar::Sidebar`), not here: this module is deliberately stateless so a
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
//!
//! **View ids.** Every `DirectoryView` the app holds is paired with a
//! [`ViewId`], and every directory message carries the id of the view it
//! is for (`Message::Directory(ViewId, _)`). Async work — a listing, a
//! rename result, a watch refresh — finishes later than it starts, and by
//! then the view that asked may have moved within the list or been closed.
//! A `Vec` index would then point at the wrong view (or none); an id that
//! is never reused can only ever match the view it was minted for, or
//! nothing, in which case the message is dropped ([`find_mut`] returns
//! `None`).

use iced::widget::{column, row};
use iced::{Element, Fill};
use saola_theme::{Surface, Theme};

use crate::core::apps::AppsDb;
use crate::core::mime::MimeDb;
use crate::core::thumbs::ThumbCache;
use crate::ui::dirview::{self, DirectoryView};
use crate::ui::header;
use crate::ui::sidebar::{self, Sidebar};

/// This seam's message type — the app nests it as
/// `Message::Explorer(explorer::Message)` and its `App::update` delegates
/// by pattern-matching through both layers, the same shape every other
/// per-module `Message` in this crate already follows.
#[derive(Debug, Clone)]
pub enum Message {
    Sidebar(sidebar::Message),
    /// A message for the directory view with this id. See the module
    /// docs' "View ids" section.
    Directory(ViewId, dirview::Message),
}

/// A directory view's identity: stable for the view's whole life and never
/// handed out again after it closes. Deliberately **not** the view's
/// position in the app's list — see the module docs' "View ids" section.
///
/// The `u64` inside is private, so the only way to get a `ViewId` is from a
/// [`ViewIdSource`]; nothing can make one up from a number.
/// `Hash` is needed because a view's watch subscription is tagged with its
/// id (`Subscription::with`), and iced tells subscriptions apart by hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId(u64);

/// Hands out [`ViewId`]s: 0, 1, 2, … — one per `App`, the same
/// "one counter, never per-view" shape as `ops::OpIdSource`.
#[derive(Debug, Default)]
pub struct ViewIdSource {
    next: u64,
}

impl ViewIdSource {
    /// The next unused id. Wrapping (rather than `+ 1`, which panics on
    /// overflow in debug builds) keeps this panic-free; reaching 2^64 views
    /// in one session is not a real possibility, so ids never repeat in
    /// practice.
    pub fn alloc(&mut self) -> ViewId {
        let id = ViewId(self.next);
        self.next = self.next.wrapping_add(1);
        id
    }
}

/// The view with id `id`, if it is still open. Generic over the view type
/// so the routing rule can be unit-tested without building real
/// `DirectoryView`s.
pub fn find<V>(views: &[(ViewId, V)], id: ViewId) -> Option<&V> {
    views
        .iter()
        .find(|(view_id, _)| *view_id == id)
        .map(|(_, view)| view)
}

/// Mutable [`find`]: how a message addressed to `id` reaches its view.
/// `None` means that view has closed; the caller drops the message.
pub fn find_mut<V>(views: &mut [(ViewId, V)], id: ViewId) -> Option<&mut V> {
    views
        .iter_mut()
        .find(|(view_id, _)| *view_id == id)
        .map(|(_, view)| view)
}

/// Render the sidebar beside `active` (the app's currently-shown
/// `DirectoryView`, whose id is `active_id`) with its toolbar, lifting
/// messages into the caller's `M` via `map`.
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
// above), and `active_id` makes 10. `clippy::too_many_arguments`'s default threshold is 7;
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
    active_id: ViewId,
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
    // `move` copies `active_id` (a `Copy` type) into the closure, so the
    // closure owns its own copy and does not borrow this function's local.
    let directory: Element<'a, Message> =
        directory.map(move |message| Message::Directory(active_id, message));

    let content: Element<'a, Message> = row![sidebar_view, directory]
        .spacing(gap)
        .padding(gap)
        .width(Fill)
        .height(Fill)
        .into();
    content.map(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_ids_are_monotonic_and_never_reused() {
        let mut ids = ViewIdSource::default();
        let first = ids.alloc();
        let second = ids.alloc();
        let third = ids.alloc();
        assert_eq!((first.0, second.0, third.0), (0, 1, 2));
        assert_ne!(first, second);
        assert_ne!(second, third);
    }

    #[test]
    fn routing_reaches_the_view_with_that_id_not_the_one_at_that_index() {
        let mut ids = ViewIdSource::default();
        let a = ids.alloc();
        let b = ids.alloc();
        // Deliberately stored out of id order: `b` sits at index 0.
        let mut views = vec![(b, "b"), (a, "a")];
        assert_eq!(find(&views, a), Some(&"a"));
        if let Some(view) = find_mut(&mut views, b) {
            *view = "b, updated";
        }
        assert_eq!(views, vec![(b, "b, updated"), (a, "a")]);
    }

    #[test]
    fn a_message_for_a_closed_view_finds_nothing() {
        let mut ids = ViewIdSource::default();
        let a = ids.alloc();
        let b = ids.alloc();
        let mut views = vec![(a, "a"), (b, "b")];
        // Close `a`; a late result addressed to it must not land on `b`,
        // which now sits at `a`'s old index.
        views.retain(|(id, _)| *id != a);
        assert!(find_mut(&mut views, a).is_none());
        assert_eq!(find(&views, b), Some(&"b"));
        // A view opened afterwards gets a fresh id, never `a`'s.
        let c = ids.alloc();
        assert_ne!(c, a);
    }
}
