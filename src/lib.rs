//! saola-files' internals, split into a library target from the thin
//! `main.rs` binary purely so dead-code analysis has the right shape for a
//! staged build: `main.rs`'s `fn main` only reaches a fraction of this
//! crate's code so far (e.g. `Backend::mkdir`/`rename`/`remove` aren't
//! wired to any UI action until a later stage's ops engine), and in a
//! binary-only crate rustc treats "not reachable from `fn main`" as dead
//! regardless of `pub`. In a library target, `pub` items reachable from
//! the crate root are the reachability roots instead — the correct match
//! for a staged build where later stages progressively wire up trait
//! surface earlier stages only had to *define* correctly and test
//! directly. There's still exactly one shipped binary; this split is
//! invisible to a user running `saola-files`.
//!
//! CLAUDE.md's layering rule governs the modules below regardless of this
//! split: `core/` never imports `ui/`; `ui/` never imports `integration/`.

pub mod cli;
pub mod config;
pub mod core;
pub mod icons;
pub mod integration;
pub mod keymap;
pub mod modules;
pub mod ui;
