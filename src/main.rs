//! saola-files — the file manager for the Saola desktop environment.
//!
//! Current scope (through Stage 7): the application shell (Stage 1), CLI
//! parsing and `files.toml` loading (Stage 2), real directory browsing
//! through a VFS `Backend` trait and a local backend, navigation chrome
//! and live updates (Stages 4–5), mime/icons/opening (Stage 6), and now
//! the places sidebar (Stage 7).
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
    Explorer(ui::explorer::Message),
}

struct App {
    theme: Theme,
    /// The tabs seam (CLAUDE.md: "the app holds `Vec<DirectoryView> +
    /// active`") — Stage 3 only ever shows one.
    views: Vec<ui::dirview::DirectoryView>,
    active: usize,
    /// The places sidebar (Stage 7) — its own self-contained state
    /// (`core::places::Place`s built once at startup, `core::udisks::
    /// Mount`s updated live), composed beside `views`/`active` by
    /// `ui::explorer::view` rather than folded into the tabs seam: unlike
    /// a `DirectoryView`, there is exactly one sidebar for the whole app,
    /// not one per tab.
    sidebar: ui::sidebar::Sidebar,
    /// Shared caches (CLAUDE.md: "Shared caches (thumbs, mime, apps, …)
    /// live on the App, never per-view") — built once at startup, since
    /// both walk real filesystem trees (`$XDG_DATA_DIRS/mime`,
    /// `$XDG_DATA_DIRS/applications`) that don't change while the app is
    /// running.
    mime_db: core::mime::MimeDb,
    apps_db: core::apps::AppsDb,
    /// `files.toml`'s `terminal` knob, the one piece of `Config` this app
    /// still needs after startup (every other knob is baked into the
    /// first `DirectoryView` at construction and never read back — see
    /// `Self::new`'s comment on `config`). Resolved the rest of the way
    /// (`$TERMINAL`, then `alacritty`) at the point of use via
    /// `core::apps::resolve_terminal_from_env`, not cached here — it's a
    /// cheap env read, not worth a second cache.
    terminal: Option<String>,
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

        // The sidebar's place list (Stage 7): built once, the same
        // "walk it at startup, never again" posture `mime_db`/`apps_db`
        // already take below — `core::places::UserDirs::load`/
        // `load_bookmarks` degrade to empty on a missing/unreadable file,
        // never fail `App::new` itself.
        let user_dirs = core::places::UserDirs::load(&home);
        let bookmarks = core::places::load_bookmarks(&home);
        let places = core::places::build(&home, &user_dirs, &bookmarks, &config.servers);

        let fallback = Location::local(home);

        // Most of `config` is consumed entirely here (view/sort/
        // show-hidden/actions defaults baked into the first
        // `DirectoryView` at construction, custom actions cloned onto it
        // too); `App` only keeps `terminal` back out as its own field
        // (Stage 6 — resolving "open in terminal"/`Terminal=true` apps
        // needs it after startup, not just at construction). A future
        // "new tab" action that needs the rest of these defaults would
        // resolve `Config::load` again or thread a clone through at that
        // point.
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
            sidebar: ui::sidebar::Sidebar::new(places),
            mime_db: core::mime::MimeDb::new(),
            apps_db: core::apps::AppsDb::load(),
            terminal: config.terminal.clone(),
        };
        (
            app,
            task.map(|m| Message::Explorer(ui::explorer::Message::Directory(m))),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Window(event) => ui::window::update(event),
            Message::Explorer(ui::explorer::Message::Sidebar(inner)) => {
                match self.sidebar.update(inner) {
                    Some(ui::sidebar::Event::OpenDirectory(location)) => {
                        self.navigate_active(location)
                    }
                    None => Task::none(),
                }
            }
            Message::Explorer(ui::explorer::Message::Directory(inner)) => {
                let Some(view) = self.views.get_mut(self.active) else {
                    return Task::none();
                };
                let (task, event) = view.update(inner);
                let mut tasks = vec![directory_task(task)];
                if let Some(event) = event {
                    tasks.push(self.handle_directory_event(event));
                }
                Task::batch(tasks)
            }
        }
    }

    /// Navigates the active `DirectoryView` to `location` — the shared
    /// tail end of both `ui::sidebar::Event::OpenDirectory` (a places-row
    /// click) and `ui::dirview::Event::OpenDirectory` (ascend/breadcrumb/
    /// descend) below, since both ultimately mean the same thing: "show
    /// this location in the one tab there currently is."
    fn navigate_active(&mut self, location: Location) -> Task<Message> {
        let Some(view) = self.views.get_mut(self.active) else {
            return Task::none();
        };
        directory_task(view.navigate(location))
    }

    /// The owner's response to a `DirectoryView` `Event` — the view only
    /// ever requests a navigation or an open/spawn, never performs one
    /// itself (see `ui::dirview`'s module docs). Every branch here is a
    /// synchronous `Command::spawn` (a detached process launch returns
    /// immediately — there's nothing to `.await`), so none of these need
    /// an `iced::Task`; a spawn failure is worded to stderr rather than
    /// surfaced in the UI (no error-dialog surface exists yet, and a
    /// failed launch is exactly the kind of thing CLAUDE.md's no-panic
    /// posture says should degrade quietly, not take the app down).
    fn handle_directory_event(&mut self, event: ui::dirview::Event) -> Task<Message> {
        match event {
            ui::dirview::Event::OpenDirectory(location) => self.navigate_active(location),
            ui::dirview::Event::Activated(locations) => {
                self.open_with_default_app(&locations);
                Task::none()
            }
            ui::dirview::Event::OpenWith(locations, desktop_id) => {
                let Some(entry) = self.apps_db.entry(&desktop_id).cloned() else {
                    eprintln!("saola-files: {desktop_id} is no longer installed");
                    return Task::none();
                };
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                if let Err(err) = core::apps::open(&entry, &paths, &terminal) {
                    eprintln!("saola-files: could not open with {}: {err}", entry.name);
                }
                Task::none()
            }
            ui::dirview::Event::OpenTerminal(location, is_dir) => {
                let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
                if let Err(err) = core::apps::open_terminal_here(&terminal, is_dir, &location.path)
                {
                    eprintln!("saola-files: could not open a terminal at {location}: {err}");
                }
                Task::none()
            }
            ui::dirview::Event::RunCustomAction(exec, locations) => {
                let paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
                for argv in core::apps::build_argv(&exec, &paths) {
                    let Some((program, args)) = argv.split_first() else {
                        continue;
                    };
                    if let Err(err) = core::apps::spawn_argv(program, args) {
                        eprintln!("saola-files: custom action \"{exec}\" failed: {err}");
                    }
                }
                Task::none()
            }
        }
    }

    /// `Event::Activated`'s handling: resolve each location's default app
    /// from its name-guessed mimetype and open it — the "files open in
    /// the right app" done criterion. A location with no name, no
    /// resolvable mimetype association, or an association naming an app
    /// this database never found all degrade to a worded `eprintln!`
    /// rather than silently doing nothing or panicking.
    fn open_with_default_app(&self, locations: &[Location]) {
        for location in locations {
            let Some(name) = location.path.file_name() else {
                continue;
            };
            let mimetype = self.mime_db.guess(name, None);
            let Some(entry) = self.apps_db.default_for(&mimetype) else {
                eprintln!("saola-files: no app is associated with {mimetype} for {location}");
                continue;
            };
            let terminal = core::apps::resolve_terminal_from_env(self.terminal.as_deref());
            if let Err(err) =
                core::apps::open(entry, std::slice::from_ref(&location.path), &terminal)
            {
                eprintln!("saola-files: could not open {location}: {err}");
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let keyboard = iced::keyboard::listen().map(|event| {
            Message::Explorer(ui::explorer::Message::Directory(
                ui::dirview::Message::Keyboard(event),
            ))
        });

        // Stage 5: the active view's own live-update watch, if its backend
        // has one — `None`/an out-of-range `active` degrades to "no watch
        // subscription" rather than a panic, same posture as `App::view`'s
        // own `views.get(self.active)` guard.
        let watch = self
            .views
            .get(self.active)
            .map(|view| view.subscription().map(directory_message))
            .unwrap_or_else(Subscription::none);

        // Stage 7: the places sidebar's own live udisks feed — one for the
        // app's whole lifetime (`ui::sidebar::Sidebar::subscription`'s doc
        // comment), batched in beside the keyboard/watch streams above.
        let mounts = self
            .sidebar
            .subscription()
            .map(|m| Message::Explorer(ui::explorer::Message::Sidebar(m)));

        Subscription::batch([keyboard, watch, mounts])
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
            Some(view) => ui::explorer::view(
                t,
                &self.sidebar,
                view,
                &self.mime_db,
                &self.apps_db,
                Message::Explorer,
            ),
            // Degrades to a blank paper surface rather than panicking —
            // `active` should always be in range, but the no-panic rule
            // means "should" isn't good enough.
            None => iced::widget::Space::new().into(),
        };

        ui::window::view(t, "Files", body, Message::Window)
    }
}

/// `ui::dirview::Message -> Message`, as a bare `fn` — used wherever a
/// `Subscription`'s own identity depends on the mapper being a plain
/// function pointer rather than a capturing closure (`ui::dirview::watch`'s
/// module docs explain why `Subscription::run_with`'s builder has the same
/// constraint). `Message::Explorer(explorer::Message::Directory(inner))` is
/// itself already `Fn(dirview::Message) -> Message`-shaped via ordinary
/// tuple-variant construction, but nested two enums deep it isn't
/// expressible as a single path the way `Message::Directory` used to be —
/// this free function is that composition, named once.
fn directory_message(inner: ui::dirview::Message) -> Message {
    Message::Explorer(ui::explorer::Message::Directory(inner))
}

/// Wraps a `Task<dirview::Message>` into `Task<Message>` — the same
/// `directory_message` composition, for the `Task::map` call sites in
/// `App::update`.
fn directory_task(task: Task<ui::dirview::Message>) -> Task<Message> {
    task.map(directory_message)
}
