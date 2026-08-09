//! saola-files — the file manager for the Saola desktop environment.
//!
//! Current scope (through Stage 3): the application shell (Stage 1), CLI
//! parsing and `files.toml` loading (Stage 2), and now real directory
//! browsing — a VFS `Backend` trait, a local backend, and the explorer
//! (sidebar/breadcrumbs land in Stages 7/4).
//!
//! This binary is a thin shell over the `saola_files` library crate
//! (`src/lib.rs`) — see that file's docs for why the split exists.

use std::path::PathBuf;

use iced::{Element, Size, Subscription, Task, window};
use saola_theme::{Theme, convert};

use core::vfs::Location;
use saola_files::{cli, config, core, ui};

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
    .subscription(App::subscription)
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

#[derive(Debug, Clone)]
enum Message {
    Window(ui::window::Event),
    Directory(ui::dirview::Message),
}

struct App {
    theme: Theme,
    /// The tabs seam (CLAUDE.md: "the app holds `Vec<DirectoryView> +
    /// active`") — Stage 3 only ever shows one.
    views: Vec<ui::dirview::DirectoryView>,
    active: usize,
}

impl App {
    fn new(config: config::Config, args: cli::Cli) -> (Self, Task<Message>) {
        // `$HOME` unset (e.g. a minimal CI sandbox) falls back to `/`
        // rather than failing to start — the config loader takes the same
        // posture for its own directory chain.
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let fallback = Location::local(home);

        // `config` is consumed entirely here (view/sort/show-hidden
        // defaults baked into the first `DirectoryView`); nothing later
        // reads it back, so `App` doesn't carry it as a field. A future
        // "new tab" action that needs the same defaults would resolve
        // `Config::load` again or thread a clone through at that point.
        let (view, task) = if let Some(select) = args.select {
            ui::dirview::DirectoryView::open_select(select, fallback, &config)
        } else if let Some(target) = args.target {
            ui::dirview::DirectoryView::open_target(target, fallback, &config)
        } else {
            ui::dirview::DirectoryView::open(fallback, &config)
        };

        let app = App {
            theme: Theme::saola(),
            views: vec![view],
            active: 0,
        };
        (app, task.map(Message::Directory))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Window(event) => ui::window::update(event),
            Message::Directory(inner) => {
                let Some(view) = self.views.get_mut(self.active) else {
                    return Task::none();
                };
                let (task, event) = view.update(inner);
                let mut tasks = vec![task.map(Message::Directory)];
                if let Some(event) = event {
                    tasks.push(self.handle_directory_event(event));
                }
                Task::batch(tasks)
            }
        }
    }

    /// The owner's response to a `DirectoryView` `Event` — the view only
    /// ever requests a navigation, never applies one itself (see
    /// `ui::dirview`'s module docs).
    fn handle_directory_event(&mut self, event: ui::dirview::Event) -> Task<Message> {
        match event {
            ui::dirview::Event::OpenDirectory(location) => {
                let Some(view) = self.views.get_mut(self.active) else {
                    return Task::none();
                };
                view.navigate(location).map(Message::Directory)
            }
            // Opening non-directory entries (xdg-open-equivalent app
            // resolution) lands in Stage 6; nothing to do yet, and never
            // a panic on an event this stage doesn't act on.
            ui::dirview::Event::Activated(_locations) => Task::none(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let keyboard = iced::keyboard::listen()
            .map(|event| Message::Directory(ui::dirview::Message::Keyboard(event)));

        // Stage 5: the active view's own live-update watch, if its backend
        // has one — `None`/an out-of-range `active` degrades to "no watch
        // subscription" rather than a panic, same posture as `App::view`'s
        // own `views.get(self.active)` guard.
        let watch = self
            .views
            .get(self.active)
            .map(|view| view.subscription().map(Message::Directory))
            .unwrap_or_else(Subscription::none);

        Subscription::batch([keyboard, watch])
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

        let body = match self.views.get(self.active) {
            Some(view) => ui::explorer::view(t, view, Message::Directory),
            // Degrades to a blank paper surface rather than panicking —
            // `active` should always be in range, but the no-panic rule
            // means "should" isn't good enough.
            None => iced::widget::Space::new().into(),
        };

        ui::window::view(t, "Files", body, Message::Window)
    }
}
