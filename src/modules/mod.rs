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
//! auth handshake) will want its own session cache — that lives on the
//! `App` as a shared cache when it lands, not here.

pub mod local;

use crate::core::vfs::Backend;

/// Look up the backend for a [`crate::core::vfs::Location`]'s scheme, or
/// `None` if nothing serves it (an unrecognized scheme, or a protocol
/// compiled out via `--no-default-features`).
pub fn resolve(scheme: &str) -> Option<Box<dyn Backend>> {
    if scheme == local::LocalBackend::SCHEME {
        return Some(Box::new(local::LocalBackend::new()));
    }

    // #[cfg(feature = "sftp")]
    // if scheme == sftp::SftpBackend::SCHEME {
    //     return Some(Box::new(sftp::SftpBackend::new(..)));
    // }
    // `sftp.rs` doesn't exist yet (Stage 13) — the `sftp` cargo feature is
    // reserved from Stage 1 but has nothing to register.

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_scheme_resolves_to_local_backend() {
        let backend = resolve("file").expect("file scheme should resolve");
        assert_eq!(backend.scheme(), "file");
    }

    #[test]
    fn unknown_scheme_resolves_to_nothing() {
        assert!(resolve("gopher").is_none());
    }
}
