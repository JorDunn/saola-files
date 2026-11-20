//! saola-files — the file manager for the Saola desktop environment.
//!
//! Current scope (through Stage 10): the application shell (Stage 1), CLI
//! parsing and `files.toml` loading (Stage 2), real directory browsing
//! through a VFS `Backend` trait and a local backend, navigation chrome
//! and live updates (Stages 4–5), mime/icons/opening (Stage 6), the places
//! sidebar (Stage 7), copy/move/rename/new-folder/new-file with live
//! progress and conflict prompts (Stage 8), Trash (Stage 9), and now a
//! session undo stack plus foreign (Wayland) clipboard interop (Stage 10).
//!
//! This binary is a thin shell over the `saola_files` library crate
//! (`src/lib.rs`) — see that file's docs for why the split exists.

mod app;

use iced::{Size, window};
use saola_theme::{Theme, convert};

use app::App;
use saola_files::{cli, config};

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
        platform_specific: window::settings::PlatformSpecific {
            // iced's own doc on this field: pick an app id matching the
            // .desktop file's basename and its Icon= key — so the desktop
            // entry is `resources/io.saola.Files.desktop` and its icon is
            // named `io.saola.Files`. This is deliberately NOT the D-Bus
            // name `io.saola.Files1` served in `integration::dbus` — the
            // trailing `1` there is interface versioning (à la
            // `org.freedesktop.FileManager1`), not part of this identity.
            application_id: "io.saola.Files".to_string(),
            ..window::settings::PlatformSpecific::default()
        },
        ..window::Settings::default()
    })
    .window_size(Size::new(1100.0, 720.0))
    .run()
}
