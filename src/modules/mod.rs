//! Protocol backend registry, keyed by URI scheme.
//!
//! `local` is always compiled — every build needs local disk access, so it
//! isn't behind a feature. A future protocol (`sftp`, Stage 13) adds its
//! own module file plus one `#[cfg(feature = "...")]`-gated arm here; see
//! CLAUDE.md's "Adding a protocol" note.
//!
//! Backends are cheap to construct (`LocalBackend` holds no state), so
//! [`resolve`] builds a fresh one per call rather than caching instances
//! in a registry map. A protocol with real connection setup cost (SFTP's
//! auth handshake) *does* want its own session cache — that's
//! [`crate::core::remote::RemoteManager`] (Stage 14), which `resolve`
//! consults for any non-local scheme (see its own doc comment on why the
//! lookup goes through a process-wide handle rather than a parameter this
//! free function has no way to receive).
//!
//! **Stage 14 signature change:** `resolve` used to take a bare `scheme:
//! &str`. A remote backend needs the *authority* too (which server, not
//! just which protocol) to find the right pooled connection, and every
//! call site already had a full [`crate::core::vfs::Location`] in hand and
//! was just throwing the rest of it away — so this now takes the whole
//! `Location`, and every caller changed from `resolve(&x.scheme)` to
//! `resolve(&x)`, a mechanical, behavior-preserving edit for the `file`
//! scheme (which still ignores `authority` entirely).

pub mod local;
#[cfg(feature = "sftp")]
pub mod sftp;

use crate::core::vfs::{Backend, Location};

/// Look up the backend for a [`Location`], or `None` if nothing serves it:
/// an unrecognized scheme, a protocol compiled out via
/// `--no-default-features`, or — for a remote scheme — a server nothing
/// has connected to yet this session (`ui::dialogs::connect` is the only
/// place a pool entry is ever created; ordinary browsing/copy/undo code
/// only ever reads one back through here).
pub fn resolve(location: &Location) -> Option<Box<dyn Backend>> {
    if location.scheme == local::LocalBackend::SCHEME {
        return Some(Box::new(local::LocalBackend::new()));
    }

    // Stage 14: unlike `local`, a remote backend has no stateless "build
    // fresh per call" shape — connecting costs a real SSH handshake with
    // interactive prompts, which this synchronous function has nowhere to
    // route. So this falls through to whatever `core::remote::
    // RemoteManager` already has pooled for this exact (scheme,
    // authority) pair; `global_pooled` degrades to `None` the same way an
    // unrecognized scheme already did if nothing is pooled (no
    // `RemoteManager` installed yet, or this exact server was never
    // connected) — see that function's own doc comment.
    crate::core::remote::global_pooled(location)
        .map(|backend| Box::new(backend) as Box<dyn Backend>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_scheme_resolves_to_local_backend() {
        let backend = resolve(&Location::local("/")).expect("file scheme should resolve");
        assert_eq!(backend.scheme(), "file");
    }

    #[test]
    fn unknown_scheme_resolves_to_nothing() {
        let location = Location {
            scheme: "gopher".to_owned(),
            authority: None,
            path: std::path::PathBuf::from("/"),
        };
        assert!(resolve(&location).is_none());
    }
}
