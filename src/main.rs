//! saola-files — the file manager for the Saola desktop environment.
//!
//! Stage 1 scope: the application shell only — a transparent, undecorated
//! toplevel window drawing its own rounded `paper_window` chrome and 46 px
//! header, with working move/resize/close/maximise against niri. Directory
//! browsing arrives with the VFS layer in Stage 3.

mod ui;

use iced::widget::{column, container, text};
use iced::{Element, Fill, Size, Task, window};
use saola_theme::{Theme, convert};

fn main() -> iced::Result {
    // `default_font` wants an owned `Font` up front, before any `App`
    // exists, so build a throwaway theme just for the font lookup. (The
    // `Box::leak` this implies is saola-theme's documented, once-per-load
    // exception — see saola-theme's convert.rs.)
    let ui_font = convert::ui_font(&Theme::saola());

    iced::application(App::new, App::update, App::view)
        .theme(App::theme)
        // Transparent app background: without this, iced clears the surface
        // to the theme's ink background before drawing, and the window's
        // rounded corners render as square ink wedges (capture learned this
        // live — see ui/window.rs's module docs).
        .style(App::style)
        .title("Files")
        .default_font(ui_font)
        .window(window::Settings {
            // niri draws no decorations and Saola windows draw their own
            // header; there is no taskbar, so minimise must not exist.
            decorations: false,
            transparent: true,
            minimizable: false,
            ..window::Settings::default()
        })
        .window_size(Size::new(1100.0, 720.0))
        .run()
}

#[derive(Debug, Clone, Copy)]
enum Message {
    Window(ui::window::Event),
}

struct App {
    theme: Theme,
}

impl App {
    fn new() -> Self {
        Self {
            theme: Theme::saola(),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Window(event) => ui::window::update(event),
        }
    }

    fn theme(&self) -> iced::Theme {
        saola_theme::to_iced_theme(&self.theme)
    }

    fn style(&self, theme: &iced::Theme) -> iced::theme::Style {
        iced::theme::Style {
            background_color: iced::Color::TRANSPARENT,
            ..iced::theme::default(theme)
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let t = &self.theme;

        // Stage 1 placeholder body: name + version, centered on the paper
        // surface. Replaced by the explorer (sidebar + directory view) in
        // Stage 3.
        let body = container(
            column![
                text("saola-files")
                    .font(convert::display_font(t))
                    .size(t.typography.size.section_heading),
                text(concat!(
                    "v",
                    env!("CARGO_PKG_VERSION"),
                    " — nothing here yet"
                ))
                .size(t.typography.size.secondary)
                .color(convert::ColorExt::into_iced(t.on_paper.secondary)),
            ]
            .spacing(8)
            .align_x(iced::Center),
        )
        .width(Fill)
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center);

        ui::window::view(t, "Files", body.into(), Message::Window)
    }
}
