# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/JorDunn/saola-files/compare/saola-files-v0.1.0...saola-files-v0.2.0) - 2026-08-11

### Added

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

- *(ui)* truncate grid tile labels with … (saola-theme 0.9)
- *(ui)* detect row double-clicks in update, not via mouse_area
- *(ui)* centre content in buttons and unify island-gap region geometry
