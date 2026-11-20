# External crates

saola-files depends on pure Rust only: no C-library bindings, for
protocols, codecs and decoders alike. When a feature needs a crate that
does not exist in pure Rust, or exists but is not fit for use, the crate is
written in its own repository outside the Saola project. This repo keeps a
brief for it here and a `blocked:external` task that waits until the crate
is published.

Each brief is one file, `<crate-or-topic>.md`, with four sections:
**Scope** (what the crate must do, and what it may leave out); **API** (the
calls the consuming saola-files module needs, as Rust signatures where
possible); **Test strategy** (how the crate proves it is correct, for
example reference files, a test server, or fuzzing); and **Licence notes**
(constraints on the format or on reimplementing it). Current briefs: `smb2-client.md` and `mtp.md`, both contingency briefs (a
suitable crate was found; see `docs/decisions/smb-mtp.md`), and
`rar-decoder.md` and `zstd-encoder.md`, also contingency briefs (see
`docs/decisions/archive-codecs.md`).
