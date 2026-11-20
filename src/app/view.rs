//! `App::view`: the window's composition — explorer body or Trash
//! browser, the ops/undo footer strip, and the modal dialogs layered
//! over it (properties, connect, conflict, in that stacking order).

use iced::{Element, Fill};

use saola_files::{core, ui};

use super::{App, Message};

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let t = &self.theme;

        // Stage 9: the Trash browser swaps in for the ordinary
        // sidebar+directory-view composition wholesale, but still shows
        // the same sidebar beside it (`ui::trashview` isn't part of
        // `ui::explorer`'s portal seam — see that module's doc comment —
        // so this is composed directly here rather than by threading a
        // "what's on the right" enum through `ui::explorer::view`).
        let body: Element<'_, Message> = if self.trash_active {
            let sidebar_view: Element<'_, Message> = self
                .sidebar
                .view(t, self.surface, &core::places::trash_location())
                .map(|m| Message::Explorer(ui::explorer::Message::Sidebar(m)));
            let trash_column: Element<'_, Message> =
                self.trash_view.view(t, self.surface).map(Message::Trash);
            // Same region geometry `ui::explorer::view` applies to the
            // ordinary composition (see its comment on `sizes.island_gap`):
            // the sidebar is an inset chrome island either way, so the
            // trash browser must not shift it by a pixel when it swaps in.
            iced::widget::row![sidebar_view, trash_column]
                .spacing(t.sizes.island_gap)
                .padding(t.sizes.island_gap)
                .width(Fill)
                .height(Fill)
                .into()
        } else {
            match self.views.get(self.active) {
                Some(view) => ui::explorer::view(
                    t,
                    self.surface,
                    &self.sidebar,
                    view,
                    &self.mime_db,
                    &self.thumb_cache,
                    &self.apps_db,
                    !self.clipboard.is_empty(),
                    Message::Explorer,
                ),
                // Degrades to a blank window ground rather than panicking —
                // `active` should always be in range, but the no-panic
                // rule means "should" isn't good enough.
                None => iced::widget::Space::new().into(),
            }
        };

        // Stage 8: the ops strip, stacked below the explorer body (a
        // persistent bar, not a popover) whenever a copy/move is running.
        // Stage 10: the undo toast takes the exact same footer position
        // whenever *no* op is running but an undo-able one just finished —
        // the two are mutually exclusive (an op finishing is what clears
        // `active_op_progress` and, in the same `handle_op_event` call,
        // sets `undo_toast`), so at most one of these two strips is ever
        // showing at once, never both stacked.
        let with_progress: Element<'_, Message> = match (&self.active_op_progress, &self.undo_toast)
        {
            (Some(progress), _) => iced::widget::column![
                body,
                ui::dialogs::progress::view(t, progress).map(Message::Progress),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            (None, Some(toast)) => iced::widget::column![
                body,
                ui::dialogs::undo_toast::view(t, toast, std::time::Instant::now())
                    .map(Message::UndoToast),
            ]
            .width(Fill)
            .height(Fill)
            .into(),
            (None, None) => body,
        };

        // The properties dialog (Stage 13) is a modal too, but — unlike the
        // conflict dialog — has a sane dismiss default (there's no decision
        // to force), so its scrim closes it instead of swallowing the
        // click into a no-op.
        let with_properties: Element<'_, Message> = match &self.pending_properties {
            Some(pending) => {
                let scrim = iced::widget::mouse_area(
                    iced::widget::container(iced::widget::Space::new())
                        .style(saola_theme::style::container::scrim(
                            t,
                            saola_theme::style::container::ScrimKind::Modal,
                        ))
                        .width(Fill)
                        .height(Fill),
                )
                .on_press(Message::Properties(
                    ui::dialogs::properties::Message::CloseRequested,
                ));
                let dialog = iced::widget::container(
                    ui::dialogs::properties::view(t, &self.mime_db, &pending.properties)
                        .map(Message::Properties),
                )
                .width(Fill)
                .height(Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center);
                iced::widget::stack![with_progress, scrim, dialog].into()
            }
            None => with_progress,
        };

        // The connect dialog (Stage 14) is a modal with the same "sane
        // dismiss default" posture as properties: its scrim closes it
        // (`Self::close_connect_dialog`'s cancel-and-clear, not a Noop
        // swallow) — there's always a reasonable "never mind" for a
        // connect attempt, unlike the conflict dialog's forced decision.
        let with_connect: Element<'_, Message> = match &self.pending_connect {
            Some(pending) => {
                let scrim = iced::widget::mouse_area(
                    iced::widget::container(iced::widget::Space::new())
                        .style(saola_theme::style::container::scrim(
                            t,
                            saola_theme::style::container::ScrimKind::Modal,
                        ))
                        .width(Fill)
                        .height(Fill),
                )
                .on_press(Message::Connect(
                    ui::dialogs::connect::Message::CancelRequested,
                ));
                let dialog = iced::widget::container(
                    ui::dialogs::connect::view(t, &pending.connect).map(Message::Connect),
                )
                .width(Fill)
                .height(Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center);
                iced::widget::stack![with_properties, scrim, dialog].into()
            }
            None => with_properties,
        };

        // The conflict dialog is a true modal: stacked over everything
        // else with a scrim that swallows clicks (`Message::Noop` — see
        // its doc comment) rather than closing on an outside click, since
        // there is no sane default resolution to fall back to.
        let with_conflict: Element<'_, Message> = match &self.pending_conflict {
            Some(pending) => {
                // Stage 12: the invisible click-swallowing scrim now paints
                // the real §2 modal backdrop (`scrim.modal`,
                // `rgba(12,10,0,0.62)`) instead of nothing — the click-swallow
                // behavior (`Message::Noop`, see its own doc comment) is
                // unchanged, only the paint underneath it.
                let scrim = iced::widget::mouse_area(
                    iced::widget::container(iced::widget::Space::new())
                        .style(saola_theme::style::container::scrim(
                            t,
                            saola_theme::style::container::ScrimKind::Modal,
                        ))
                        .width(Fill)
                        .height(Fill),
                )
                .on_press(Message::Noop);
                let dialog = iced::widget::container(
                    ui::dialogs::conflict::view(t, &pending.conflict, pending.apply_to_all)
                        .map(Message::Conflict),
                )
                .width(Fill)
                .height(Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center);
                iced::widget::stack![with_connect, scrim, dialog].into()
            }
            None => with_connect,
        };

        ui::window::view(t, self.surface, "Files", with_conflict, Message::Window)
    }
}
