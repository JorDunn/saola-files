//! The one layer allowed to touch things outside this process: D-Bus so
//! far. CLAUDE.md's layering rule: `integration/` reaches the app only
//! through a bounded event channel (`ui::mod`'s own doc comment already
//! says `ui/` never imports `integration/` — this is that rule's other
//! half, spelled out where the code actually lives). `main.rs` is the one
//! place that both imports this module and constructs `App`; nothing under
//! `core/`/`ui/` ever does.

pub mod dbus;
