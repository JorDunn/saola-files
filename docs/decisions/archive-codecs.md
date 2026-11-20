# Decision: archive codecs

Date: 2026-11-20. Status: proposed (survey for issue #65). Follow-ups:
#66 (extract), #67 (compress), #68 (browse archives as folders).

## Verdict

Every format a desktop file manager is expected to open has a pure-Rust
reader today, and all but two have a pure-Rust writer. The backlog's
expectation (xz encoder, zstd encoder and RAR decoder all missing) was wrong
for xz and RAR and half wrong for zstd.

| Format             | Read | Write                      | Chosen crate (exact pin)                          |
| ------------------ | ---- | -------------------------- | ------------------------------------------------- |
| zip                | yes  | yes                        | `zip =9.8.0`                                      |
| tar                | yes  | yes                        | `tar =0.4.46`                                     |
| gzip (`.gz`)       | yes  | yes                        | `flate2 =1.1.11`, `zlib-rs` backend               |
| bzip2              | yes  | yes                        | `bzip2 =0.6.1` (pure `libbz2-rs-sys 0.2.7`)       |
| xz and lzma, lzip  | yes  | yes                        | `lzma-rust2 =0.25.0`                              |
| zstd               | yes  | poor ratio, needs a guard  | `ruzstd =0.9.1`                                  |
| 7z                 | yes  | yes, with defects          | `sevenz-rust2 =0.25.0`                            |
| rar (RAR 1.3 to 7) | yes  | no (by policy)             | `rars =0.13.0`, after a spike                     |
| lz4 (frame)        | yes  | yes                        | `lz4_flex =0.14.2`                                |
| cab                | yes  | not offered                | `cab =0.6.0`                                      |
| ar and `.deb`      | yes  | not offered                | `ar =0.9.0` plus the codecs above                 |
| cpio (newc)        | yes  | not offered                | `cpio =0.4.2`                                     |
| iso9660            | no   | no                         | none suitable; mount it instead                   |
| rpm                | no   | no                         | not surveyed (cpio plus a payload codec)          |

Compound names (`tar.gz`, `tar.bz2`, `tar.xz`, `tar.zst`, `tar.lz4`) are the
tar crate over the matching stream codec; all five read and write.

No external crate project is needed to start. Two briefs are written as
contingencies, in the style of the SMB and MTP survey:

- [`docs/external-crates/rar-decoder.md`](../external-crates/rar-decoder.md):
  only if the `rars` spike below fails.
- [`docs/external-crates/zstd-encoder.md`](../external-crates/zstd-encoder.md):
  only if zstd levels above 1, or a non-panicking encoder, become a
  requirement. Upstream contribution to `ruzstd` comes first.

Consequence for the board: no task needs `blocked:external` for archives.
This survey does not change labels; the dispatcher decides.

Two things block a faithful extraction that are **not** codec problems, and
the maintainer needs to see them before #66 starts (details in
[Backend gaps](#backend-gaps)): `Backend` cannot create symlinks or set
permissions, so "extract with correct modes" cannot be met through the trait
as it stands.

## Method and what was verified

All facts were collected on 2026-11-19 and 2026-11-20 on x86-64 Linux,
Rust 1.100.0 (the repository pins its toolchain in `rust-toolchain.toml`).

- Versions, release dates, licences and download counts: the crates.io API
  and `cargo info`. Repository activity: the GitHub API.
- **Purity.** A throwaway Cargo project per candidate (outside this
  repository), depending on the exact version and the exact features
  proposed below. `cargo tree -e normal,build` was searched for `-sys`
  crates, `cc`, `cmake`, `bindgen`, `pkg-config`, `openssl`, `ring`, and
  `aws-lc`. A hit is only a lead: three names that look alarming are pure
  Rust and are explained in the next section.
- **Behaviour.** One probe program (not committed) linking the chosen
  crates, run against:
  - fixtures built on the survey machine with the system `tar`, `gzip`,
    `bzip2`, `xz`, `zstd`, `lz4`, `zip` (Info-ZIP, including ZipCrypto and
    `-fz` zip64), `bsdtar` (7z, zip, ar) and `cpio`;
  - third-party fixtures: the `zip2` and `sevenz-rust2` repositories' test
    data (zip with AES, deflate64, lzma, xz, ppmd, data descriptors,
    symlinks, non-UTF-8 names, zip64; 7z with BCJ x86, BCJ ARM64, BCJ2,
    delta, PPMd, bzip2, AES-256, solid and non-solid) and 23 RAR files from
    the libarchive test suite (RAR 3, 4 and 5; compressed, solid, encrypted,
    ARM filter, BLAKE2, file attributes, and fuzzer-found regressions);
  - archives written by the crates and then checked with the reference
    tools (`gzip -t`, `bzip2 -t`, `xz -t`, `zstd -t`, `lz4 -t`, `unzip -t`,
    `bsdtar -tvf`, and `diff -r` of the extracted trees);
  - hostile archives made with Python (`..`, absolute, drive-letter,
    backslash and NUL names, a symlink pointing out of the target followed
    by a file written through it, a hard link to an outside file, a setuid
    mode, a device node, duplicate names). Their targets all pointed into a
    scratch directory; nothing outside it was at risk.
- **Memory and speed**: peak resident memory and wall time on a 195 MB text
  file, compared with the reference tools (see
  [Measurements](#measurements)).

### Not verified

Said once, up front:

- No fuzzing was run. Panic freedom on hostile input is unproven for every
  crate; the evidence is the fixtures above (the 23 RAR fixtures include
  about a dozen fuzzer-found malformed files; `rars` returned an error on
  each, none panicked or hung) and the crates' own claims.
- No Windows or macOS dependency trees (the project targets Linux).
- Writing AES-encrypted zips was not checked against an independent reader:
  `unzip` 6.0 and the system `bsdtar` cannot read AES zips. Only the `zip`
  crate's own reader opened it.
- Reading a zip entry with the `zstd` method (93) is not supported by the
  recommended feature set; the suggested route (raw access plus `ruzstd`) was
  not tried.
- RAR: no fixture made by the genuine WinRAR or `rar` was available, only
  libarchive's. Password-protected RAR decryption was exercised with the
  libarchive fixtures; one entry decrypted with a guessed password and the
  wrong-password errors were typed and clean, but the fixtures' intended
  passwords were not found, so full coverage is not shown.
- Timing was taken on a loaded machine; read the speed figures as rough.
- No legal review of the RAR provenance claims (see
  [RAR](#rar)).

## What "pure Rust" meant here

The rule (AGENTS.md): no C-library bindings, for codecs and decoders alike.
Judged per configuration, not per crate:

- Disqualifies: a `*-sys` crate that wraps a C library, a `cc`, `cmake`,
  `bindgen` or `pkg-config` build dependency, or bundled C source.
- Does **not** disqualify, despite the name or appearance:
  - `libbz2-rs-sys` 0.2.7: a Rust port of bzip2 from Trifecta Tech
    Foundation; no `cc` in its tree. It is what `bzip2` 0.6 uses by default.
    The *other* feature, `bzip2-sys`, is the C binding, and the `zip` crate's
    feature called `bzip2-rs` turns *that* on. Do not enable it.
  - `libc`, `rustix` and `linux-raw-sys`: Rust declarations of system calls,
    nothing to install. (The SMB and MTP survey drew the same line.)
  - `libzstd-rs-sys` is **not** in this group: see zstd.
- Defaults that fail the rule and must be switched off:
  - `zip` default features: `zstd` pulls `zstd-sys` (C). The `zstd-rs`
    feature swaps in `libzstd-rs-sys`, whose `build.rs` runs `cc` to
    assemble a `.S` file on x86-64 and which is a prerelease; also rejected.
  - `sevenz-rust2` `zstd` feature: same `zstd-sys`.
  - `unarc-rs`, `rarchive` and `unrar-rs`: each pulls `zstd-sys`, `cc` or
    `aws-lc-sys`; `unrar` and `unrar_sys` compile the C++ UnRAR source.

## Recommended dependency lines

Exact pins with `=`, as the SMB and MTP decision does, because several of
these crates break their API on minor releases. Each line was checked with
`cargo tree` on 2026-11-20 and has no flagged crate.

```toml
zip = { version = "=9.8.0", default-features = false, features = [
    "aes-crypto", "bzip2", "deflate", "deflate64", "lzma", "ppmd", "xz",
] }
tar = { version = "=0.4.46", default-features = false }
flate2 = { version = "=1.1.11", default-features = false, features = ["zlib-rs"] }
bzip2 = "=0.6.1"
lzma-rust2 = { version = "=0.25.0", default-features = false, features = [
    "std", "encoder", "xz", "lzip",   # add "optimization" only after a review, see below
] }
ruzstd = "=0.9.1"
sevenz-rust2 = { version = "=0.25.0", default-features = false, features = [
    "aes256", "bzip2", "compress", "ppmd", "util", "deflate", "brotli", "lz4",
] }
rars = { version = "=0.13.0", default-features = false, features = ["encryption"] }
lz4_flex = { version = "=0.14.2", features = ["frame"] }
```

The `archive` feature in `Cargo.toml` owns all of these as optional
dependencies, so `--no-default-features` still builds. Each addition needs
the dated survey comment AGENTS.md asks for; this document is the survey.

Notes on the lines:

- `tar` without default features drops the `xattr` crate. Extraction never
  calls `Entry::unpack` here (it would bypass the `Backend`), so extended
  attributes are never needed.
- `zip` `time` and `chrono` features are left off; read modification times
  from the extended-timestamp field, see below.
- `lzma-rust2` `optimization` is the one feature that adds `unsafe` and
  inline assembly (its README says so, and that turning it off gives 100
  percent standard Rust). It is off here. Whether the speed gain matters is
  a measurement the compress task can make.
- `sevenz-rust2` MSRV is 1.93 and `zip` is 1.89; the repository's toolchain
  is newer. `rars` is 1.89.
- Licences: `zip` MIT; `tar` and `flate2` MIT OR Apache-2.0; `bzip2`
  MIT OR Apache-2.0 with `libbz2-rs-sys` under the bzip2 licence;
  `ruzstd` MIT; `lz4_flex` MIT; **`lzma-rust2`, `sevenz-rust2` and `rars` are
  Apache-2.0 only**. saola-files is MIT OR Apache-2.0, and Apache-2.0
  dependencies are compatible with shipping under either; the binary
  distribution just carries the Apache notices. `ppmd-rust` (via `zip` and
  `sevenz-rust2`) is CC0-1.0 OR MIT-0.

## Findings per format

### zip

Crate `zip` 9.8.0 (released 2026-11-19, MIT, repository `zip-rs/zip2`, 391
stars, last commit 2026-11-20, MSRV 1.89). The fork that replaced the
unmaintained `zip` 0.6; the most used crate in this survey (109 M recent
downloads).

- Read: stored, deflate, deflate64, bzip2, lzma, xz, ppmd, plus the legacy
  shrink, reduce and implode. zip64: read from a `zip -fz` archive and the
  `zip64_demo.zip` fixture. Data descriptors, symlink entries, comments,
  extended timestamp extra fields, NTFS times: present in the API.
- Write: stored, deflate (levels 1 to 9 via `flate2`, 10 to 264 via
  `zopfli`), bzip2, xz, ppmd, zip64 (`large_file(true)` or
  `set_auto_large_file`). `lzma` and `deflate64` are read only. Output from
  the crate passed `unzip -t` for deflate, stored and zip64.
- **Encrypted entries.** ZipCrypto (the weak legacy scheme) and AES-128,
  192 and 256 (WinZip AE-1 and AE-2) are readable with a password given to
  `by_index_decrypt`. Verified: Info-ZIP's `zip -e` archive opens with the
  right password; `zip2`'s AES fixtures decode with theirs. With no password,
  `by_index` fails with "Password required to decrypt file" and with a wrong
  one with "provided password is incorrect" (both ZipCrypto and AES);
  neither writes partial data. These are errors in an
  `unsupported`-style variant and the text, not a dedicated variant, so the
  task should check `ZipFile::encrypted()` through raw access first and
  prompt before reading. Writing AES is available
  (`with_aes_encryption`); writing ZipCrypto is crate-private in 9.8.0. #67
  excludes encryption, so neither is offered.
- Seek: **required** for the normal reader (`ZipArchive::new` needs
  `Read + Seek`, reading the central directory at the end). A streaming
  local-header reader exists (`ZipStreamReader`, listed under `unstable` in
  the README); not tried, and it cannot see entries the central directory
  would add or hide. Plan on seekable only.
- Async: none; the README says to use `spawn_blocking`.
- Memory: the whole central directory is held in memory (one record per
  entry; the cost for a million entries was not measured). Entry data
  streams.
- Names: `name_raw()` returns the bytes as stored. `name()` decodes CP437
  unless the UTF-8 flag is set, so the Latin-1 byte `0xE9` came back as
  `Θ` and Shift-JIS and GBK names from `non_utf8.zip` came back as box-drawing
  characters. **Use `name_raw()`**, never `name()` or `enclosed_name()`, for
  the on-disk name (see the maintainer decision on encodings). The writer
  takes `String` names only, so a file with a non-UTF-8 name cannot be added
  to a zip (the probe skipped it; `start_file_from_path` also converts).
- Path safety: `enclosed_name()` returned `None` for `../x`,
  `a/../../x` and `dir\..\..\x`; it stripped a leading `/` and a drive
  letter (`C:\x` became `x`); it cut a name at an embedded NUL. It is
  string-based, so we write our own byte-based check anyway.
  `has_duplicate_names()` flags duplicates. A symlink entry is reported by
  `is_symlink()` with `unix_mode` `0o120777`; its content is the target.
- Permissions and times: `unix_mode()` gave `0o100755` for the executable
  in Info-ZIP and `zip`-crate archives. `last_modified()` is the DOS
  timestamp: local time, two-second resolution, no time zone, so an entry
  stored at 04:05:06 UTC read back as 23:05:06 on a UTC-5 machine. The
  extended-timestamp extra field (`ExtendedTimestamp::mod_time`, Unix
  seconds) is the accurate source and Info-ZIP writes it; the field's API
  exists in the source, but reading it through the entry was not tried. On
  write, `unix_permissions` is honoured; `last_modified_time` defaults to
  "now" if not set, so the compress task must set it.
- Zstandard entries (method 93) are unreadable with this feature set (the
  `zstd` crate is C). `by_index_raw` exposes the compressed bytes, so
  routing method 93 through `ruzstd` is plausible; not tried. Until then,
  word it as unsupported.
- Maintenance: very active; 32 open issues; several breaking releases in
  2025 and 2026 (pin exactly). The crate has had security fixes for
  path-handling and decompression-size problems; read its `SECURITY.md`
  before bumping.

### tar

Crate `tar` 0.4.46 (released 2026-05-18, MIT OR Apache-2.0, repository
`composefs/tar-rs` after the move from alexcrichton, 747 stars, last commit
2026-11-09, 169 open issues). Pure Rust; the tree is `filetime`, `libc` and,
only with default features, `xattr` and `rustix`.

- Read and write: ustar, GNU (long names, sparse), and pax. Verified by
  reading `tar --format=gnu` and `--format=posix` archives made by GNU tar
  and by writing a tree that `tar -x` extracted identically (`diff -r`).
- Streaming: yes. `Archive::new(impl Read)` and `entries()` need no `Seek`;
  an entry must be read or skipped before the next. This makes every `tar.*`
  usable straight from a `Backend::read` stream, with no download first.
- Names are bytes (`path_bytes()`, `link_name_bytes()`). The Latin-1 name
  `caf\xe9.txt` and the UTF-8 name round-tripped.
- Permissions and times: `header().mode()` and `mtime()` (Unix seconds)
  preserved through gz, bz2, xz, zst and lz4 round trips; the writer stored
  the source file's mode and mtime.
- Path safety: the crate's own `unpack_in(dest)` refused `../x` and
  `a/../../x` (`Ok(false)`), re-rooted `/abs/x` under the destination,
  created a symlink to an outside directory without complaint but then
  refused to write `sym/file` through it, and refused a hard link to an
  outside file. Good, but it uses `std::fs`, which this project does not
  use outside `src/modules/`; so we use `entries()` and our own checks.
  Raw headers pass through dangerous values: a setuid mode `4755` and a
  character device entry were reported as ordinary entries. **The extractor
  must mask the mode and skip special files itself.**
- Not async. For streaming from an async `Backend::read`, bridge in a
  blocking thread (see [Async and blocking](#async-and-blocking)). The
  async forks (`astral-tokio-tar` 0.7.1; the GitHub repository is marked
  archived) are not needed because the codecs under them are blocking.

### gzip

Crate `flate2` 1.1.11 (released 2026-11-06, MIT OR Apache-2.0, 1,132 stars,
last commit 2026-11-18). Backends: `rust_backend` (`miniz_oxide`, the
default) and `zlib-rs` (Trifecta Tech's pure-Rust zlib, 0.6.8 released
2026-09-15); both have clean trees. `zlib-rs` is chosen because `zip` and
`sevenz-rust2` already use it, so the tree carries one deflate.

- `GzDecoder` reads a single member; **`MultiGzDecoder`** handles
  concatenated members (what `pigz` and `cat a.gz b.gz` produce) and is the
  right default. Streaming, no `Seek`. `GzEncoder` wrote a `tar.gz` that
  `gzip -t` and `tar -tzf` accepted and `diff -r` matched.
- Memory: constant (32 KiB window), 11 MiB peak for the whole write probe.

### bzip2

Crate `bzip2` 0.6.1 (released 2025-10-16, MIT OR Apache-2.0, last commit
2026-02-09, 20 open issues), default backend `libbz2-rs-sys` 0.2.7
(released 2026-11-19, bzip2 licence, 79 stars). Pure, as explained above.

- Read with `MultiBzDecoder` (concatenated streams), write with `BzEncoder`;
  output passed `bzip2 -t`. Streaming, no `Seek`.
- Speed: slower than the C tool, see Measurements.
- The standalone `bzip2-rs` 0.1.2 (2021) is read-only and unmaintained;
  not used.

### xz, lzma, lzip

Crate `lzma-rust2` 0.25.0 (released 2026-11-01, Apache-2.0, last commit
2026-11-19, 53 stars; the maintained fork of `lzma-rust`, by the same author
as `sevenz-rust2`). **This is the crate the backlog expected to be
missing**: it has `XzReader` and `XzWriter` (and `XzReaderMt` and
`XzWriterMt` for multithreaded blocks), `LzmaReader` and `LzmaWriter` for
`.lzma`, `Lzma2Reader` and `Lzma2Writer`, and `LzipReader` and `LzipWriter`.

- Verified: `XzReader` decoded `tar.xz` made by `xz -6` and `xz -9`, in
  single- and multi-block form; `XzWriter` (preset 6) wrote a `tar.xz` that
  `xz -t` accepted and `xz -l` reported as one stream with a CRC64 check;
  `tar -xJf` extracted it identically.
- Streaming, no `Seek`. Memory: `XzReader::new_mem_limit` caps decoder
  memory; the writer's memory is set by the preset (80 MiB peak on preset 6
  for this probe). Use the limit constructor on the read side and treat a
  `.xz` that asks for more as a worded error.
- The old `lzma-rs` 0.3.0 (2023, MIT) is read-and-write for `.lzma` and
  `.xz` decode only, unmaintained; not used. `xz2` and `liblzma` bind C.

### zstd

Decoder: `ruzstd` 0.9.1 (released 2026-10-11, MIT, 460 stars, last commit
2026-11-17). Pure, small tree (`twox-hash` for the checksum).

- Read: `StreamingDecoder` decoded `tar.zst` made by `zstd -3`, and a
  `--ultra -22 --long=27` file with a 128 MiB window (Measurements). Frame
  window size is attacker-controlled, so use
  `StreamingDecoder::new_with_max_window_size` with a ceiling (suggest
  128 MiB) and report larger as unsupported.
- Write: only `CompressionLevel::Uncompressed` and `Fastest` (about zstd
  level 1). `Fastest` output passed `zstd -t` and extracted identically, but compressed a 2.6 MB source tar to 802 kB against 553 kB for `zstd -1` and 510 kB for `gzip -6`, so it is worse than gzip.
  **Defects** (run, not just read): `Default`, `Better` and `Best` panic with
  `not implemented`, and an I/O error from the sink or source panics
  (`Result::unwrap()` on `Err`), because `compress` returns `()`. See the
  brief for the guard and for the case for improving it upstream.
- `zstd` 0.14.4 (BSD-3-Clause) and its `zstd-safe` are C bindings; with the
  `rust-backend` feature they use `libzstd-rs-sys` 0.0.1-prerelease.2, which
  still runs `cc` in `build.rs`. Rejected on both counts.
- `zip` entries and 7z blocks compressed with zstd are therefore not
  readable (7z zstd is a non-standard codec from a 7-Zip fork anyway).

### 7z

Crate `sevenz-rust2` 0.25.0 (released 2026-11-06, Apache-2.0, 129 stars,
last commit 2026-11-17, 4 open issues; fork of the unmaintained
`sevenz-rust`, same author as `lzma-rust2`). Pure with the feature set
above (tree of 34 crates for defaults, plus `flate2`/`zlib-rs`,
`lz4_flex` and `brotli` for the extra codecs).

- Codecs: copy, LZMA, LZMA2, PPMd, bzip2, deflate (feature), brotli and
  lz4 (features), zstd (feature, C: not used). Filters: BCJ x86, ARM,
  ARM64, ARM Thumb, RISC-V, PPC, SPARC, IA64, delta; BCJ2 is
  **decode only**. AES-256 encryption (with the 7z key derivation) read and
  write, including encrypted headers.
- Verified on 7-Zip-made fixtures: LZMA2 with BCJ2 (the 7-Zip 4.33
  distribution archive, 180 kB), BCJ x86, BCJ ARM64, delta, delta with
  BCJ2, PPMd, bzip2, solid and non-solid, and AES-256 with the right
  password (`encrypted.7z`, content matched). Without the `deflate`, `brotli`
  or `lz4` feature a block using that codec returns a typed
  `UnsupportedCompressionMethod("...")` error, not a panic. A wrong
  password returns an error from `read`.
- **Seek required** for reading (`ArchiveReader::new(R: Read + Seek)`: the
  header is at the end) and for writing (`ArchiveWriter<W: Write + Seek>`
  rewrites the start header). No streaming mode.
- Memory: `new_with_memory_limit` exists. For a LZMA2 archive the decoder
  needs the dictionary (up to 1.5 GiB on `-mx9` with a big dictionary), and
  a solid block must be decoded from its start to reach a late file, so
  extracting one file can cost the whole block.
- Names: `ArchiveEntry::name()` is a `String` (7z stores UTF-16). Lossless
  except for unpaired surrogates. A name that was not valid UTF-8 when
  `bsdtar` wrote the fixture came out as `U+FFFD` replacement characters:
  that was bsdtar's conversion under `LC_ALL=C`, not the reader. The
  **writer** (`push_source_path`) took the same non-UTF-8 file and wrote the
  lossy name without warning.
- Permissions and times: modification time is an NT timestamp (100 ns since
  1601; clamp before converting to `SystemTime`). Unix mode and symlink-ness
  are in the high 16 bits of `windows_attributes()` when bit `0x8000` is set
  (`0o100644` and `0o120777` read back correctly from the `bsdtar` archive;
  not set for archives from Windows 7-Zip, which show only `0x20` or
  `0x10`). **The writer drops both**: an executable written with
  `push_source_path` came back `0644`, and a symlink in the source tree was
  omitted. Do not offer 7z creation in the first release; if it is wanted
  later, patch upstream or set attributes through the lower-level
  `push_archive_entry`.
- Maintenance: good; breaking changes between 0.2x releases; pin exactly.

### rar

**A pure-Rust RAR reader exists.** The backlog's "no pure-Rust decoder" is
out of date.

| Crate           | Version | Released   | Licence    | Pure | Verdict                                         |
| --------------- | ------- | ---------- | ---------- | ---- | ----------------------------------------------- |
| `rars`          | 0.13.0  | 2026-11-17 | Apache-2.0 | yes  | Chosen, after a spike. Reads RAR 1.3 to 7.      |
| `rar-codec`     | 0.1.11  | 2026-11-15 | Apache-2.0 | yes  | Fallback. RAR4 and RAR5 decode; no encryption.  |
| `rar`           | 0.4.0   | 2025-11-05 | MIT        | yes  | RAR5 stored only; rejected.                     |
| `rarchive`      | 0.1.1   | 2026-11-09 | MIT        | no   | Pulls `zstd-sys`; rejected.                     |
| `unarc-rs`      | 0.8.1   | 2026-10-29 | MIT/Apache | no   | Pulls `zstd-sys` and `cc`; rejected.            |
| `unrar-rs`      | 0.16.6  | 2026-11-19 | non-standard | no | `aws-lc-sys`, `cmake`; rejected.                |
| `unrar`, `unrar_sys` | 0.5.8 | n/a    | MIT/Apache | no   | Wrap the C++ UnRAR source; rejected.            |

`rars` 0.13.0: repository `bitplane/rars`, 100 stars, 11 open issues, first
published 2026-05-13, last commit 2026-11-20, 1,900 of roughly 2,200
commits by one author. 131,000 lines of source and 36,000 of tests. Its
README calls it an "agentic development experiment" that "has since matured"
and says plainly that it is slower than WinRAR, uses more memory, and "could
use more testing at volume". `unsafe_code = "forbid"` in its lint table.
Default features: `write`, `parallel` (rayon), `recovery`, `encryption`. We
use `encryption` only (a smaller, all-RustCrypto tree: `aes`, `sha1`,
`sha2`, `hmac`, `blake2s_simd`).

- Verified: 23 libarchive fixtures. Decoded with correct names, sizes and
  CRC32: RAR 3 compressed (`rar3_lowdist_reset`), RAR 4 with a symlink and
  a directory, RAR 5 compressed, RAR 5 solid (7 members), RAR 5 ARM filter
  (a 90 kB ELF), RAR 5 BLAKE2, RAR 5 file attributes and multi-file
  archives. Errors, not panics: encrypted entries with no or a wrong
  password ("a password is required", "wrong password or corrupt encrypted
  data"), a malformed Huffman table ("oversubscribed Huffman table"), a
  truncated multi-volume part, several short fuzz-regression files
  ("input is too short"). In two cases `rars` is stricter than libarchive
  (libarchive lists `bytes_remaining_underflow` and `loop_bug`); that is
  the safe direction.
- API fit: `ArchiveReader::read_path`, `read_reader(impl Read + Seek + Send
  + 'static)`, `read_owned`; `Archive::members()`; and
  `extract_with_control(options, |member| Decision::Extract(Box<dyn Write>)
  | Skip | Stop)`, a push-style callback that suits our "stream into a
  `Backend` sink" model. Options include `with_password`,
  `with_max_member_output_bytes`, `with_max_total_output_bytes`,
  `rar50_dictionary_size_limit`, `with_max_header_count`,
  `with_max_header_bytes`, and a `ReadCancellation` token. These are the
  zip-bomb and cancel controls we need, ready made. `entry_relative_path`
  rejects absolute, `..`, drive and NUL names, treats `\` as a separator
  (RAR 3/4 archives made on Windows store `testdir\test.txt`; the fixture
  showed this) and preserves non-UTF-8 bytes.
- Gaps seen: a RAR symlink in a RAR 4 archive (`testlink`) arrives as a
  small ordinary-looking file whose content is the target; only the unix
  mode in `file_attr` marks it, so the extractor must check it. A RAR 5
  hard link ("redirection") makes `extract_with_control` return an error
  ("extracting redirections through extraction control is not supported"),
  so such archives need `Skip` handling or the lower-level API. Solid
  members cannot be skipped (`Decision::Skip` errors; extract them to
  `io::sink()` instead), which matches what a solid archive is.
- Seek: reads from a `Read + Seek + Send + 'static` source or a file path,
  so a non-`LOCAL_PATH` backend must download first. The `'static` bound
  means the reader owns the source (a `File`, or a `Cursor`).
- Writing exists (RAR 1.3 to 7.0, solid, volumes, encryption, recovery
  records) but is **not** part of the plan; see the licence note and the
  maintainer decision. Leave the `write` feature off.

**Licence constraint on reimplementing RAR, accurately.** RARLAB's UnRAR
source licence (`license.txt` in UnRAR 7.1.8, downloaded from rarlab.com on
2026-11-20) allows the UnRAR source to be used "in any software to handle
RAR archives without limitations free of charge", but says it "cannot be
used to develop RAR (WinRAR) compatible archiver and to re-create RAR
compression algorithm, which is proprietary", and that distributing
*modified* UnRAR source requires the full paragraph in the licence or
documentation and in source comments. Consequences:

- Linking the real UnRAR (what `unrar`/`unrar-rs` do) is allowed for a
  reader but is C++ code, so the project's pure-Rust rule forbids it.
- A port of the UnRAR source into Rust, even decode-only, is a modified
  copy: it keeps the notice duty and the non-standard terms, and the same
  UnRAR-derived code in 7-Zip is under "GNU LGPL with unRAR license
  restriction". A port is the wrong path for a project that is MIT OR
  Apache-2.0.
- The clean-room path is writing from format specifications and observed
  behaviour, never from UnRAR source. Specifications: the RAR 5 format
  is documented publicly by RARLAB (technical note; not re-fetched here)
  and RAR 1.5 to 4.x by RARLAB's older `technote.txt`; the compression
  algorithms themselves are not published, so a clean-room decoder
  depends on reverse engineering or on third-party descriptions (PPMd is
  public; the LZ and Huffman layouts, the filters and the VM are described
  by independent readers such as libarchive's BSD-2 code and The
  Unarchiver).
- Writing RAR is the part the licence is most pointed about. `rars` writes
  RAR; it does not use UnRAR source (per its own specification repository,
  which also says it reverse-engineered RAR/WinRAR binaries). This survey
  does not judge that. We sidestep it by not enabling `write`.
- This is an engineering reading of the quoted text, not legal advice.

### Other formats, one line each

| Format | Verdict |
| ------ | ------- |
| lz4 | `lz4_flex` 0.14.2 (released 2026-11-15, MIT, 623 stars): frame format read and write, safe code by default. `tar.lz4` written and read; `lz4 -t` accepted the output. Streaming. Offer read; write is cheap but rarely wanted. |
| lzip | `lzma-rust2` `LzipReader` and `LzipWriter`. Not fixture-tested (no `lzip` binary on the survey machine). Read: yes, as an obscure extra; the old `lzip` 0.1.1 crate (2022) is a C binding. |
| cab | `cab` 0.6.0 (released 2024-04-05, MIT, 23 stars): read and write; none, MSZIP and LZX (via `lzxd`), pure tree. No Quantum. Not fixture-tested (no `cabextract` or `gcab`). Read-only offer. Lightly maintained. |
| ar, `.deb` | `ar` 0.9.0 (2021, MIT, last commit 2024): read worked on a `bsdtar`-written archive. A `.deb` is `ar` holding `data.tar.{gz,xz,zst}`: opens through the same tar stack. Read only. |
| cpio | `cpio` 0.4.2 (released 2026-11-01, MIT, 14 stars): `NewcReader` read a `cpio -H newc` archive correctly (names, modes, CRCs); the crate describes itself as an archive creator, and `newc` writing exists. Only the `newc` and `crc` variants, not the old binary or odc formats. Read-only offer. |
| rpm | Not surveyed. An RPM is a lead, a header, then a compressed cpio payload; it needs a header parser (the `rpm` crate was not evaluated). Skip for v1. |
| iso9660 | `iso9660` 0.1.1 is "work in progress", last released 2023-04-02. No Joliet or Rock Ridge write-up, no UDF. Do not build on it. A disc image is better opened by mounting it through udisks (a loop device), which the sidebar already understands as a mount. Out of scope. |
| Brotli, Snappy, others | Not archives people open. Skipped. |

## Cross-cutting findings

### Seek and streaming

| Format family | Needs `Seek`? | Streams from `Backend::read`? |
| ------------- | ------------- | ----------------------------- |
| tar, tar.gz, .bz2, .xz, .zst, .lz4, plain `.gz`/`.xz` | No | **Yes** |
| zip | Yes (central directory at the end) | No, download first |
| 7z | Yes (header at the end) | No, download first |
| rar | Yes (`rars` takes `Read + Seek + Send + 'static`) | No, download first |
| ar, cpio, cab | cab: yes; ar and cpio: no | ar, cpio: yes |

`Backend::read` takes a `Location` and returns a forward-only chunk stream;
there is no offset or range. A backend without `LOCAL_PATH` (SFTP, and the
future SMB and MTP) cannot serve random access, so for zip, 7z and rar the
archive must first be downloaded to a temporary file, the same read-only
caveat path #15 already built for "open". A local archive is opened
directly (`modules/archive.rs` sits under `src/modules/`, where `std::fs`
is allowed).

### Async and blocking

Every codec above is blocking. The app has one tokio runtime (iced's), so
all archive work runs under `tokio::task::spawn_blocking`, bridged to the
ops engine with bounded `futures::channel::mpsc` channels and `try_send`,
the pattern `modules/local.rs` already uses for `WriterSink`. For tar
streams, the blocking thread reads the source through an adapter implementing
`std::io::Read` over a `ReadStream`, calling
`tokio::runtime::Handle::block_on(stream.next())` for each chunk; that is
legal inside `spawn_blocking`. `Sink::close` for any archive writer must obey
the durability contract in `vfs.rs`: do not resolve until the data is on
disk.

Treat a panic in a codec as damaged input, not as a crash: await the
`JoinHandle`, and turn `JoinError::is_panic()` into a worded error ("This
archive could not be read safely"). It is a second line of defence behind
the no-panic rule, which the crates themselves cannot promise.

### Memory

Streaming codecs and `tar` hold constant memory plus their window. What
grows is index data (zip central directory, 7z and RAR headers, and every
`tar.*` browse, below) and decoder dictionaries. The limits the crates
offer, all of which the tasks must set explicitly:

| Source | Control |
| ------ | ------- |
| xz | `XzReader::new_mem_limit` |
| zstd | `StreamingDecoder::new_with_max_window_size` |
| 7z | `ArchiveReader::new_with_memory_limit` |
| rar | `rar50_dictionary_size_limit`, `with_max_member_output_bytes`, `with_max_total_output_bytes`, header limits |
| zip, tar, gzip, bzip2 | none needed for the codec; apply the output budget in our own copy loop |

### Names and OsString discipline

Archive names are bytes. Reader results, at a glance: tar and zip give raw
bytes (`path_bytes`, `name_raw`); rar gives raw bytes; 7z gives a UTF-16
derived `String`. The extractor holds names as `Vec<u8>` and converts with
`OsStr::from_bytes` at the last step, using `to_string_lossy` only for
display (the worded progress text). The one lossy format is 7z, and only for
names with unpaired surrogates.

### Permissions and times

| Format | Mode on read | Mtime on read | Mode on write | Mtime on write |
| ------ | ------------ | ------------- | ------------- | -------------- |
| tar | yes (12 bits) | Unix seconds | yes | yes |
| zip | `unix_mode()` if made on Unix | DOS local time, or extended timestamp | yes | set explicitly, else "now" |
| 7z | high 16 bits of attributes when bit `0x8000` is set | NT time | **dropped** | writer sets it |
| rar | `file_attr` plus host OS | stored (DOS local or Unix) via `modification_time()` | not offered | not offered |

Mask every mode to `0o777` on extraction: tar delivered `4755` and the
reference tools would apply it. Do not restore owner or group.

### Path traversal, symlinks and absolute names: evidence

All run on 2026-11-20 against scratch directories. `x` is the entry name.

| Case | `tar` crate `unpack_in` | `zip` crate `enclosed_name` | `rars` `entry_relative_path` |
| ---- | ----------------------- | --------------------------- | ---------------------------- |
| `../x` | refused (`Ok(false)`) | `None` | rejects |
| `a/../../x` | refused | `None` | rejects |
| `/abs/x` | re-rooted under dest | leading `/` stripped | rejects |
| `C:\x` | not applicable | drive stripped | rejects |
| `dir\..\..\x` | one odd file name | `None` | `\` is a separator, rejects |
| NUL inside | not tried | cut at the NUL | rejects |
| symlink to an outside dir, then `sym/x` | link created; write through it refused | not extracted by the crate here (we have no `extract`); `is_symlink` flags it | n/a |
| hard link to an outside file | refused | n/a | RAR "redirection": surfaced as an error |
| setuid mode, device node | passed through as entries | n/a | n/a |
| duplicate names | both entries delivered | `has_duplicate_names()` true | not tried |

The lesson: none of the crates is a complete guard. Their helpers are
useful cross-checks; the guard is ours.

## Security requirements for extraction

For #66 to meet, each with a test (see the notes for #66). Written as
rules the task must satisfy:

1. **One shared, byte-based name guard** in `core/` (iced-free), used by
   every format: split on `/` and, for zip and RAR, on `\`; reject an empty
   name, a NUL byte, an absolute path, a drive prefix, any `..` component
   after splitting; drop `.` components; refuse components over 255 bytes
   and joined paths over 4,096 bytes. A refused entry is reported as skipped
   with its name, never silently rewritten and never extracted elsewhere.
   (The crates' helpers strip or re-root instead; do not copy that.)
2. **Never trust the filesystem under the destination.** Before writing an
   entry, every parent component must be a real directory created by this
   operation or already present and not a symlink (check with the
   non-following `metadata` the backends provide, `FileEntry::is_symlink`).
   Opening for write must not follow a symlink at the final component
   either; remove or refuse instead (the ops engine's `write` through a
   symlink dest follows it today; confirm and fix in the same PR).
3. **Symlink and hard-link entries are not created in the first release.**
   The `Backend` trait has no such call, and creating one safely means
   resolving its target (rule 2 on the target). List them as skipped, with
   a count in the summary. If the maintainer approves a trait change
   (see [Backend gaps](#backend-gaps)), allow only relative targets that
   stay inside the destination after normalisation, and never write
   through one.
4. **Special files are skipped**: character and block devices, FIFOs,
   sockets, and anything not a regular file or directory.
5. **Modes are masked** to `0o777` (no setuid, setgid or sticky), owner and
   group are never applied. Directories get at least `0o700` while being
   filled so a read-only directory entry in the archive does not block its
   own contents.
6. **Budgets**, because declared sizes lie: a total-bytes ceiling (default:
   the free space on the destination minus a margin, and not more than a
   configurable cap), an entry-count ceiling, and a per-entry ceiling, all
   counted from the bytes actually produced by the decoder, not from the
   header. Abort with a worded error when exceeded. Also pass the library
   limits in the table under Memory. A compression ratio above, say,
   1,000:1 with a large output is worth an "is this a zip bomb?" prompt
   rather than a silent run.
7. **Cancel and failure leave a consistent state.** Write each file to a
   hidden temporary name in the destination and rename on success; on
   cancel or failure remove the temporaries and any directories this
   operation created and nothing else. Record created top-level items for
   undo (trash them).
8. **Conflicts use the existing engine prompt.** Two entries that map to
   the same destination path (duplicate names, or a file and a directory
   with one name) are a conflict, never a silent overwrite.
9. **Passwords**: asked for through the capacity-1 reply channel pattern;
   held in a zeroizing buffer; dropped after use; never logged or put in
   an error message. A wrong password is a typed error with no partial
   output left behind, offered again at most a few times.
10. **Codec isolation**: every decode runs under `spawn_blocking`; a panic is
    reported as a damaged archive (see Async and blocking).
11. **Timestamps** are clamped to a sane range (reject or omit anything
    before 1970 or beyond year 9999) before conversion to `SystemTime`.
12. **Detect the format by magic bytes**, with the extension only as a
    hint, so a `.zip` that is really a tar is not handed to the wrong
    decoder; and refuse nested archives in the browse backend in v1.

## Backend gaps

Verified by reading `src/core/vfs.rs`, `src/core/fs/ops.rs` and
`src/modules/mod.rs` on 2026-11-20:

- `Backend` has nine methods: `list`, `metadata`, `read`, `write`, `mkdir`,
  `rename`, `remove`, `set_times`, `watch`. There is **no** call to create
  a symlink or hard link, and **none** to set permissions
  (`Caps::SET_PERMISSIONS` is reserved but "no backend sets this bit yet";
  `FileEntry::mode` is read only).
- #66's acceptance says "extract with correct names and modes". With the
  trait as it is, extracted files come out with the destination's default
  mode, so an extracted script is not executable. Meeting the criterion
  needs a new trait method, e.g. `set_permissions(&self, &Location, u32)`
  with `Caps::SET_PERMISSIONS`, implemented by `modules/local.rs` first
  (SFTP can follow; it has `setstat`). Adding a method to `Backend` takes
  the `lock:backend-trait` label and a change to every backend and to
  `FakeBackend`. Symlink creation would be a second method. **Update from the
  coordinator:** issue #16 (in progress, holds `lock:backend-trait`) is adding
  `set_permissions` and symlink creation to `Backend`, so #66 should depend on
  #16 rather than request a separate trait change.
- `ops.rs` knows two `OpKind`s, `Copy` and `Move`, and copies a tree from
  one backend to another. Extraction and compression are new kinds with
  their own walker (reading entries from the codec, not from a
  `Backend::list`), but they should reuse the engine's event stream,
  conflict prompt, cancel flag and `Started`/`Progress`/`Finished` events.
- `modules::resolve` builds a stateless backend per call for `file` and
  asks `RemoteManager` for pooled remote ones. An `archive://` backend needs
  the opened archive (its entry index) to persist across calls, so it needs
  a session cache like `RemoteManager`, keyed by archive location and
  invalidated by the archive's mtime and size.

## Notes for the follow-up tasks

### #66 Extract archives here or into a chosen folder

- Formats: read zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst, tar.lz4, 7z, rar
  (after the spike), plain `.gz` `.bz2` `.xz` `.zst` (a single file),
  `.deb`, cpio. Unsupported and worded: zip entries with zstd or other
  unsupported methods ("This zip uses a compression method that can't be
  opened yet"), RAR if the spike failed, 7z entries with unsupported
  codecs, multi-volume sets, iso, rpm.
- Do not model extraction as "copy out of the `archive://` backend"
  (#68). Entries of a `tar.*`, a solid 7z and a solid RAR must be read in
  one sequential pass; reading each entry by name would decode the stream
  from the start every time, quadratic in archive size. A single walker
  pass is the design; #68's single-entry copy is the exception, and must
  warn.
- The pipeline per format family: open (magic sniff) then iterate entries,
  calling a shared `Sink` callback with `(name bytes, kind, size hint,
  mode, mtime)` and a reader; the callback applies the name guard, the
  parent-directory rule, the conflict prompt and the budget, then writes
  through the destination `Backend`. A first pass to count entries and
  total size is cheap for zip, 7z and rar (headers) but costs a full
  decode for `tar.*`; use the compressed size from the source as the
  progress denominator for those instead of a pre-scan.
- The "folder named after the archive when more than one top-level entry"
  rule needs the set of top-level names before extraction starts. For zip,
  7z, rar this is the central directory. For `tar.*` it means a first
  decode of the stream (or extract into a temporary sibling folder and
  rename into place at the end). Prefer the temporary-folder approach; it
  also gives the consistent-on-cancel behaviour for free. Strip a lone
  `./` prefix when counting.
- `zip`: use `name_raw()`; check `encrypted()` through raw access, then
  prompt; set `has_duplicate_names()` to a conflict path; honour the
  extended-timestamp field before the DOS time; skip entries where
  `is_symlink()`.
- `7z`: `for_each_entries` already yields a reader per entry in archive
  order and streams; a solid block is decoded once. Keep the password in
  `Password`. Clamp NT times. Read the mode from the attribute high bits.
- `rar`: use `extract_with_control` with `Decision::Skip` for refused
  entries except in a solid archive (use `io::sink()` there), the memory
  and output limits above, and `entry_relative_path` as a cross-check of
  our own guard. Detect symlinks by mode in `file_attr`.
- Unit tests required beyond the issue's list: the guard (every row of the
  evidence table), the top-level folder rule, a bomb fixture (small
  archive, huge declared and real output; budget stops it), a duplicate
  name fixture, a symlink-then-write fixture (nothing outside the
  destination changes), a cancel mid-entry fixture (no temp files left),
  the setuid fixture (extracted mode has no `4000`), and a non-UTF-8 name
  fixture (`caf\xe9.txt` extracts byte for byte).
- Fixtures: generate small archives in the test with the crates' own
  writers where they are faithful (tar, zip deflate, 7z) and check in
  small hostile ones (under 10 kB each) made with the Python scripts used
  here; the third-party corpora named in this document are the larger
  reference set, to be fetched by an `#[ignore]` test, not committed.
- Needs the `lock:backend-trait` change for modes (see Backend gaps). Ask
  the maintainer whether to do it in #66 or defer modes.

### #67 Compress files into an archive

- Offer, in this order: `zip` (deflate; level 1 to 9), `tar.gz` (level 1 to
  9), `tar.xz` (preset 0 to 9), `tar.bz2` (1 to 9), plain `tar`. Not
  `tar.zst` in v1 (ratio worse than gzip; needs the guard from the zstd brief). Do
  not offer 7z or RAR in v1 (7z writer drops modes and symlinks and
  corrupts non-UTF-8 names silently; RAR is a policy decision). Do not offer
  encryption (out of scope in the issue; AES zip writing exists in the crate
  but was not cross-checked).
- zip writing only takes UTF-8 names and `String` link targets: a selection
  containing a non-UTF-8 name must be refused (or those items skipped) with
  a worded message, never lossy-converted. tar has no such limit; prefer
  `tar.gz` as the first offer for such selections.
- Set `last_modified_time` and `unix_permissions` per zip entry from the
  source `FileEntry`; the crate's default is the current time. For tar the
  header carries mode and mtime from the source. Use `with_extended_timestamp`
  on zip so mtimes survive time zones.
- zip is written to a seekable sink: write to a local temporary file next to
  the destination, then move into place. The zip writer needs `Write +
  Seek`, which `WriteSink` is not. tar and its compressors stream, so they
  can write straight into a `WriteSink` through a blocking bridge.
- Cancel and failure: remove the partial archive (the `Sink::close`
  durability rule applies: the archive must be complete on disk before the
  op reports finished).
- Large inputs: stream file contents; never `read_to_end` a source file
  (the probe did, for brevity). A zip entry over 4 GiB needs
  `large_file(true)`; call `set_auto_large_file` or set it for any file with
  `size >= 4 GiB`.
- Symlinks in the selection: `tar` stores them; zip has `add_symlink`.
  Default to storing them as links (not following), and say so. Never
  follow a link out of the selection.
- Test: round trip with the #66 extractor on a fixture tree (names, modes
  that the trait can express, mtimes, empty directories, an empty file, a
  file over 4 GiB marked `#[ignore]`), and cross-check with the reference
  tools in an `#[ignore]`d test (`unzip -t`, `tar -tzf`, `xz -t`).

### #68 Browse archives as read-only folders

- `archive://` location: scheme `archive`; keep the outer archive's
  `Location` in the authority (percent-encoded) and the inner path in
  `path`. Round-tripping through `Location::parse` and `Display` must be
  tested with an outer path containing `/`, `?`, `#`, `%` and a space.
  Refuse an archive inside an archive in v1.
- Caps: none of `WATCH`, `TRASH`, `LOCAL_PATH` (as the issue says); also
  not `RENAME_IN_PLACE` and not `THUMBNAILS` initially. Every write method
  returns a typed `VfsError` with worded text ("Archives are read-only
  here"). The trait has no read-only variant; `VfsError::Other` with the
  message is what exists; a new `ReadOnly` variant is a small, additive
  change if the maintainer wants it.
- Index per archive, built once, cached in a session map keyed by (archive
  location, size, mtime) and dropped when the user leaves the archive:
  - zip, 7z, rar: from headers; fast, memory proportional to entry count.
  - `tar.*`: **requires decoding the whole compressed stream once** to
    list it. For a multi-gigabyte `tar.gz` this is slow and must show
    progress as a worded "Reading the archive" state, with cancel, not
    freeze the folder view. Do not rebuild the index per `list` call.
    Consider a size threshold above which browsing a `tar.*` is declined
    with a suggestion to extract.
- `list` for an inner directory derives children from the flat entry list
  (archives may omit directory entries; synthesise them). `metadata` and
  `list` must return `FileEntry` with `kind`, `size`, `modified` and `mode`
  (read only).
- `read` of one inner file: zip, and 7z and rar non-solid, are random
  access. A `tar.*`, a solid 7z and a solid RAR must decode from the start
  (or from the start of the solid block) to reach the file, which can cost
  the whole archive; cache nothing, but word the wait ("Unpacking from the
  start of the archive") and honour cancel. Opening an inner file uses
  #15's download-to-temp path.
- Remote archives (no `LOCAL_PATH` on the outer backend): download the
  archive to a cache file once per session first, show worded progress, and
  clean up on leaving.
- Encrypted archives: browse needs a password prompt at open time for
  header-encrypted RAR and 7z, and at read time for zip and file-encrypted
  entries. Show locked entries as such (capability-honest wording).
- Seam note: the codec traits for #66 and #68 should be one module,
  `core/archive/`, with `modules/archive.rs` as the thin `Backend` adapter,
  so extraction and browsing share the guard, the format sniffer and the
  per-format readers. `core/` stays iced-free; the crates are not iced
  types, so the hard import rule holds.
- `--no-default-features` must still build: the `archive` feature owns all
  the codec dependencies and `modules/archive.rs`.

## Acceptance spikes

Short, throwaway, local, no production data. Each result recorded in the
task's PR as words.

### `rars` spike (before #66 or #68 ship RAR)

1. Compiles with the saola-files toolchain and `default-features = false,
   features = ["encryption"]`; `cargo tree` shows no `cc`, `-sys` or `pkg-config`.
2. Reads real archives from genuine WinRAR/`rar` at several levels and
   dictionary sizes (maintainer supplies a handful, including a RAR 7 one,
   one solid, one with a recovery record, one with encrypted headers, one
   split into volumes), content hash equal to the originals.
3. Hostile corpus: the libarchive fuzz-regression files and a few
   hand-made truncations and bit flips; no panic, no hang over five seconds,
   memory under the set limits.
4. A 1 GiB-class archive extracts with flat memory (peak resident under
   the limits set) and cancel returns within a second.
5. Ten minutes of `cargo-fuzz` on the open and extract entry points, no
   crash.
6. Record the maintainer's decision on the legal note (below).

If steps 2 to 5 fail on a defect that upstream will not fix, try `rar-codec`
as the reader; failing that, open the contingency brief.

### `ruzstd` write guard spike (only if `tar.zst` is offered in #67)

Prove the infallible-adapter pattern from the zstd brief: sink error,
source error and cancel all produce a worded error, no panic and no
partial file left.

## Measurements

**Large-file speed and memory were not measured.** A 195 MB benchmark
(peak resident memory and wall time against the reference tools) was started
but not completed, so claims about memory on large archives rest on the
crates' documented limits and on the small runs below, not on measurement.
The writer probe on a 7 MB tree showed peak resident memory of 11 MiB for
gzip, bzip2, zstd, lz4 and zip, and about 80 MiB for xz preset 6 and 7z
(LZMA2).

Indicative only: taken on 2026-11-20 on a busy machine, one run each, 7 MB
fixtures, release build:

- bzip2 decode, pure Rust: 1.85 s; native `bzip2 -d`: about 0.6 s CPU.
- 7z PPMd decode, `sevenz-rust2`: 13 s; native `bsdtar`: about 3 s CPU.
- xz, zstd, gzip, lz4 decodes: tens of milliseconds, same order as native.

zstd encoder size comparison on a 2.6 MB tar of Rust source: `ruzstd`
`Fastest` 802,079 bytes; `zstd -1` 553,105; `zstd -3` 498,671; `gzip -6`
509,888.

## Needs the maintainer's decision

1. **Accept `rars` as the RAR reader** (young, agent-assisted, single
   author, Apache-2.0, reverse-engineering provenance of some format
   knowledge) behind the spike, or hold RAR for the later of: a second
   reader maturing, or the brief. Recommendation: accept behind the spike,
   read-only.
2. **RAR writing**: not offered. Confirm. (If wanted later, the licence
   reading above should get a lawyer's look first.)
3. **Zip name encodings.** Archives made on Windows or in East Asian
   locales without the UTF-8 flag store names in an unknown code page
   (CP437, GBK, Shift-JIS). Options: (a) write the raw bytes as the file
   name, as Info-ZIP's `unzip` does on Linux (round-trips, shows as mojibake
   in a UTF-8 file manager); (b) decode as CP437 when the flag is off (what
   the crate's `name()` does; lossless but wrong for East Asian archives);
   (c) offer a "filename encoding" choice in the extract dialog (needs a
   charset crate and a UI decision). Recommendation: (a) for v1, record (c)
   as a later task.
4. **Backend trait change** for permissions and symlinks: being done by
   #16 (holds `lock:backend-trait`); make #66 depend on #16.
5. **7z creation**: do not offer in v1 (writer drops modes and symlinks).
   Confirm, or ask for an upstream patch first.
6. **Browse size limits** for `tar.*` (a threshold, and what to say), and
   whether browsing a remote archive may download it to the cache.
7. **zstd**: offer `tar.zst` at level 1 behind the guard, or wait for an
   upstream encoder. Recommendation: do not offer `tar.zst` in v1 (the ratio is worse
   than gzip, see zstd); revisit when `ruzstd` ships levels.

## Risks and the reversal rule

- Every chosen crate other than `tar`, `flate2` and `zip` is under three
  years old or has one main maintainer; `rars` and `sevenz-rust2` and
  `lzma-rust2` break their API between minor releases. Exact pins, and bump
  deliberately with the fixtures above rerun.
- Parser bugs in archive crates are a security surface (the `zip` crate has
  published advisories on path handling). Run `cargo audit` (or
  `cargo deny`) in CI for these crates; not checked in this survey.
- If a spike fails on a defect upstream will not fix, patch upstream or
  open the project from the matching brief in its own repository.
- Re-run this survey when a pin is bumped past a minor version, when
  `libzstd-rs-sys` has a release with no `cc` step, and when `ruzstd` ships
  encoder levels.

## Sizes, for the record

If a crate has to be written (see the briefs): a read-only RAR decoder is
about 8,000 to 12,000 lines and three months for one engineer (the existing
`rar-codec` is 7,000 lines); a zstd encoder with real levels, 3,000 to 7,000
lines and two to four months (the upstream route is much cheaper). For
scale, the chosen crates are `zip` 27,000 lines, `lzma-rust2` 21,000,
`ruzstd` 12,600, `sevenz-rust2` 9,500, `tar` 5,300, `rars` 131,000 (with
writers and recovery).
