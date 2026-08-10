//! The places sidebar: a §6-list-row column of "here's a shortcut"
//! entries, composed into `ui::explorer` beside the header+directory-view
//! column (portal-seam discipline — CLAUDE.md: `ui::explorer` is "free of
//! app-window concerns", and this module follows the same rule
//! `ui::header`/`ui::breadcrumbs` already do: state lives on the value the
//! caller hands in, no app-window types appear anywhere here).
//!
//! Two independent sources feed it, kept apart on purpose:
//!
//! - [`crate::core::places::Place`]s — home, XDG user dirs, bookmarks,
//!   saved servers, trash — computed once at startup (`main.rs::App::new`,
//!   mirroring how `mime_db`/`apps_db` are built once) and handed to
//!   [`Sidebar::new`]. Nothing here re-reads the bookmarks/`user-dirs.dirs`
//!   files; a later "add bookmark" action would re-run
//!   `core::places::build` and replace `self.places` wholesale, the same
//!   shape [`Message::MountsUpdated`] already uses for the live side.
//! - [`crate::core::udisks::Mount`]s — a *live*, D-Bus-fed section
//!   rendered separately underneath. `Sidebar` owns the running
//!   `Vec<Mount>`; [`Message::MountsUpdated`] replaces it wholesale each
//!   time the udisks worker emits a fresh snapshot (see `core::udisks`'s
//!   module docs on why whole-snapshot-replace, not fine-grained add/
//!   remove events, is what actually crosses the D-Bus boundary). No
//!   udisks on the bus, or nothing mounted, both leave this empty and the
//!   "Removable" section simply doesn't render — CLAUDE.md's degrade-to-
//!   nothing rule, exercised here exactly the way a backend without
//!   `Caps::WATCH` degrades the header's refresh affordance.

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Subscription};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style, widget};

use crate::core::places::Place;
use crate::core::udisks::{Mount, MountsSource, UdisksMounts};
use crate::core::vfs::Location;

#[derive(Debug, Clone)]
pub enum Message {
    PlaceClicked(Location),
    MountClicked(Location),
    MountsUpdated(Vec<Mount>),
}

/// What the sidebar asks its owner to do — the same "the view only ever
/// requests, the owner acts" shape `ui::dirview::Event` uses (see that
/// module's docs on why the actual navigation happens one layer up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    OpenDirectory(Location),
}

/// The sidebar's state: the (rare-to-change) place list plus the
/// (frequently-updated) live mount list.
pub struct Sidebar {
    places: Vec<Place>,
    mounts: Vec<Mount>,
}

impl Sidebar {
    pub fn new(places: Vec<Place>) -> Self {
        Sidebar {
            places,
            mounts: Vec::new(),
        }
    }

    /// Read-only access for `main.rs`/tests that want to inspect what a
    /// `Sidebar` was built with, without reaching into a private field.
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    pub fn update(&mut self, message: Message) -> Option<Event> {
        match message {
            Message::PlaceClicked(location) | Message::MountClicked(location) => {
                Some(Event::OpenDirectory(location))
            }
            Message::MountsUpdated(mounts) => {
                self.mounts = mounts;
                None
            }
        }
    }

    /// `current` is the active `DirectoryView`'s location — the row (place
    /// or mount) that matches it draws selected, the same terracotta §6
    /// list-row treatment `ui::dirview::list`'s selected rows use.
    pub fn view<'a>(&'a self, t: &'a Theme, current: &Location) -> Element<'a, Message> {
        let mut rows: Vec<Element<'a, Message>> = self
            .places
            .iter()
            .map(|place| place_row(t, place, place.location == *current))
            .collect();

        if !self.mounts.is_empty() {
            rows.push(widget::section_label(t, Surface::Paper, "REMOVABLE"));
            rows.extend(self.mounts.iter().map(|mount| {
                let location = Location::local(mount.mount_point.clone());
                mount_row(t, mount, location == *current)
            }));
        }

        let content = column(rows).width(Fill);

        // Region separation by *ground*, not by a line: `style::container::
        // inset` (Stage 12 — the upstreamed promotion of the pending
        // `container::tile`-at-`radii.inset` gap this call site used to
        // flag) is `on_paper.fill_subtle` (ink at 4%) at `radii.inset`
        // (20px, the style guide §4 "Inset panels, media rows | 18–22px"
        // tier — `container::tile`'s own `radii.tile`, 13px, is for *icon*
        // tiles, one size class down from a window-scale panel like this
        // one). The places column sits one alpha step below the file
        // listing's plain paper, the same way the quick-settings popover's
        // media row sits below its popover — keeping the three-colour rule
        // intact (a step of ink, not a fourth hue) while making "sidebar"
        // and "files" read as two zones rather than one continuous sheet.
        //
        // `ui::explorer` (and `main.rs`'s trash composition) is what insets
        // this panel from the window edges with `sizes.island_gap`; the
        // panel only owns its own inner breathing room here.
        //
        // `sizes.window_sidebar` (200, Stage 12) replaces this file's own
        // `SIDEBAR_WIDTH` local constant.
        container(
            scrollable(content)
                .style(style::scrollable::rest(t, Surface::Paper))
                .width(Fill)
                .height(Fill),
        )
        .style(style::container::inset(t, Surface::Paper))
        .padding(t.sizes.pill_gap)
        .width(t.sizes.window_sidebar)
        .height(Fill)
        .into()
    }

    /// The udisks live feed as an iced subscription — `ui::explorer`/
    /// `main.rs` batches this beside the active view's own directory
    /// watch. `Subscription::run` (not `run_with`, unlike a directory
    /// watch keyed by `Location`): there's exactly one udisks feed for the
    /// app's whole lifetime, so a bare function pointer is its own stable
    /// identity across re-renders — see `saola-panel`'s `battery.rs` doc
    /// comment for the fuller teaching note on why a `fn` pointer alone is
    /// enough for `Subscription::run`'s identity.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run(mounts_stream)
    }
}

/// Bridges `core::udisks`'s iced-free `BoxStream` into an
/// `iced::futures::Stream` of `Message`s — the one place this module
/// touches `core::udisks` directly; everything above only ever sees
/// `Message::MountsUpdated`. `UdisksMounts` is always the real
/// implementation here (there is exactly one udisks feed in the running
/// app); `core::udisks`'s own tests are what exercise `MountsSource`
/// through the fake.
fn mounts_stream() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::stream::StreamExt;
    UdisksMounts.watch().map(Message::MountsUpdated)
}

fn place_row<'a>(t: &'a Theme, place: &'a Place, selected: bool) -> Element<'a, Message> {
    row_button(
        t,
        crate::icons::for_place(place.kind),
        &place.label,
        selected,
        Message::PlaceClicked(place.location.clone()),
    )
}

fn mount_row<'a>(t: &'a Theme, mount: &'a Mount, selected: bool) -> Element<'a, Message> {
    row_button(
        t,
        crate::icons::for_mount(mount.removable),
        &mount.label,
        selected,
        Message::MountClicked(Location::local(mount.mount_point.clone())),
    )
}

fn row_button<'a>(
    t: &'a Theme,
    glyph: Icon,
    label: &'a str,
    selected: bool,
    on_press: Message,
) -> Element<'a, Message> {
    // Same fixed-tint reasoning `ui::header::nav_button`'s doc comment
    // spells out: an `Svg` icon's color closure is set once at build time,
    // not re-evaluated per `button::Status`, so the selected/unselected
    // split has to be decided by the caller rather than left to hover
    // state.
    let icon_color = if selected {
        t.palette.paper
    } else {
        t.on_paper.primary
    };
    // `.height(Fill)` is load-bearing, not decoration. An iced `button`
    // lays its content out at the padding's top-left corner and never
    // aligns it (`layout::padded` -> `layout::positioned`, which just
    // does `content.move_to((padding.left, padding.top))`) — so inside a
    // button with an explicit `.height(...)`, a `Shrink`-height row is
    // pinned to the *top* of the row and `align_y(Center)` alone does
    // nothing (it only centres the row's children within the row's own
    // 16px-tall box). Making the row `Fill` grows that box to the
    // button's full `list_row` height, and *then* `align_y(Center)` puts
    // the glyph and label on the row's centre line.
    let content = row![
        icon::icon(glyph, t.sizes.icon_row, icon_color.into_iced()),
        text(label)
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .height(Fill)
    .align_y(Center);

    // Stage 12: `style::button::list_row` — the upstreamed promotion of
    // this function's own hand-rolled recipe (§6 "List row": height
    // `sizes.list_row`, `radii.pill`, transparent at rest, `fill_subtle`
    // hover, terracotta selected with ivory text). The sidebar has no
    // keyboard cursor this stage (only mouse selection-by-current-location),
    // so `focused` is always `false` — `list_row`'s fourth parameter, which
    // draws the keyboard-focus ring `ui::dirview::list`'s own rows use.
    button(content)
        .style(style::button::list_row(t, Surface::Paper, selected, false))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding([0.0, t.sizes.pill_gap])
        .on_press(on_press)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::places::PlaceKind;

    fn place(label: &str, path: &str, kind: PlaceKind) -> Place {
        Place {
            label: label.to_owned(),
            location: Location::local(path),
            kind,
        }
    }

    #[test]
    fn new_sidebar_starts_with_no_mounts() {
        let sidebar = Sidebar::new(vec![place("Home", "/home/jordan", PlaceKind::Home)]);
        assert!(sidebar.mounts().is_empty());
        assert_eq!(sidebar.places().len(), 1);
    }

    #[test]
    fn place_clicked_bubbles_an_open_directory_event() {
        let mut sidebar = Sidebar::new(vec![place("Home", "/home/jordan", PlaceKind::Home)]);
        let event = sidebar.update(Message::PlaceClicked(Location::local("/home/jordan")));
        assert_eq!(
            event,
            Some(Event::OpenDirectory(Location::local("/home/jordan")))
        );
    }

    #[test]
    fn mount_clicked_bubbles_an_open_directory_event() {
        let mut sidebar = Sidebar::new(vec![]);
        let event = sidebar.update(Message::MountClicked(Location::local("/media/usb")));
        assert_eq!(
            event,
            Some(Event::OpenDirectory(Location::local("/media/usb")))
        );
    }

    #[test]
    fn mounts_updated_replaces_the_running_list_and_bubbles_nothing() {
        let mut sidebar = Sidebar::new(vec![]);
        let mount = Mount {
            label: "USB Drive".to_owned(),
            mount_point: std::path::PathBuf::from("/media/usb"),
            removable: true,
        };
        let event = sidebar.update(Message::MountsUpdated(vec![mount.clone()]));
        assert_eq!(event, None);
        assert_eq!(sidebar.mounts(), &[mount]);

        // A later, smaller snapshot replaces the list wholesale — this is
        // the "mount removed" half of the live-update contract.
        let event = sidebar.update(Message::MountsUpdated(vec![]));
        assert_eq!(event, None);
        assert!(sidebar.mounts().is_empty());
    }
}
