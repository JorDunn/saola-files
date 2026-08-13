//! Hand-rolled command-line parsing.
//!
//! Two flags and one positional don't justify a clap dependency (the
//! panel's precedent; saola-capture needs clap only because it multiplexes
//! CLI verbs). The grammar:
//!
//! ```text
//! saola-files [OPTIONS] [PATH|URI]
//!
//!   PATH|URI            directory: browse it; file: reveal it (open the
//!                       parent with the file selected) — `saola-files
//!                       /some/file` is a universal "reveal" command
//!   --select <PATH>     reveal PATH (same as passing a file positional)
//!   --config-dir <DIR>  read files.toml from DIR instead of the standard
//!                       chain
//!   -V, --version       print version and exit
//!   -h, --help          print usage and exit
//! ```
//!
//! Both `--flag value` and `--flag=value` spellings are accepted. Arguments
//! stay `OsString` — paths are bytes on Linux and non-UTF-8 names must
//! survive the trip (the CLAUDE.md OsString discipline); only an argument
//! that has to be *matched* as a flag is inspected as UTF-8.
//!
//! Once a running instance exists, a second invocation forwards its target
//! over D-Bus and exits (Stage 15 — see `integration::dbus`'s module doc
//! comment for the activation handshake); until then every invocation
//! opens its own window. `Hash` is derived on [`Cli`] because `App` hands
//! a clone of it to `Subscription::run_with` as that D-Bus subscription's
//! identity key (`integration::dbus::subscription`) — it never changes
//! after startup, so this is just satisfying the bound, not meaningfully
//! discriminating between different `Cli` values the way `ConnectRequest`'s
//! by-id `Hash` does.

use std::ffi::OsString;
use std::path::PathBuf;

/// What the process was asked to do. `Run` carries the parsed options;
/// `Version`/`Help` short-circuit in `main` before any window exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Run(Cli),
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Cli {
    /// The positional PATH or URI to open. Interpretation (directory vs
    /// file vs remote URI) happens against the VFS in Stage 3 — parsing
    /// doesn't touch the filesystem.
    pub target: Option<OsString>,
    /// `--select`: reveal this path (parent directory + selection).
    pub select: Option<PathBuf>,
    /// `--config-dir`: overrides the whole config resolution chain.
    pub config_dir: Option<PathBuf>,
}

pub const USAGE: &str = "\
Usage: saola-files [OPTIONS] [PATH|URI]

  PATH|URI            directory: browse it; file: reveal it in its parent
  --select <PATH>     reveal PATH (open its parent with it selected)
  --config-dir <DIR>  read files.toml from DIR
  -V, --version       print version and exit
  -h, --help          print this help and exit";

/// Parse the process arguments (everything after argv[0]). Errors are a
/// human-readable message; `main` prints it with the usage text and exits
/// non-zero. Pure function of its input — the unit tests below feed it
/// argument vectors directly.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, String> {
    let mut cli = Cli::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        // Only arguments that *look like* flags need to be readable as
        // UTF-8; a non-UTF-8 argument can't match any flag, so it falls
        // through to the positional branch untouched.
        match arg.to_str() {
            Some("-V" | "--version") => return Ok(Invocation::Version),
            Some("-h" | "--help") => return Ok(Invocation::Help),
            Some("--select") => {
                let value = args
                    .next()
                    .ok_or_else(|| "--select needs a path".to_owned())?;
                cli.select = Some(PathBuf::from(value));
            }
            Some(flag) if flag.starts_with("--select=") => {
                // `--flag=value` arrives as one argument; splitting it as
                // UTF-8 is fine here because `starts_with` already proved
                // the whole argument is valid UTF-8… except the value part
                // of a path may not be. `to_str` above returned Some, so
                // it is — non-UTF-8 `--select=<bytes>` is the one spelling
                // we can't accept, and `--select <bytes>` (two arguments)
                // is the escape hatch.
                cli.select = Some(PathBuf::from(&flag["--select=".len()..]));
            }
            Some("--config-dir") => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config-dir needs a directory".to_owned())?;
                cli.config_dir = Some(PathBuf::from(value));
            }
            Some(flag) if flag.starts_with("--config-dir=") => {
                cli.config_dir = Some(PathBuf::from(&flag["--config-dir=".len()..]));
            }
            Some(flag) if flag.starts_with('-') && flag.len() > 1 => {
                return Err(format!("unrecognized option: {flag}"));
            }
            // A positional: the PATH or URI to open. ("-" on its own is a
            // valid filename, so the flag branch above requires len > 1.)
            _ => {
                if cli.target.is_some() {
                    return Err("at most one PATH|URI may be given".to_owned());
                }
                cli.target = Some(arg);
            }
        }
    }

    Ok(Invocation::Run(cli))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn parse_strs(args: &[&str]) -> Result<Invocation, String> {
        parse(args.iter().map(OsString::from))
    }

    fn run(args: &[&str]) -> Cli {
        match parse_strs(args) {
            Ok(Invocation::Run(cli)) => cli,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn no_args_is_a_plain_run() {
        assert_eq!(run(&[]), Cli::default());
    }

    #[test]
    fn version_and_help_short_circuit() {
        assert_eq!(parse_strs(&["--version"]), Ok(Invocation::Version));
        assert_eq!(parse_strs(&["-V"]), Ok(Invocation::Version));
        assert_eq!(parse_strs(&["--help"]), Ok(Invocation::Help));
        // …even when other arguments are present.
        assert_eq!(parse_strs(&["/tmp", "-h"]), Ok(Invocation::Help));
    }

    #[test]
    fn positional_target_and_flags_in_both_spellings() {
        let cli = run(&["--config-dir", "/tmp/conf", "/home/j/Downloads"]);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/conf")));
        assert_eq!(cli.target, Some(OsString::from("/home/j/Downloads")));

        let cli = run(&["--config-dir=/tmp/conf", "--select=/tmp/a.txt"]);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/conf")));
        assert_eq!(cli.select, Some(PathBuf::from("/tmp/a.txt")));
    }

    #[test]
    fn missing_flag_values_error() {
        assert!(parse_strs(&["--select"]).is_err());
        assert!(parse_strs(&["--config-dir"]).is_err());
    }

    #[test]
    fn unknown_flags_error_but_dash_is_a_filename() {
        assert!(parse_strs(&["--frobnicate"]).is_err());
        assert!(parse_strs(&["-x"]).is_err());
        assert_eq!(run(&["-"]).target, Some(OsString::from("-")));
    }

    #[test]
    fn two_positionals_error() {
        assert!(parse_strs(&["/a", "/b"]).is_err());
    }

    #[test]
    fn non_utf8_target_survives() {
        let raw = OsString::from_vec(vec![b'/', 0xff, 0xfe]);
        let result = parse([raw.clone()]);
        assert_eq!(
            result,
            Ok(Invocation::Run(Cli {
                target: Some(raw),
                ..Cli::default()
            }))
        );
    }

    #[test]
    fn non_utf8_flag_value_survives_as_two_arguments() {
        let raw = OsString::from_vec(vec![b'/', 0x80]);
        let result = parse([OsString::from("--select"), raw.clone()]);
        assert_eq!(
            result,
            Ok(Invocation::Run(Cli {
                select: Some(PathBuf::from(raw)),
                ..Cli::default()
            }))
        );
    }
}
