//! `App::subscription`: every live stream the app listens to (keyboard,
//! directory watch, udisks mounts, op progress, undo-toast tick, size
//! count, connect handshake, D-Bus), gated on the state that needs it.
//!
//! A feature with its own stream adds a `fn <feature>_subscription(&self)
//! -> Subscription<Message>` in its own `src/app/<feature>.rs` and one
//! line calling it in the `batch` at the bottom, under the marker.

use iced::Subscription;

use saola_files::{integration, ui};

use super::{App, KeyboardTarget, Message, directory_message};

impl App {
    pub(crate) fn subscription(&self) -> Subscription<Message> {
        // Stage 9: while the Trash browser is showing, the hidden
        // background `DirectoryView` must not receive keyboard input at
        // all — `dirview::Message::Keyboard` mutating its (invisible) cursor/
        // selection would be merely odd on its own, but reaching Delete/
        // Shift+Delete through it would silently trash/permanently-delete
        // files in whatever directory the user last browsed, with no
        // visual feedback in the Trash view showing it happened. Cutting
        // the subscription off here — rather than trying to filter
        // individual `Action`s downstream — is the same "one surface owns
        // the keyboard at a time" posture `DirectoryView::handle_keyboard`
        // already takes for its own path-editor/rename/menu guards, just
        // one level up.
        // Stage 13: the properties dialog is App-level chrome, not a
        // `DirectoryView` overlay (no `menu_open`-style flag on the view
        // itself to guard through) — gated here the same way `trash_active`
        // already is, for the same reason: arrow keys/Delete/etc must not
        // reach the (invisible, behind the dialog) directory view while
        // it's open.
        // Stage 14: the connect dialog is the same kind of App-level modal
        // as properties — gated here for the identical reason.
        // The gate is now one named predicate, `App::keyboard_target` (it
        // also covers the copy-conflict dialog, which this used to miss).
        let keyboard = match self.keyboard_target() {
            // `App::update` routes `Message::Keyboard` to the focused view.
            KeyboardTarget::FocusedView => iced::keyboard::listen().map(Message::Keyboard),
            KeyboardTarget::Nobody => Subscription::none(),
        };

        // Stage 5: every open view's own live-update watch (a backend that
        // can't watch just never produces an event). All views, not only
        // the focused one: a hidden tab must still see files appear.
        //
        // Why `.with(*id)` instead of a closure: `Subscription::map` only
        // accepts a function that captures nothing (iced checks this at
        // compile time), because iced identifies a running subscription
        // partly by its mapper's type, and a captured value would be
        // invisible to that identity. So `.map(|m| directory_message((id,
        // m)))` is rejected. `.with(id)` instead folds the id into the
        // subscription's identity (two views on the same folder get two
        // separate watches) and turns each `m` into the pair `(id, m)`,
        // which the plain function `directory_message` can then wrap.
        let watch = Subscription::batch(
            self.views
                .iter()
                .map(|(id, view)| view.subscription().with(*id).map(directory_message)),
        );

        // Stage 7: the places sidebar's own live udisks feed — one for the
        // app's whole lifetime (`ui::sidebar::Sidebar::subscription`'s doc
        // comment), batched in beside the keyboard/watch streams above.
        let mounts = self
            .sidebar
            .subscription()
            .map(|m| Message::Explorer(ui::explorer::Message::Sidebar(m)));

        // Stage 8: the active op's progress stream, if one is running —
        // identified by `OpRequest` (its manual `Hash`-by-`id`, see that
        // type's doc comment), so iced keeps the same running copy/move
        // alive across re-renders and tears it down the moment `active_op`
        // goes back to `None`.
        let ops = self
            .active_op
            .as_ref()
            .map(|request| ui::dialogs::progress::subscription(request).map(Message::OpEvent))
            .unwrap_or_else(Subscription::none);

        // Stage 10: the undo toast's fade tick, gated to only run while a
        // toast is actually showing — CLAUDE.md's "nothing ticks without a
        // documented exception" (see `ui::dialogs::undo_toast`'s own
        // module doc comment for this exception's reasoning).
        let undo_toast = ui::dialogs::undo_toast::subscription(self.undo_toast.is_some())
            .map(Message::UndoToast);

        // Stage 13: the properties dialog's live size count, if one is
        // still running — same "identified by the request's own `Hash`,
        // torn down the moment the field goes back to `None`" posture as
        // `ops` above, just for `size::SizeRequest` instead of `OpRequest`.
        let size_sub = self
            .pending_properties
            .as_ref()
            .and_then(|pending| pending.size_request.as_ref())
            .map(|request| ui::dialogs::properties::subscription(request).map(Message::SizeEvent))
            .unwrap_or_else(Subscription::none);

        // Stage 14: the connect dialog's in-flight handshake, if one is
        // running — same "identified by the request's own `Hash`, torn
        // down the moment the field goes back to `None`" posture as `ops`/
        // `size_sub` above, just for `remote::ConnectRequest`.
        let connect_sub = self
            .pending_connect
            .as_ref()
            .and_then(|pending| pending.request.as_ref())
            .map(|request| ui::dialogs::connect::subscription(request).map(Message::ConnectEvent))
            .unwrap_or_else(Subscription::none);

        // Stage 15: the D-Bus activation surface (`org.freedesktop.
        // FileManager1`/`io.saola.Files1`) — one for the app's whole
        // lifetime, the same "keyed by a stable identity, torn down never"
        // posture `mounts` above already takes, just keyed on `Self::
        // activation` (which never changes after `Self::new`) rather than
        // being unconditional. See `integration::dbus::subscription`'s own
        // doc comment for the full acquire-or-forward handshake this runs.
        let dbus = Subscription::run_with(self.activation.clone(), integration::dbus::subscription)
            .map(Message::Dbus);

        Subscription::batch([
            keyboard,
            watch,
            mounts,
            ops,
            undo_toast,
            size_sub,
            connect_sub,
            dbus,
            // --- features (alphabetical) ---
            self.session_subscription(),
        ])
    }
}
