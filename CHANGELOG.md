# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0](https://github.com/JorDunn/saola-files/compare/saola-files-v0.2.0...saola-files-v0.3.0) - 2026-11-20

### Added

- *(ops)* rename many files at once ([#118](https://github.com/JorDunn/saola-files/pull/118))
- *(ui)* show counts, selection size and free space in a status bar ([#116](https://github.com/JorDunn/saola-files/pull/116))
- *(config)* let users rebind keyboard shortcuts in files.toml ([#113](https://github.com/JorDunn/saola-files/pull/113))
- *(thumbs)* generate large thumbnails, cache failures and dispatch by mimetype ([#107](https://github.com/JorDunn/saola-files/pull/107))
- *(state)* remember window size, last location and per-folder view settings ([#104](https://github.com/JorDunn/saola-files/pull/104))
- *(mime)* recognise file types by content when the name is not enough ([#98](https://github.com/JorDunn/saola-files/pull/98))
- *(vfs)* add symlink, permission and free-space operations to backends ([#99](https://github.com/JorDunn/saola-files/pull/99))
- *(i18n)* route user-visible strings through a message catalogue ([#95](https://github.com/JorDunn/saola-files/pull/95))

### Changed

- *(ui)* route directory views by ViewId and nest menu and app actions ([#92](https://github.com/JorDunn/saola-files/pull/92))
- *(dirview)* split the directory view into focused modules ([#62](https://github.com/JorDunn/saola-files/pull/62))
- *(app)* split main.rs into the app module tree ([#6](https://github.com/JorDunn/saola-files/pull/6))

### Fixed

- *(ops)* never lose data on a failed, cancelled or same-file overwrite ([#121](https://github.com/JorDunn/saola-files/pull/121))
- *(vfs)* open remote files as a downloaded read-only copy ([#115](https://github.com/JorDunn/saola-files/pull/115))
- *(ui)* report a failed New Folder even after navigating away ([#114](https://github.com/JorDunn/saola-files/pull/114))
- *(ui)* open the context menu at the pointer on right-click and from the Menu key ([#110](https://github.com/JorDunn/saola-files/pull/110))
- *(ui)* show operation and open failures in the window ([#102](https://github.com/JorDunn/saola-files/pull/102))
- *(ui)* separate the size and date columns in list view ([#105](https://github.com/JorDunn/saola-files/pull/105))
- *(ui)* keep keyboard shortcuts away from the directory behind a modal ([#103](https://github.com/JorDunn/saola-files/pull/103))
- *(ci)* pin the Rust toolchain and clear the chunks_exact lint ([#2](https://github.com/JorDunn/saola-files/pull/2))

## [0.2.0](https://github.com/JorDunn/saola-files/compare/saola-files-v0.1.0...saola-files-v0.2.0) - 2026-09-07

### Added

- add the io.saola.Files desktop entry
- *(dbus)* serve org.freedesktop.FileManager1 + io.saola.Files1, single-instance activation
- *(config)* surface knob — ink window variant
- *(ui)* responsive column count in grid view
- *(ui)* width-adaptive name truncation in list view via responsive
- *(sftp)* SFTP module + connect UI with host-key/auth prompts
- *(ui)* properties dialog with live directory-size streaming
- thumbnails — freedesktop cache, thumbnailer registry, LRU, viewport-driven requests
- undo stack + foreign clipboard interop
- trash — freedesktop trash engine, trash view, delete/permanent-delete
- ops engine — streamed copy/move, conflicts, clipboard, rename, new folder/file
- places sidebar — provider registry, UDisks2 mounts, sidebar UI
- mime detection, Lucide icon set, app launching, context menus
- live updates — inotify watch stream, per-view subscription, incremental apply
- navigation chrome — header, breadcrumbs, history, grid, type-ahead
- VFS backend trait, local module, and virtualized list view
- files.toml config loader and hand-rolled CLI

### Changed

- *(theme)* adopt saola-theme 0.8 breadcrumb and icon switcher
- *(theme)* adopt saola-theme 0.7 upstreamed styles and icons

### Fixed

- *(ui)* set the Wayland app_id to saola-files
- *(ui)* scroll the breadcrumb trail instead of overlapping the toolbar
- *(ui)* truncate a failed rename's message inside the date column
- *(ui)* width-adaptive truncation in the trash browser
- *(ui)* width-aware grid label budget (saola-theme 0.10)
- *(ui)* truncate grid tile labels with … (saola-theme 0.9)
- *(ui)* detect row double-clicks in update, not via mouse_area
- *(ui)* centre content in buttons and unify island-gap region geometry
