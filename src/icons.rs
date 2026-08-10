//! Domain-specific icon mapping: file-manager business logic (entry kind,
//! mimetype category, place kind, mount removability) → the shared Saola
//! Lucide icon set.
//!
//! **Stage 12 (saola-theme 0.7 adoption):** every glyph this crate used to
//! embed locally — the panel/file-manager Lucide set (`Icon`, `icon()`,
//! `include_bytes!` assets, the stroke-width/viewBox asset tests) — now
//! lives upstream in [`saola_theme::icon`] (76 variants, including the whole
//! file-manager set this crate used). What's left here is *only* the
//! file-manager-specific mapping from domain types (`EntryKind`,
//! `mime::Category`, `PlaceKind`, mount removability) to an upstream
//! [`Icon`] variant — logic upstream has no business knowing about, so it
//! can't move any further than this.
//!
//! These were inherent methods on a locally-defined `Icon` (`Icon::for_entry`
//! etc.) before this stage; now that `Icon` is a foreign type
//! (`saola_theme::icon::Icon`), Rust's orphan rule forbids an inherent `impl`
//! block on it, so they are free functions instead — call sites read
//! `crate::icons::for_entry(...)` rather than `Icon::for_entry(...)`.

use saola_theme::icon::Icon;

use crate::core::fs::entry::EntryKind;
use crate::core::mime::Category;

/// The glyph for one row/tile entry — the "right glyph per type" stage-6
/// done-criterion (`ui::dirview::list`/`grid`). A symlink emblem always wins
/// over kind/mimetype (the entry itself — not whatever it points at — is
/// what `kind`/`category` describe here; see [`EntryKind`]'s docs on why a
/// symlinked directory still isn't `EntryKind::Directory`), then directories
/// get the folder glyph, then everything else differentiates by mimetype
/// [`Category`] — shape only, never hue (style guide §1). Deliberately a
/// plain replacement rather than a composited "folder + link badge": that
/// would need stacking two SVGs, which this stage's icon set doesn't need to
/// take on yet.
pub fn for_entry(kind: EntryKind, is_symlink: bool, category: Category) -> Icon {
    if is_symlink {
        return Icon::Link;
    }
    if kind == EntryKind::Directory {
        return Icon::Folder;
    }
    match category {
        // Defensive only — a non-directory `kind` with a `Directory`
        // category shouldn't happen (only `inode/directory` maps to it, and
        // only directories guess that mimetype), but this still has to
        // return *something* rather than being unreachable!()'d away, per
        // CLAUDE.md's no-panic rule.
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

/// The glyph for one places-sidebar row (Stage 7, `ui::sidebar`).
/// `Desktop`/`Documents` share the plain folder glyph — neither has a
/// dedicated Lucide icon in the upstream reserved places set, and a shortcut
/// to *a* folder reads fine as "a folder" the same way any other directory
/// row does.
pub fn for_place(kind: crate::core::places::PlaceKind) -> Icon {
    use crate::core::places::PlaceKind;
    match kind {
        PlaceKind::Home => Icon::House,
        PlaceKind::Downloads => Icon::Download,
        PlaceKind::Pictures => Icon::Image,
        PlaceKind::Music => Icon::Music,
        PlaceKind::Videos => Icon::Film,
        PlaceKind::Desktop | PlaceKind::Documents => Icon::Folder,
        PlaceKind::Bookmark => Icon::Bookmark,
        PlaceKind::Server => Icon::Server,
        PlaceKind::Trash => Icon::Trash2,
    }
}

/// The glyph for one places-sidebar mount row (Stage 7,
/// `core::udisks::Mount`) — shape carries removability, never hue (style
/// guide §1), the same rule [`for_entry`] follows for mimetype.
pub fn for_mount(removable: bool) -> Icon {
    if removable {
        Icon::Usb
    } else {
        Icon::HardDrive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `for_entry` — the mimetype/kind -> glyph mapping ─────────────────

    #[test]
    fn symlinks_always_get_the_link_glyph() {
        assert_eq!(
            for_entry(EntryKind::Directory, true, Category::Directory),
            Icon::Link
        );
        assert_eq!(for_entry(EntryKind::File, true, Category::Text), Icon::Link);
    }

    #[test]
    fn directories_get_the_folder_glyph_regardless_of_category() {
        assert_eq!(
            for_entry(EntryKind::Directory, false, Category::Generic),
            Icon::Folder
        );
    }

    #[test]
    fn files_differentiate_by_category_shape_not_hue() {
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Text),
            Icon::FileText
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Code),
            Icon::FileCode
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Image),
            Icon::FileImage
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Audio),
            Icon::FileAudio
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Video),
            Icon::FileVideo
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Archive),
            Icon::FileArchive
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Config),
            Icon::FileCog
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Generic),
            Icon::File
        );
        assert_eq!(
            for_entry(EntryKind::File, false, Category::Unknown),
            Icon::FileQuestion
        );
    }

    // ── `for_place`/`for_mount` (Stage 7) ─────────────────────────────────

    #[test]
    fn for_place_maps_every_kind_to_a_distinct_reserved_glyph() {
        use crate::core::places::PlaceKind;
        assert_eq!(for_place(PlaceKind::Home), Icon::House);
        assert_eq!(for_place(PlaceKind::Downloads), Icon::Download);
        assert_eq!(for_place(PlaceKind::Pictures), Icon::Image);
        assert_eq!(for_place(PlaceKind::Music), Icon::Music);
        assert_eq!(for_place(PlaceKind::Videos), Icon::Film);
        assert_eq!(for_place(PlaceKind::Bookmark), Icon::Bookmark);
        assert_eq!(for_place(PlaceKind::Server), Icon::Server);
        assert_eq!(for_place(PlaceKind::Trash), Icon::Trash2);
    }

    #[test]
    fn for_place_falls_back_to_the_folder_glyph_for_desktop_and_documents() {
        use crate::core::places::PlaceKind;
        assert_eq!(for_place(PlaceKind::Desktop), Icon::Folder);
        assert_eq!(for_place(PlaceKind::Documents), Icon::Folder);
    }

    #[test]
    fn for_mount_differentiates_removable_by_shape_not_hue() {
        assert_eq!(for_mount(true), Icon::Usb);
        assert_eq!(for_mount(false), Icon::HardDrive);
    }
}
