//! SVG icon infrastructure: Lucide icons at stroke-width 2.75
//! (`saola_theme::tokens::Sizes::icon_stroke`), embedded at compile time and
//! tinted at *view* time from theme colors.
//!
//! Copied from `saola-panel/src/icons.rs`'s pattern (CLAUDE.md: "Icons are
//! Lucide outline, 24×24 viewBox, `stroke-width="2.75"` baked into the
//! asset, tinted at draw time. Copy the panel's `src/icons.rs` pattern") —
//! see that file's module doc comment for the full mechanics explanation
//! (`include_bytes!` at compile time, `svg::Style { color }`'s recolor-every-
//! pixel behavior in `iced_tiny_skia`'s vector cache, the `RasterKey`
//! caching-per-tint note). This copy only restates what's specific to this
//! crate:
//!
//! - Every asset here was fetched from `lucide-static` (the same source the
//!   panel used) with only `stroke-width="2"` mechanically bumped to
//!   `"2.75"` — no hand-edited glyphs like the panel's volume ladder, so
//!   every icon in this crate is stroke-based and the test module below has
//!   no solid-fill exemption list (contrast the panel's `SOLID`/
//!   `STROKE_ONLY` split for `Play`/`Anthropic`/`ClaudeCode`).
//! - **Mimetype differentiation is glyph *shape* only, never hue**
//!   (style guide §1, CLAUDE.md's design-language section): the `File*`
//!   family below (`FileText`, `FileImage`, `FileAudio`, …) is what a
//!   directory row/tile reads to tell file kinds apart, and every call site
//!   tints them with the same theme role a plain `File` glyph would get —
//!   never a per-mimetype color.

use iced::Color;
use iced::widget::svg::{Handle, Style};
use iced::widget::{Svg, svg};

use crate::core::fs::entry::EntryKind;
use crate::core::mime::Category;

/// One embedded icon asset. Each variant is one 24×24 Lucide SVG living
/// under `assets/icons/`.
///
/// Adding an icon: drop the `.svg` file in `assets/icons/` with
/// `stroke-width="2.75"` baked in, add a variant here, add its `bytes()`
/// arm, and add it to `tests::ALL` so the asset tests cover it automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    // ── Places (sidebar providers, Stage 7) ─────────────────────────────
    House,
    Download,
    Image,
    Music,
    Film,
    HardDrive,
    Usb,
    Trash2,
    Bookmark,
    Monitor,
    /// A saved remote server entry in the places sidebar (Stage 7/13).
    Server,
    /// "Connect" for a saved server entry.
    PlugZap,

    // ── File-kind glyphs — shape carries mimetype, never hue ────────────
    Folder,
    FolderOpen,
    /// The generic/unknown-mimetype file glyph.
    File,
    FileText,
    FileImage,
    FileAudio,
    FileVideo,
    FileCode,
    FileArchive,
    /// A `.desktop`/config-shaped file.
    FileCog,
    /// Unrecognized mimetype fallback, distinct from the generic [`File`][Icon::File].
    FileQuestion,
    /// The symlink emblem — composited over a row's base glyph, never
    /// replacing it (a symlinked directory still reads as a folder).
    Link,
    /// A permission-denied/inaccessible entry.
    Lock,

    // ── Navigation chrome ────────────────────────────────────────────────
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    /// Breadcrumb segment separator.
    ChevronRight,
    List,
    LayoutGrid,
    Eye,
    EyeOff,
    /// Ascending name/date/size sort indicator.
    ArrowDownAZ,
    /// Descending name/date/size sort indicator.
    ArrowUpAZ,
    /// The overflow-menu trigger — replaces Stage 4's `"⋯"` text placeholder.
    Ellipsis,
    /// The window close pill — replaces Stage 1's `"✕"` text placeholder.
    X,
    /// Rename.
    Pencil,
    /// Manual refresh (backends without `Caps::WATCH`).
    RefreshCw,

    // ── Clipboard/file actions ──────────────────────────────────────────
    Copy,
    /// Cut.
    Scissors,
    ClipboardPaste,
    FolderPlus,
    FilePlus,
    /// Undo (Stage 10).
    RotateCcw,
    /// "Open in terminal".
    Terminal,
    /// Properties (Stage 12).
    Info,
    /// "Open with…" / open in an external app.
    ExternalLink,
    /// A completed/selected affordance (e.g. the active entry in an
    /// Open-with popover).
    Check,
    /// A network/remote-scheme location.
    Globe,
}

impl Icon {
    /// The embedded SVG source bytes for this icon. `include_bytes!` reads
    /// the file at compile time (path relative to *this* source file) —
    /// see the module doc comment (and the panel's fuller version) for why
    /// that means no runtime I/O at all.
    fn bytes(self) -> &'static [u8] {
        match self {
            Icon::House => include_bytes!("../assets/icons/house.svg"),
            Icon::Download => include_bytes!("../assets/icons/download.svg"),
            Icon::Image => include_bytes!("../assets/icons/image.svg"),
            Icon::Music => include_bytes!("../assets/icons/music.svg"),
            Icon::Film => include_bytes!("../assets/icons/film.svg"),
            Icon::HardDrive => include_bytes!("../assets/icons/hard-drive.svg"),
            Icon::Usb => include_bytes!("../assets/icons/usb.svg"),
            Icon::Trash2 => include_bytes!("../assets/icons/trash-2.svg"),
            Icon::Bookmark => include_bytes!("../assets/icons/bookmark.svg"),
            Icon::Monitor => include_bytes!("../assets/icons/monitor.svg"),
            Icon::Server => include_bytes!("../assets/icons/server.svg"),
            Icon::PlugZap => include_bytes!("../assets/icons/plug-zap.svg"),
            Icon::Folder => include_bytes!("../assets/icons/folder.svg"),
            Icon::FolderOpen => include_bytes!("../assets/icons/folder-open.svg"),
            Icon::File => include_bytes!("../assets/icons/file.svg"),
            Icon::FileText => include_bytes!("../assets/icons/file-text.svg"),
            Icon::FileImage => include_bytes!("../assets/icons/file-image.svg"),
            Icon::FileAudio => include_bytes!("../assets/icons/file-audio.svg"),
            Icon::FileVideo => include_bytes!("../assets/icons/file-video.svg"),
            Icon::FileCode => include_bytes!("../assets/icons/file-code.svg"),
            Icon::FileArchive => include_bytes!("../assets/icons/file-archive.svg"),
            Icon::FileCog => include_bytes!("../assets/icons/file-cog.svg"),
            Icon::FileQuestion => include_bytes!("../assets/icons/file-question.svg"),
            Icon::Link => include_bytes!("../assets/icons/link.svg"),
            Icon::Lock => include_bytes!("../assets/icons/lock.svg"),
            Icon::ArrowLeft => include_bytes!("../assets/icons/arrow-left.svg"),
            Icon::ArrowRight => include_bytes!("../assets/icons/arrow-right.svg"),
            Icon::ArrowUp => include_bytes!("../assets/icons/arrow-up.svg"),
            Icon::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Icon::List => include_bytes!("../assets/icons/list.svg"),
            Icon::LayoutGrid => include_bytes!("../assets/icons/layout-grid.svg"),
            Icon::Eye => include_bytes!("../assets/icons/eye.svg"),
            Icon::EyeOff => include_bytes!("../assets/icons/eye-off.svg"),
            Icon::ArrowDownAZ => include_bytes!("../assets/icons/arrow-down-a-z.svg"),
            Icon::ArrowUpAZ => include_bytes!("../assets/icons/arrow-up-a-z.svg"),
            Icon::Ellipsis => include_bytes!("../assets/icons/ellipsis.svg"),
            Icon::X => include_bytes!("../assets/icons/x.svg"),
            Icon::Pencil => include_bytes!("../assets/icons/pencil.svg"),
            Icon::RefreshCw => include_bytes!("../assets/icons/refresh-cw.svg"),
            Icon::Copy => include_bytes!("../assets/icons/copy.svg"),
            Icon::Scissors => include_bytes!("../assets/icons/scissors.svg"),
            Icon::ClipboardPaste => include_bytes!("../assets/icons/clipboard-paste.svg"),
            Icon::FolderPlus => include_bytes!("../assets/icons/folder-plus.svg"),
            Icon::FilePlus => include_bytes!("../assets/icons/file-plus.svg"),
            Icon::RotateCcw => include_bytes!("../assets/icons/rotate-ccw.svg"),
            Icon::Terminal => include_bytes!("../assets/icons/terminal.svg"),
            Icon::Info => include_bytes!("../assets/icons/info.svg"),
            Icon::ExternalLink => include_bytes!("../assets/icons/external-link.svg"),
            Icon::Check => include_bytes!("../assets/icons/check.svg"),
            Icon::Globe => include_bytes!("../assets/icons/globe.svg"),
        }
    }

    /// An SVG handle for this icon. `Handle::from_memory` hashes the bytes
    /// it's given to build the handle's id, so calling this repeatedly for
    /// the same variant (once per `view`, say) is cheap — see the panel's
    /// fuller doc comment for the caching mechanics.
    fn handle(self) -> Handle {
        Handle::from_memory(self.bytes())
    }

    /// The glyph for one directory-row entry — the "right glyph per type"
    /// stage-6 done-criterion (`ui::dirview::list`/`grid`). A symlink
    /// emblem always wins over kind/mimetype (the entry itself — not
    /// whatever it points at — is what `kind`/`category` describe here;
    /// see [`EntryKind`]'s docs on why a symlinked directory still isn't
    /// `EntryKind::Directory`), then directories get the folder glyph,
    /// then everything else differentiates by mimetype [`Category`] —
    /// shape only, never hue (style guide §1). Deliberately a plain
    /// replacement rather than a composited "folder + link badge": that
    /// would need stacking two SVGs, which this stage's icon set doesn't
    /// need to take on yet.
    pub fn for_entry(kind: EntryKind, is_symlink: bool, category: Category) -> Icon {
        if is_symlink {
            return Icon::Link;
        }
        if kind == EntryKind::Directory {
            return Icon::Folder;
        }
        match category {
            // Defensive only — a non-directory `kind` with a `Directory`
            // category shouldn't happen (only `inode/directory` maps to
            // it, and only directories guess that mimetype), but this
            // still has to return *something* rather than being
            // unreachable!()'d away, per CLAUDE.md's no-panic rule.
            Category::Directory => Icon::Folder,
            Category::Text => Icon::FileText,
            Category::Code => Icon::FileCode,
            Category::Image => Icon::FileImage,
            Category::Audio => Icon::FileAudio,
            Category::Video => Icon::FileVideo,
            Category::Archive => Icon::FileArchive,
            Category::Config => Icon::FileCog,
            Category::Generic => Icon::File,
            Category::Unknown => Icon::FileQuestion,
        }
    }
}

/// Builds a sized, tinted [`Svg`] widget for `icon`.
///
/// `size` is a theme size token, in logical pixels — e.g.
/// `theme.sizes.icon_row`/`icon_menu`/`icon_bare`, never a literal number
/// (CLAUDE.md: zero hardcoded sizes). `color` is likewise always a theme
/// role — a `theme.on(Surface).primary`/`.secondary`/etc, or
/// `theme.palette.accent`, converted with `saola_theme::ColorExt::into_iced`
/// before it reaches this function.
pub fn icon<'a>(kind: Icon, size: f32, color: Color) -> Svg<'a> {
    svg(kind.handle())
        .width(size)
        .height(size)
        .style(move |_theme, _status| Style { color: Some(color) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded icon, so the tests below can walk the whole asset set
    /// without a second hand-maintained list of bytes.
    const ALL: [Icon; 50] = [
        Icon::House,
        Icon::Download,
        Icon::Image,
        Icon::Music,
        Icon::Film,
        Icon::HardDrive,
        Icon::Usb,
        Icon::Trash2,
        Icon::Bookmark,
        Icon::Monitor,
        Icon::Server,
        Icon::PlugZap,
        Icon::Folder,
        Icon::FolderOpen,
        Icon::File,
        Icon::FileText,
        Icon::FileImage,
        Icon::FileAudio,
        Icon::FileVideo,
        Icon::FileCode,
        Icon::FileArchive,
        Icon::FileCog,
        Icon::FileQuestion,
        Icon::Link,
        Icon::Lock,
        Icon::ArrowLeft,
        Icon::ArrowRight,
        Icon::ArrowUp,
        Icon::ChevronRight,
        Icon::List,
        Icon::LayoutGrid,
        Icon::Eye,
        Icon::EyeOff,
        Icon::ArrowDownAZ,
        Icon::ArrowUpAZ,
        Icon::Ellipsis,
        Icon::X,
        Icon::Pencil,
        Icon::RefreshCw,
        Icon::Copy,
        Icon::Scissors,
        Icon::ClipboardPaste,
        Icon::FolderPlus,
        Icon::FilePlus,
        Icon::RotateCcw,
        Icon::Terminal,
        Icon::Info,
        Icon::ExternalLink,
        Icon::Check,
        Icon::Globe,
    ];

    /// Binding constraint (CLAUDE.md, style guide §4): every embedded asset
    /// must have `stroke-width="2.75"` baked in at authoring time. Unlike
    /// the panel's icon set, every icon in this crate is a stock Lucide
    /// outline glyph (no hand-edited solid fills), so this walks `ALL`
    /// directly rather than needing a `STROKE_ONLY`/`SOLID` split.
    #[test]
    fn every_asset_bakes_in_the_theme_stroke_width() {
        for icon in ALL {
            let source = std::str::from_utf8(icon.bytes())
                .unwrap_or_else(|_| panic!("{icon:?}'s asset is not valid UTF-8"));
            assert!(
                source.contains("stroke-width=\"2.75\""),
                "{icon:?}'s asset is missing stroke-width=\"2.75\""
            );
        }
    }

    /// Cheap well-formedness check: catches a mismatched/truncated asset at
    /// `cargo test` time instead of leaving it for a runtime SVG-parse
    /// failure inside `resvg`, which `include_bytes!` itself can't catch
    /// (it embeds bytes, it doesn't parse them).
    #[test]
    fn every_asset_is_a_24x24_svg() {
        for icon in ALL {
            let source = std::str::from_utf8(icon.bytes()).unwrap();
            assert!(
                source.contains("viewBox=\"0 0 24 24\""),
                "{icon:?}'s asset is not a 24x24 viewBox"
            );
        }
    }

    // ── `Icon::for_entry` — the mimetype/kind -> glyph mapping ──────────

    #[test]
    fn symlinks_always_get_the_link_glyph() {
        assert_eq!(
            Icon::for_entry(EntryKind::Directory, true, Category::Directory),
            Icon::Link
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, true, Category::Text),
            Icon::Link
        );
    }

    #[test]
    fn directories_get_the_folder_glyph_regardless_of_category() {
        assert_eq!(
            Icon::for_entry(EntryKind::Directory, false, Category::Generic),
            Icon::Folder
        );
    }

    #[test]
    fn files_differentiate_by_category_shape_not_hue() {
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Text),
            Icon::FileText
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Code),
            Icon::FileCode
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Image),
            Icon::FileImage
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Audio),
            Icon::FileAudio
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Video),
            Icon::FileVideo
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Archive),
            Icon::FileArchive
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Config),
            Icon::FileCog
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Generic),
            Icon::File
        );
        assert_eq!(
            Icon::for_entry(EntryKind::File, false, Category::Unknown),
            Icon::FileQuestion
        );
    }

    /// None of these assets are filled shapes — every one still carries a
    /// real stroke (`fill="none"`), so a future accidental solid-fill
    /// asset would show up here rather than silently passing the
    /// stroke-width test above (which only checks the attribute is
    /// *present*, not that it's meaningful on a genuinely stroked path).
    #[test]
    fn every_asset_is_unfilled_outline_style() {
        for icon in ALL {
            let source = std::str::from_utf8(icon.bytes()).unwrap();
            assert!(
                source.contains("fill=\"none\""),
                "{icon:?}'s asset should be an unfilled outline (fill=\"none\")"
            );
        }
    }
}
