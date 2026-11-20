//! The connect-to-server dialog (Stage 14): opening it, submitting a
//! `ConnectRequest`, its host-key/auth prompts, and the connect
//! attempt's event stream.

use iced::Task;

use core::remote;
use core::vfs::Location;
use saola_files::{config, core, tr, ui};

use super::{App, Message};

/// See `App::pending_connect`'s doc comment.
pub(super) struct PendingConnect {
    pub(super) connect: ui::dialogs::connect::Connect,
    /// `None` once the attempt reaches a terminal state (`Connected` or
    /// `Failed`) — clearing it is what stops `App::subscription`'s
    /// `connect_sub` binding, the same "`Option` doubles as the
    /// subscription gate" shape `active_op`/`pending_properties.
    /// size_request` already take.
    pub(super) request: Option<remote::ConnectRequest>,
    /// Set while `connect.phase` is `Phase::HostKey` — the reply channel
    /// `Message::Connect(connect::Message::HostKeyDecided(..))`'s handler
    /// sends the human's answer down. CLAUDE.md's capacity-1 reply-channel
    /// pattern (`core::fs::ops`'s conflict prompts).
    host_key_reply: Option<futures::channel::mpsc::Sender<bool>>,
    /// Set while `connect.phase` is `Phase::Auth` — same pattern as
    /// `host_key_reply`, for `Message::Connect(connect::Message::
    /// AuthSubmitted/AuthSkipped)`.
    auth_reply: Option<futures::channel::mpsc::Sender<Option<String>>>,
}

impl App {
    /// `sidebar::Event::ConnectRequested`'s handling and `Self::
    /// navigate_active`'s auto-connect path (Stage 14): opens the connect
    /// dialog. `prefill_uri` seeds the URI field (a clicked sidebar server
    /// row's own location, stringified — `None` for the sidebar's plain
    /// "Connect to Server…" button, which starts blank). `auto_start`
    /// skips straight past `Phase::Entering` into `Self::start_connect` —
    /// the sidebar-row path, where there's nothing left for the human to
    /// type; the "Connect to Server…" button path leaves `Phase::Entering`
    /// showing so the human can actually enter something. Replaces
    /// (rather than stacks behind) any connect dialog already open,
    /// cancelling its in-flight attempt first — same posture `Self::
    /// open_properties` already takes for a second properties request.
    pub(super) fn open_connect_dialog(
        &mut self,
        prefill_uri: Option<String>,
        auto_start: bool,
    ) -> Task<Message> {
        self.close_connect_dialog();

        let uri = prefill_uri.unwrap_or_default();
        // The saved-servers list the dialog offers to pick from is read
        // straight off the sidebar's already-built `Place` list (filtered
        // to `PlaceKind::Server`) rather than a second copy of `Config::
        // servers` kept on `App` — `core::places::build` already turned
        // each `SavedServer` into exactly the `(label, uri)` pair this
        // dialog wants to show, and re-deriving it here keeps there being
        // one source of truth for "what servers does this session know
        // about".
        let servers: Vec<config::SavedServer> = self
            .sidebar
            .places()
            .iter()
            .filter(|place| place.kind == core::places::PlaceKind::Server)
            .map(|place| config::SavedServer {
                name: place.label.clone(),
                uri: place.location.to_string(),
            })
            .collect();

        self.pending_connect = Some(PendingConnect {
            connect: ui::dialogs::connect::Connect::new(uri.clone(), servers),
            request: None,
            host_key_reply: None,
            auth_reply: None,
        });

        if auto_start {
            self.start_connect(uri)
        } else {
            Task::none()
        }
    }

    /// Submits `uri` as a [`remote::ConnectRequest`] and moves the open
    /// connect dialog into `Phase::Connecting` — `ui::dialogs::connect::
    /// Message::ConnectRequested`'s handling, and `Self::
    /// open_connect_dialog`'s auto-start path. A `uri` that doesn't parse
    /// to a remote location (`Location::parse` degrades anything without
    /// a recognized `scheme://` prefix to a bare local path — see that
    /// function's own doc comment) fails immediately with a worded
    /// `Phase::Failed`, before ever touching `core::remote` — there is
    /// nothing for a connection manager to connect *to* for a plain local
    /// path.
    fn start_connect(&mut self, uri: String) -> Task<Message> {
        let location = Location::parse(&uri);
        let Some(pending) = self.pending_connect.as_mut() else {
            return Task::none();
        };
        if location.is_local() {
            pending.connect.phase = ui::dialogs::connect::Phase::Failed(tr!("connect-enter-uri"));
            return Task::none();
        }

        let id = self.connect_ids.alloc();
        pending.connect.phase = ui::dialogs::connect::Phase::Connecting;
        pending.request = Some(remote::ConnectRequest::new(id, location));
        Task::none()
    }

    /// Closes the connect dialog, if one is open — cancels any in-flight
    /// `ConnectRequest` first (same "cancel, then let the engine notice on
    /// its own" posture `Self::close_properties`/`Message::Progress::
    /// CancelRequested` already take), and `try_send`s a negative answer
    /// down whichever prompt reply channel is currently open. A dropped
    /// reply sender would eventually make the awaiting `.await` on the
    /// handshake task's side fail too, but doing it explicitly here means
    /// that task notices immediately rather than only on its next poll.
    fn close_connect_dialog(&mut self) {
        let Some(pending) = self.pending_connect.take() else {
            return;
        };
        if let Some(request) = pending.request {
            request.request_cancel();
        }
        if let Some(mut reply) = pending.host_key_reply {
            let _ = reply.try_send(false);
        }
        if let Some(mut reply) = pending.auth_reply {
            let _ = reply.try_send(None);
        }
    }

    /// `Message::Connect`'s handling — every button/field in `ui::dialogs::
    /// connect` funnels through here.
    pub(super) fn handle_connect_message(
        &mut self,
        message: ui::dialogs::connect::Message,
    ) -> Task<Message> {
        use ui::dialogs::connect::{Message as ConnectMessage, Phase};
        match message {
            ConnectMessage::UriChanged(uri) | ConnectMessage::ServerPicked(uri) => {
                if let Some(pending) = self.pending_connect.as_mut() {
                    pending.connect.uri = uri;
                }
                Task::none()
            }
            ConnectMessage::ConnectRequested => {
                let uri = self
                    .pending_connect
                    .as_ref()
                    .map(|pending| pending.connect.uri.clone())
                    .unwrap_or_default();
                self.start_connect(uri)
            }
            ConnectMessage::CancelRequested => {
                self.close_connect_dialog();
                Task::none()
            }
            ConnectMessage::HostKeyDecided(trust) => {
                if let Some(pending) = self.pending_connect.as_mut()
                    && let Some(mut reply) = pending.host_key_reply.take()
                {
                    let _ = reply.try_send(trust);
                    pending.connect.phase = Phase::Connecting;
                }
                Task::none()
            }
            ConnectMessage::AuthInputChanged(input) => {
                if let Some(pending) = self.pending_connect.as_mut()
                    && let Phase::Auth(_, buffer) = &mut pending.connect.phase
                {
                    *buffer = input;
                }
                Task::none()
            }
            ConnectMessage::AuthSubmitted => {
                if let Some(pending) = self.pending_connect.as_mut() {
                    let answer = match &pending.connect.phase {
                        Phase::Auth(_, buffer) => Some(buffer.clone()),
                        _ => None,
                    };
                    if let Some(mut reply) = pending.auth_reply.take() {
                        let _ = reply.try_send(answer);
                        pending.connect.phase = Phase::Connecting;
                    }
                }
                Task::none()
            }
            ConnectMessage::AuthSkipped => {
                if let Some(pending) = self.pending_connect.as_mut()
                    && let Some(mut reply) = pending.auth_reply.take()
                {
                    let _ = reply.try_send(None);
                    pending.connect.phase = Phase::Connecting;
                }
                Task::none()
            }
        }
    }

    /// `Message::ConnectEvent`'s handling — translates one raw
    /// `remote::ConnectEvent` into the open connect dialog's state,
    /// mirroring `Self::handle_op_event`/`Self::handle_size_event`'s
    /// identical split.
    pub(super) fn handle_connect_event(&mut self, event: remote::ConnectEvent) -> Task<Message> {
        let Some(pending) = self.pending_connect.as_mut() else {
            return Task::none();
        };
        match event {
            remote::ConnectEvent::HostKeyPrompt { prompt, reply } => {
                pending.host_key_reply = Some(reply);
                pending.connect.phase = ui::dialogs::connect::Phase::HostKey(prompt);
                Task::none()
            }
            remote::ConnectEvent::AuthPrompt { stage, reply } => {
                pending.auth_reply = Some(reply);
                pending.connect.phase = ui::dialogs::connect::Phase::Auth(stage, String::new());
                Task::none()
            }
            remote::ConnectEvent::Connected => {
                // Registration into the pool already happened inside
                // `core::remote::connect` *before* this event was sent
                // (see its own doc comment) — `Self::navigate_active`'s
                // `self.remote.pooled(..)` check is guaranteed to find it.
                let location = Location::parse(&pending.connect.uri);
                self.pending_connect = None;
                self.navigate_active(location)
            }
            remote::ConnectEvent::Failed(err) => {
                pending.request = None;
                pending.connect.phase = ui::dialogs::connect::Phase::Failed(err.to_string());
                Task::none()
            }
        }
    }
}
