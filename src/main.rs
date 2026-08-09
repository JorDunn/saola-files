//! saola-files — the file manager for the Saola desktop environment.
//!
//! Current scope (through Stage 2): the application shell — a transparent,
//! undecorated toplevel window drawing its own rounded `paper_window`
//! chrome and 46 px header — plus CLI parsing and `files.toml` loading.
//! Directory browsing arrives with the VFS layer in Stage 3.

mod cli;
mod config;
mod ui;

use iced::widget::{column, container, text};
use iced::{Element, Fill, Size, Task, window};
use saola_theme::{Theme, convert};

fn main() -> iced::Result {
    let invocation = match cli::parse(std::env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("saola-files: {message}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let args = match invocation {
        cli::Invocation::Version => {
            println!("saola-files {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        cli::Invocation::Help => {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        cli::Invocation::Run(args) => args,
    };

    let config_path = config::Config::resolve_path(args.config_dir.as_deref());
    let config = config::Config::load(config_path.as_deref());

    // `default_font` wants an owned `Font` up front, before any `App`
    // exists, so build a throwaway theme just for the font lookup. (The
    // `Box::leak` this implies is saola-theme's documented, once-per-load
    // exception — see saola-theme's convert.rs.)
    let ui_font = convert::ui_font(&Theme::saola());

    iced::application(
        move || App::new(config.clone(), args.clone()),
        App::update,
        App::view,
    )
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
    /// Loaded `files.toml`. Read from Stage 3 on (view/sort defaults for
    /// new directory views); `#[expect]` makes the compiler flag the
    /// attribute for removal the moment that lands.
    #[expect(dead_code)]
    config: config::Config,
    /// The CLI's target/select, consumed when the VFS opens the initial
    /// location in Stage 3.
    #[expect(dead_code)]
    args: cli::Cli,
}

impl App {
    fn new(config: config::Config, args: cli::Cli) -> Self {
        Self {
            theme: Theme::saola(),
            config,
            args,
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
