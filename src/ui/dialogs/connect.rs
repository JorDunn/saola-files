//! The connect dialog (Stage 14): a URI entry plus the saved `[[server]]`
//! list from the config, and — once a connect attempt is under way — the
//! same dialog surface swaps its body for whatever `core::remote::
//! RemoteManager`'s handshake is waiting on (a host-key confirmation, a
//! passphrase/password prompt, or a plain "Connecting…" status), and
//! finally a worded failure if it doesn't work out. Built on the Stage 12
//! dialog kit exactly like `ui::dialogs::conflict`/`properties` — see
//! either's doc comment for the shared conventions (`style::dialog::
//! surface`, the modal scrim, `sizes.dialog_width`) this one repeats
//! rather than re-derives.
//!
//! **Where the state lives.** `main.rs::App`'s `PendingConnect` owns the
//! actual `core::remote::ConnectRequest`/its event subscription and the
//! two prompt reply `mpsc::Sender`s (`host_key_reply`/`auth_reply`) — the
//! same "App owns the plumbing, this module only ever renders a pure
//! snapshot" split `ui::dialogs::properties::Properties`/`PendingProperties`
//! already draws. [`Connect`] is that snapshot; [`Phase`] is which of the
//! handshake's stages it's currently showing.
//!
//! **Reused, not reinvented.** [`Phase::HostKey`] carries `core::remote::
//! HostKeyPrompt` and [`Phase::Auth`] carries `core::remote::AuthStage`
//! directly rather than this module defining its own parallel copies —
//! one type per concept, per CLAUDE.md's simplification instincts.

use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Center, Element, Fill, Length, Subscription};
use saola_theme::icon::{self, Icon};
use saola_theme::{Chrome, ColorExt, Surface, Theme, convert, style, widget};

use crate::config::SavedServer;
use crate::core::remote::{AuthStage, ConnectEvent, ConnectRequest, HostKeyPrompt};

/// The saved-servers list's fixed height — layout-specific to this
/// dialog's own body, not a saola-theme design-system size (same
/// distinction `ui::dialogs::properties::LABEL_COLUMN` draws for its own
/// local layout constant).
const SERVER_LIST_HEIGHT: f32 = 160.0;

#[derive(Debug, Clone)]
pub enum Message {
    /// The URI field's live buffer, while [`Phase::Entering`].
    UriChanged(String),
    /// A saved server row was clicked — fills the URI field with it
    /// rather than submitting immediately, so the human can review/edit
    /// before connecting (e.g. a saved server whose `uri` has no
    /// `user@` and needs one typed in for a different account).
    ServerPicked(String),
    /// The URI field's Enter key, or the dialog's "Connect" button.
    ConnectRequested,
    /// The footer's "Cancel" button, or a click on the modal scrim —
    /// closes the dialog outright. While a connect attempt is in flight
    /// (any phase past [`Phase::Entering`]), `main.rs::App` also cancels
    /// the underlying `ConnectRequest` — see that phase's own reasoning
    /// for why this dialog's scrim behaves like `properties`'s (a sane
    /// dismiss default exists: "never mind, stop trying"), not
    /// `conflict`'s (which swallows the click because there's no sane
    /// default to fall back to).
    CancelRequested,
    /// The host-key confirmation's Trust/Don't Trust buttons.
    HostKeyDecided(bool),
    /// The passphrase/password field's live buffer, while [`Phase::Auth`].
    AuthInputChanged(String),
    /// The passphrase/password field's Enter key, or its "Submit" button.
    AuthSubmitted,
    /// "Skip this key" (only shown for [`AuthStage::KeyPassphrase`]) —
    /// answers the prompt with `None`, which `modules::sftp::try_key_file`
    /// reads as "move on to the next candidate" rather than a hard
    /// failure.
    AuthSkipped,
}

/// Which stage of the handshake the dialog is currently showing.
pub enum Phase {
    /// The URI field + saved-servers picker — the dialog's opening state
    /// whenever it wasn't opened by clicking an already-known sidebar
    /// server row (see `main.rs::App::navigate_active`'s auto-connect
    /// path, which skips straight to [`Phase::Connecting`]).
    Entering,
    /// A connect attempt is running and hasn't hit a prompt yet (still
    /// doing TCP/key-exchange, or an authentication method that doesn't
    /// need a human — ssh-agent, an unencrypted default key).
    Connecting,
    /// `core::remote::ConnectEvent::HostKeyPrompt` landed — first contact
    /// with this host, waiting on Trust/Don't Trust.
    HostKey(HostKeyPrompt),
    /// `core::remote::ConnectEvent::AuthPrompt` landed — waiting on a
    /// passphrase or password. The `String` is the field's live input
    /// buffer (this module's one piece of genuinely-local state, the same
    /// way `ui::dirview::rename`'s edit buffer lives beside the state it
    /// edits rather than back on `App`).
    Auth(AuthStage, String),
    /// The attempt ended in `core::remote::ConnectEvent::Failed` — the
    /// error's own `Display` wording stays on screen with a "Close"
    /// button rather than the dialog vanishing the instant it fails
    /// (CLAUDE.md's capability-honest posture: a failure is worded, not
    /// hidden). Retrying means reopening the dialog from the sidebar —
    /// there's no "back to `Phase::Entering` with the same URI" step this
    /// stage wires up; a future stage could add one without touching
    /// anything outside this `match` arm and `Self::status_body`'s call
    /// site for it.
    Failed(String),
}

/// The dialog's render-time snapshot. See the module doc comment for the
/// App/dialog state split.
pub struct Connect {
    pub uri: String,
    pub servers: Vec<SavedServer>,
    pub phase: Phase,
}

impl Connect {
    pub fn new(uri: String, servers: Vec<SavedServer>) -> Self {
        Connect {
            uri,
            servers,
            phase: Phase::Entering,
        }
    }
}

/// Bridges `core::remote::connect`'s plain `BoxStream` into an
/// `iced::Subscription`, identified by `request` (its manual `Hash`-by-
/// `id` — see that type's doc comment) — the exact same shape `ui::
/// dialogs::progress::subscription`/`properties::subscription` already
/// draw over `ops::run`/`size::run`, just keyed by a `ConnectRequest`.
pub fn subscription(request: &ConnectRequest) -> Subscription<ConnectEvent> {
    Subscription::run_with(request.clone(), crate::core::remote::connect)
}

pub fn view<'a>(t: &'a Theme, connect: &'a Connect) -> Element<'a, Message> {
    let title = text("Connect to Server")
        .size(t.typography.size.dialog_title)
        .font(convert::display_font(t))
        .color(t.on_paper.primary.into_iced());

    let body: Element<'a, Message> = match &connect.phase {
        Phase::Entering => entering_body(t, connect),
        Phase::Connecting => status_body(t, Icon::Globe, "Connecting\u{2026}", None),
        Phase::HostKey(prompt) => host_key_body(t, prompt),
        Phase::Auth(stage, input) => auth_body(t, stage, input),
        Phase::Failed(message) => status_body(t, Icon::X, message, Some(cancel_row(t, "Close"))),
    };

    let content = column![title, body]
        .spacing(t.sizes.popover_padding / 2.0)
        .width(Length::Fixed(t.sizes.dialog_width));

    // Same recipe `ui::dialogs::conflict::view`/`properties::view` use —
    // see either's own comment on why `style::dialog::surface` takes no
    // `Surface` parameter.
    container(content)
        .style(style::dialog::surface(t))
        .padding(t.sizes.popover_padding)
        .into()
}

fn entering_body<'a>(t: &'a Theme, connect: &'a Connect) -> Element<'a, Message> {
    let field = text_input("sftp://user@host/path", &connect.uri)
        .on_input(Message::UriChanged)
        .on_submit(Message::ConnectRequested)
        .style(style::text_input::rest(t, Surface::Paper))
        .font(convert::mono_font(t))
        .size(t.typography.size.secondary)
        .width(Fill);

    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    if connect.servers.is_empty() {
        rows.push(
            text("No saved servers yet — add a [[server]] entry to files.toml.")
                .size(t.typography.size.secondary)
                .font(convert::ui_font_regular(t))
                .color(t.on_paper.secondary.into_iced())
                .into(),
        );
    } else {
        rows.push(widget::section_label(t, Surface::Paper, "SAVED SERVERS"));
        rows.extend(connect.servers.iter().map(|server| server_row(t, server)));
    }

    let saved = container(
        scrollable(column(rows).width(Fill))
            .style(style::scrollable::rest(t, Surface::Paper))
            .width(Fill)
            .height(Length::Fixed(SERVER_LIST_HEIGHT)),
    )
    .style(style::container::inset(t, Surface::Paper))
    .padding(t.sizes.gap_tight);

    let connect_button = button(
        row![
            icon::icon(Icon::Globe, t.sizes.icon_row, t.palette.paper.into_iced()),
            text("Connect")
                .size(t.typography.size.body)
                .font(convert::ui_font(t)),
        ]
        .spacing(t.sizes.pill_gap)
        .align_y(Center),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        true,
    ))
    .padding(t.paddings.dialog_button)
    .on_press(Message::ConnectRequested);

    let footer = widget::footer_strip(
        t,
        Surface::Paper,
        row![cancel_button(t), Space::new().width(Fill), connect_button]
            .width(Fill)
            .align_y(Center),
    );

    column![field, saved, footer]
        .spacing(t.sizes.gap_tight)
        .into()
}

fn server_row<'a>(t: &'a Theme, server: &'a SavedServer) -> Element<'a, Message> {
    let content = row![
        icon::icon(
            Icon::Server,
            t.sizes.icon_row,
            t.on_paper.primary.into_iced()
        ),
        column![
            text(&server.name)
                .size(t.typography.size.body)
                .font(convert::ui_font(t)),
            text(&server.uri)
                .size(t.typography.size.secondary)
                .font(convert::mono_font(t))
                .color(t.on_paper.secondary.into_iced()),
        ]
        .spacing(0.0),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center)
    .height(Fill);

    button(content)
        .style(style::button::list_row(t, Surface::Paper, false, false))
        .width(Fill)
        .height(t.sizes.list_row)
        .padding([0.0, t.sizes.pill_gap])
        .on_press(Message::ServerPicked(server.uri.clone()))
        .into()
}

/// A one-line icon + message body, optionally followed by a footer row —
/// shared by the "Connecting…" and "Failed" phases, which are both just
/// "one glyph, one line of text" with a different (or absent) footer.
fn status_body<'a>(
    t: &'a Theme,
    glyph: Icon,
    message: &'a str,
    footer: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let line = row![
        icon::icon(glyph, t.sizes.icon_row, t.on_paper.primary.into_iced()),
        text(message)
            .size(t.typography.size.body)
            .font(convert::ui_font_regular(t))
            .color(t.on_paper.primary.into_iced()),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);

    match footer {
        Some(footer) => column![line, footer].spacing(t.sizes.gap_tight).into(),
        None => line.into(),
    }
}

fn host_key_body<'a>(t: &'a Theme, prompt: &'a HostKeyPrompt) -> Element<'a, Message> {
    let title = row![
        icon::icon(Icon::Lock, t.sizes.icon_row, t.on_paper.primary.into_iced()),
        text(format!(
            "{}:{} presented a new host key",
            prompt.host, prompt.port
        ))
        .size(t.typography.size.body)
        .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);

    let detail = text(format!(
        "{} {}\nThis server has never been connected to before. Only trust it if you recognize this fingerprint.",
        prompt.key_type, prompt.fingerprint
    ))
    .size(t.typography.size.secondary)
    .font(convert::mono_font(t))
    .color(t.on_paper.secondary.into_iced());

    let trust = button(
        text("Trust & Connect")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        true,
    ))
    .padding(t.paddings.dialog_button)
    .on_press(Message::HostKeyDecided(true));

    let footer = widget::footer_strip(
        t,
        Surface::Paper,
        row![
            cancel_labeled_button(t, "Don't Trust", Message::HostKeyDecided(false)),
            Space::new().width(Fill),
            trust
        ]
        .width(Fill)
        .align_y(Center),
    );

    column![title, detail, footer]
        .spacing(t.sizes.gap_tight)
        .into()
}

fn auth_body<'a>(t: &'a Theme, stage: &'a AuthStage, input: &'a str) -> Element<'a, Message> {
    let (label, placeholder) = match stage {
        AuthStage::KeyPassphrase { key_path } => (
            format!("Passphrase for {}", key_path.display()),
            "Passphrase",
        ),
        AuthStage::Password { user } => (format!("Password for {user}"), "Password"),
    };

    let title = row![
        icon::icon(Icon::Lock, t.sizes.icon_row, t.on_paper.primary.into_iced()),
        text(label)
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    ]
    .spacing(t.sizes.pill_gap)
    .align_y(Center);

    let field = text_input(placeholder, input)
        .on_input(Message::AuthInputChanged)
        .on_submit(Message::AuthSubmitted)
        .secure(true)
        .style(style::text_input::rest(t, Surface::Paper))
        .font(convert::ui_font_regular(t))
        .size(t.typography.size.secondary)
        .width(Fill);

    let submit = button(
        text("Submit")
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::emphasis(
        t,
        Surface::Paper,
        Chrome::Window,
        true,
    ))
    .padding(t.paddings.dialog_button)
    .on_press(Message::AuthSubmitted);

    // "Skip this key" only makes sense for a key passphrase (there's a
    // next candidate — another default key file, or password auth — to
    // fall through to); a skipped password prompt has nothing left to try
    // and is exactly what "Cancel" already means.
    let secondary: Element<'a, Message> = match stage {
        AuthStage::KeyPassphrase { .. } => {
            cancel_labeled_button(t, "Skip This Key", Message::AuthSkipped)
        }
        AuthStage::Password { .. } => cancel_button(t),
    };

    let footer = widget::footer_strip(
        t,
        Surface::Paper,
        row![secondary, Space::new().width(Fill), submit]
            .width(Fill)
            .align_y(Center),
    );

    column![title, field, footer]
        .spacing(t.sizes.gap_tight)
        .into()
}

fn cancel_button<'a>(t: &'a Theme) -> Element<'a, Message> {
    cancel_labeled_button(t, "Cancel", Message::CancelRequested)
}

fn cancel_row<'a>(t: &'a Theme, label: &'a str) -> Element<'a, Message> {
    row![
        Space::new().width(Fill),
        cancel_labeled_button(t, label, Message::CancelRequested)
    ]
    .width(Fill)
    .into()
}

fn cancel_labeled_button<'a>(
    t: &'a Theme,
    label: &'a str,
    on_press: Message,
) -> Element<'a, Message> {
    button(
        text(label)
            .size(t.typography.size.body)
            .font(convert::ui_font(t)),
    )
    .style(style::button::rest(t, Surface::Paper, Chrome::Window))
    .padding(t.paddings.dialog_button)
    .on_press(on_press)
    .into()
}
