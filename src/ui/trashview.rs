//! The Trash browser (Stage 9): a synthetic, read-mostly directory-view-
//! shaped surface reachable from the sidebar's Trash place — list what's
//! in `$XDG_DATA_HOME/Trash`, restore an item, or empty everything. Not a
//! literal `DirectoryView` (it doesn't navigate, has no `Backend`, no
//! selection/keyboard-cursor model) — the stage's "synthetic
//! `DirectoryView`" wording means "the same *idea*: self-contained state
//! plus a `Message`/`Task`-driven `update`, composed beside the sidebar
//! the way `ui::explorer` composes a real `DirectoryView`", not the same
//! struct. `main.rs::App` swaps this in for the ordinary explorer body
//! whenever the active location is the sidebar's trash sentinel
//! (`core::places::trash_location()`) — see that function's doc comment
//! for the swap mechanics.
//!
//! Deliberately minimal for a first cut, documented as such rather than
//! silently thin:
//! - **No selection/keyboard model.** Each row's own "Restore" button acts
//!   on that row alone; there is no multi-select restore or keyboard
//!   navigation yet (`main.rs` also stops feeding the keyboard
//!   subscription into the hidden background `DirectoryView` while this is
//!   showing — see its own doc comment — so there's nothing *accidentally*
//!   reachable by keyboard here either). Trash browsing is a "get one
//!   thing back" surface, not a primary workflow, so this is a reasonable
//!   v1 scope cut, not an oversight.
//! - **No per-item permanent-delete-from-trash.** Only "Empty Trash"
//!   (everything at once, gated on `files.toml`'s `confirm-empty-trash`)
//!   and "Restore" (one item) exist.
//! - Every list/restore/empty call runs through `core::fs::trash` inside
//!   `tokio::task::spawn_blocking` (the same posture `modules::local`'s
//!   own `run_blocking` takes for its `std::fs` calls) — this view never
//!   touches `std::fs` itself, keeping the "trash is local-only,
//!   core/fs/trash.rs is the one exception" boundary in one place.

use std::path::PathBuf;

use iced::widget::{Space, button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Task};
use saola_theme::icon::{self, Icon};
use saola_theme::{ColorExt, Surface, Theme, convert, style};

use crate::core::fs::entry::EntryKind;
use crate::core::fs::trash::{self, TrashedItem};
use crate::core::vfs::VfsError;

#[derive(Debug, Clone)]
pub enum Message {
    /// A `core::fs::trash::list` response — from the initial `load()` the
    /// owner kicks off when switching into Trash, and from every reload
    /// this view triggers itself after a restore/empty.
    Loaded(Result<Vec<TrashedItem>, VfsError>),
    /// A row's "Restore" button, by its index into `items`.
    RestoreRequested(usize),
    /// The `core::fs::trash::restore` call `RestoreRequested` kicks off.
    /// Either outcome reloads the list (see `update`'s doc comment on that
    /// arm) — there's no per-item error slot this stage, a failed restore
    /// is worded to stderr and the row simply stays put.
    RestoreResult(usize, Result<PathBuf, VfsError>),
    /// The toolbar's "Empty Trash" button.
    EmptyRequested,
    /// The inline confirm strip's "Empty Trash" button (only reachable
    /// when `files.toml`'s `confirm-empty-trash` is true).
    EmptyConfirmClicked,
    /// The inline confirm strip's "Cancel" button.
    EmptyCancelClicked,
    EmptyResult(Result<(), VfsError>),
}

pub struct TrashView {
    items: Vec<TrashedItem>,
    loading: bool,
    error: Option<String>,
    /// `files.toml`'s `confirm-empty-trash` knob, baked in at construction
    /// — the same "config knobs become fixed per-view state, not re-read
    /// live" posture `DirectoryView::new` takes for `sort`/`view_mode`/etc.
    confirm_empty_trash: bool,
    /// `true` while the inline "really empty the trash?" strip is showing
    /// in place of the ordinary toolbar — only reachable when
    /// `confirm_empty_trash` is set; otherwise `EmptyRequested` empties
    /// immediately and this never becomes `true`.
    confirming_empty: bool,
}

impl TrashView {
    pub fn new(confirm_empty_trash: bool) -> Self {
        TrashView {
            items: Vec::new(),
            loading: false,
            error: None,
            confirm_empty_trash,
            confirming_empty: false,
        }
    }

    /// Fetches the trash's current contents — the owner calls this once
    /// when switching into Trash view; `update` calls it again itself
    /// after every restore/empty so the list is always what's actually on
    /// disk, never optimistically edited in place.
    pub fn load(&mut self) -> Task<Message> {
        self.loading = true;
        self.error = None;
        Task::perform(run_blocking(trash::list), Message::Loaded)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(items) => {
                        self.items = items;
                        self.error = None;
                    }
                    Err(err) => {
                        self.items.clear();
                        self.error = Some(err.to_string());
                    }
                }
                Task::none()
            }
            Message::RestoreRequested(index) => {
                let Some(item) = self.items.get(index).cloned() else {
                    return Task::none();
                };
                Task::perform(
                    run_blocking(move || trash::restore(&item.id)),
                    move |result| Message::RestoreResult(index, result),
                )
            }
            Message::RestoreResult(_index, result) => {
                if let Err(err) = &result {
                    eprintln!("saola-files: couldn't restore from trash: {err}");
                }
                // Reload either way: success removes the restored row,
                // failure just re-confirms it's still there — there's no
                // per-item error slot this stage (see the module doc
                // comment's stated gaps).
                self.load()
            }
            Message::EmptyRequested => {
                if self.confirm_empty_trash {
                    self.confirming_empty = true;
                    Task::none()
                } else {
                    self.start_empty()
                }
            }
            Message::EmptyConfirmClicked => {
                self.confirming_empty = false;
                self.start_empty()
            }
            Message::EmptyCancelClicked => {
                self.confirming_empty = false;
                Task::none()
            }
            Message::EmptyResult(result) => {
                if let Err(err) = &result {
                    eprintln!("saola-files: couldn't empty the trash: {err}");
                }
                self.load()
            }
        }
    }

    fn start_empty(&mut self) -> Task<Message> {
        Task::perform(run_blocking(trash::empty), Message::EmptyResult)
    }

    pub fn view<'a>(&'a self, t: &'a Theme) -> Element<'a, Message> {
        let toolbar_row = toolbar(t, self.items.is_empty(), self.confirming_empty);

        let body: Element<'a, Message> = if let Some(err) = &self.error {
            saola_theme::widget::empty_state(t, Surface::Paper, err)
        } else if self.items.is_empty() {
            let message = if self.loading {
                "Loading…"
            } else {
                "Trash is empty"
            };
            saola_theme::widget::empty_state(t, Surface::Paper, message)
        } else {
            let rows: Vec<Element<'a, Message>> = self
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| item_row(t, index, item))
                .collect();
            scrollable(column(rows).width(Fill))
                .style(style::scrollable::rest(t, Surface::Paper))
                .width(Fill)
                .height(Fill)
                .into()
        };

        // `sizes.island_gap` between the toolbar island and the listing
        // below it — the identical seam `ui::explorer::view` puts between
        // `ui::header`'s toolbar and the directory view, so switching to
        // the trash browser doesn't change the window's rhythm.
        column![toolbar_row, body]
            .spacing(t.sizes.island_gap)
            .width(Fill)
            .height(Fill)
            .into()
    }
}

/// Runs `f` on the blocking pool, turning a `JoinError` (the task
/// panicked) into a worded `VfsError` rather than propagating a panic —
/// the no-panic rule extends to background work, the same posture
/// `modules::local::run_blocking` already takes for its own `std::fs`
/// calls (not reused directly: that one is private to `modules::local` and
/// keyed to a `Location` for its error wording, this one to a bare
/// message, since `core::fs::trash`'s own functions already word their
/// errors against a `Path`).
async fn run_blocking<T, F>(f: F) -> Result<T, VfsError>
where
    F: FnOnce() -> Result<T, VfsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(VfsError::Other {
            message: format!("internal error talking to the trash: {join_err}"),
        }),
    }
}

fn toolbar<'a>(t: &'a Theme, items_empty: bool, confirming: bool) -> Element<'a, Message> {
    if confirming {
        return confirm_strip(t);
    }

    let title = text("Trash")
        .size(t.typography.size.dialog_title)
        .font(convert::display_font(t))
        .color(t.on_paper.primary.into_iced());

    let empty_content = row![
        icon::icon(
            Icon::Trash2,
            t.sizes.icon_row,
            t.on_paper.primary.into_iced()
        ),
        text("Empty Trash")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);
    let mut empty_button = button(empty_content)
        .style(style::button::rest(t, Surface::Paper))
        .padding(t.paddings.pill_button);
    if !items_empty {
        empty_button = empty_button.on_press(Message::EmptyRequested);
    }

    // The trash browser's toolbar is the same chrome region `ui::header`'s
    // is, so it takes the same recessed `style::container::inset` ground
    // (Stage 12) rather than sitting on the listing's paper.
    container(
        row![title, Space::new().width(Fill), empty_button]
            .align_y(Center)
            .width(Fill),
    )
    .style(style::container::inset(t, Surface::Paper))
    .width(Fill)
    .height(t.sizes.window_header)
    .padding([0.0, t.sizes.pill_gap])
    .align_y(Center)
    .into()
}

/// The inline "really empty the trash?" strip — replaces the ordinary
/// toolbar in place (not a modal: CLAUDE.md's severity-by-wording posture
/// doesn't call for a scrim/dialog for a single static yes/no, unlike the
/// per-item conflict dialog `core::fs::ops` drives, which carries real
/// per-conflict data). `App` never sees this state at all — it's entirely
/// local to `TrashView`, the same "self-contained per-surface state"
/// posture `DirectoryView`'s own inline-rename state takes.
fn confirm_strip<'a>(t: &'a Theme) -> Element<'a, Message> {
    let label = text("Permanently delete everything in the trash?")
        .size(t.typography.size.body)
        .font(convert::ui_font(t))
        .color(t.on_paper.primary.into_iced());

    let cancel = button(
        text("Cancel")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper))
    .padding(t.paddings.pill_button)
    .on_press(Message::EmptyCancelClicked);

    let confirm = button(
        text("Empty Trash")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper))
    .padding(t.paddings.pill_button)
    .on_press(Message::EmptyConfirmClicked);

    // Takes the toolbar's own `style::container::inset` ground (Stage 12):
    // it *replaces* the toolbar in place, so it has to occupy the same
    // region, at the same height, on the same surface — otherwise
    // confirming would look like the chrome band vanished.
    container(
        row![label, Space::new().width(Fill), cancel, confirm]
            .spacing(t.sizes.pill_gap)
            .align_y(Center),
    )
    .style(style::container::inset(t, Surface::Paper))
    .width(Fill)
    .height(t.sizes.window_header)
    .padding([0.0, t.sizes.pill_gap])
    .align_y(Center)
    .into()
}

fn item_row<'a>(t: &'a Theme, index: usize, item: &'a TrashedItem) -> Element<'a, Message> {
    let glyph = if item.is_symlink {
        Icon::Link
    } else if item.kind == EntryKind::Directory {
        Icon::Folder
    } else {
        Icon::File
    };
    let icon = icon::icon(glyph, t.sizes.icon_row, t.on_paper.primary.into_iced());

    let name = item
        .original_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| item.original_path.display().to_string());
    let name_text = text(name)
        .size(t.typography.size.body)
        .font(convert::ui_font(t));
    let original = text(item.original_path.display().to_string())
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t))
        .color(t.on_paper.secondary.into_iced());
    let labels = column![name_text, original].spacing(2.0).width(Fill);

    let date = text(item.deletion_date.clone())
        .size(t.typography.size.secondary)
        .font(convert::mono_font(t))
        .color(t.on_paper.secondary.into_iced());

    let restore_content = row![
        icon::icon(
            Icon::RotateCcw,
            t.sizes.icon_row,
            t.on_paper.primary.into_iced()
        ),
        text("Restore")
            .size(t.typography.size.label)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.gap_tight)
    .align_y(Center);
    let restore = button(restore_content)
        .style(style::button::bare(t, Surface::Paper))
        .padding(t.paddings.strip)
        .on_press(Message::RestoreRequested(index));

    let content = row![icon, labels, date, restore]
        .spacing(t.sizes.pill_gap)
        .align_y(Center)
        .padding([t.sizes.pill_gap / 2.0, t.sizes.pill_gap]);

    container(content).width(Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_view_starts_empty_and_not_confirming() {
        let view = TrashView::new(true);
        assert!(view.items.is_empty());
        assert!(!view.confirming_empty);
        assert!(!view.loading);
    }

    #[test]
    fn empty_requested_without_confirmation_configured_never_shows_the_strip() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::EmptyRequested);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn empty_requested_with_confirmation_configured_shows_the_strip() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        assert!(view.confirming_empty);
    }

    #[test]
    fn cancelling_the_confirm_strip_clears_it() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        let _ = view.update(Message::EmptyCancelClicked);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn confirming_the_strip_clears_it_too() {
        let mut view = TrashView::new(true);
        let _ = view.update(Message::EmptyRequested);
        let _ = view.update(Message::EmptyConfirmClicked);
        assert!(!view.confirming_empty);
    }

    #[test]
    fn restore_requested_out_of_range_is_a_safe_no_op() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::RestoreRequested(0));
        assert!(view.items.is_empty());
    }

    #[test]
    fn loaded_error_words_the_failure_and_clears_any_previous_items() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::Loaded(Err(VfsError::PermissionDenied {
            location: "/x".to_owned(),
        })));
        assert!(view.items.is_empty());
        assert!(view.error.is_some());
    }

    #[test]
    fn a_later_successful_load_clears_a_previous_error() {
        let mut view = TrashView::new(false);
        let _ = view.update(Message::Loaded(Err(VfsError::Other {
            message: "x".to_owned(),
        })));
        assert!(view.error.is_some());
        let _ = view.update(Message::Loaded(Ok(Vec::new())));
        assert!(view.error.is_none());
    }
}
