# Brief: a read-only RAR decoder crate (contingency)

Date: 2026-11-20. Status: **contingency**. Nothing is blocked on this brief
today. It activates only if the `rars` acceptance spike in
[`docs/decisions/archive-codecs.md`](../decisions/archive-codecs.md) fails.
Written for the follow-up tasks #66 (extract) and #68 (browse).

## Why this brief exists

The backlog assumed that no pure-Rust RAR decoder exists. On 2026-11-20 one
does: `rars` 0.13.0 (Apache-2.0, 100 stars, released 2026-11-17) reads and
writes RAR 1.3 through RAR 7, has `unsafe_code = "forbid"`, and a pure-Rust
dependency tree. It is six months old, was written with heavy agent
assistance (its README says so), and has one main author. That is a real
risk for a format with decades of odd archives in the wild, so the
decision records a spike and keeps this brief as the fallback.

Two other pure-Rust candidates were checked and rejected as the primary:
`rar-codec` 0.1.11 (released 2026-11-15, 3 weeks old, no encryption support,
lives inside an unrelated search tool's repository) and `rar` 0.4.0 (RAR5
stored entries only; RAR4 and all compressed RAR5 are unchecked boxes in its
own README). `unrar` 0.5.8 and `unrar-rs` 0.16.6 are disqualified: they
compile the C++ UnRAR source (`unrar-rs` also pulls `aws-lc-sys`).

## Scope

A pure-Rust crate, read only, in its own repository outside the Saola
project. Staged so each stage ships and is useful alone:

1. **Container and stored entries.** RAR 5 and RAR 3/4 header parsing,
   listing (name bytes, sizes, mtime, attributes, host OS, flags), stored
   (method 0) extraction with CRC32 and BLAKE2sp checks, symlink and hard
   link records exposed as data (not acted on), clean errors on truncated or
   hostile headers.
2. **RAR 5 compression.** The LZ77 plus Huffman decoder (format versions 0
   and 1, the second from RAR 7), solid archives, the E8/E8E9, ARM and delta
   filters, dictionary sizes up to a caller-set cap.
3. **RAR 3/4 compression.** LZ plus Huffman, PPMd variant H, solid archives,
   and the RAR VM filters. Recognise the standard filter programs by
   fingerprint (E8, E8E9, delta, RGB, audio) as other readers do, and return
   `Unsupported` for any other VM program rather than implement a VM.
4. **Encryption.** RAR 5 AES-256 (PBKDF2-HMAC-SHA256, password check value)
   and RAR 3/4 AES-128 (SHA-1 key derivation), file data and, in a later
   step, encrypted headers.
5. **Multi-volume sets** (`.part1.rar`, `.r00`). Optional; last.

May leave out: writing, recovery records and repair, SFX stubs, RAR 1.3 to
2.x legacy codecs, comments, and anything needing a C library.

## API

The consuming module is `modules/archive.rs` (#68) plus the extraction task
(#66). Both run the decoder inside `spawn_blocking`. The crate must be
synchronous, `Send`, and must never panic on input.

```rust
/// Parses headers only. `R` is a seekable source (a local file or a
/// downloaded temp file). Does no decompression.
pub fn open<R: Read + Seek + Send>(
    source: R,
    options: &OpenOptions,
) -> Result<Archive<R>, Error>;

pub struct OpenOptions {
    pub max_header_bytes: u64,        // refuse absurd headers
    pub max_entries: u64,
    pub max_dictionary_bytes: u64,    // memory ceiling for the LZ window
}

impl<R> Archive<R> {
    pub fn format(&self) -> Format;               // Rar4 | Rar5
    pub fn is_solid(&self) -> bool;
    pub fn is_header_encrypted(&self) -> bool;
    pub fn entries(&self) -> &[Entry];
    /// Streams one entry. For solid archives this may decode earlier
    /// entries and discard them; `Entry::solid_cost` says how many bytes.
    pub fn read_entry(
        &mut self,
        index: usize,
        password: Option<&[u8]>,
        cancel: &AtomicBool,
    ) -> Result<EntryReader<'_, R>, Error>;
}

pub struct Entry {
    pub name: Vec<u8>,          // raw bytes, '\\' kept as stored
    pub kind: EntryKind,        // File | Dir | Symlink { target: Vec<u8> } | HardLink { .. }
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub unix_mode: Option<u32>, // only when the host OS recorded one
    pub encrypted: bool,
    pub crc32: Option<u32>,
}

pub enum Error {
    NeedPassword, WrongPassword, Truncated, Corrupt(String),
    Unsupported(Feature), LimitExceeded(Limit), Cancelled, Io(io::Error),
}
```

`EntryReader` implements `Read`, verifies the stored checksum at end of
entry (a mismatch is `Corrupt`, never silent), and stops within one block of
`cancel` being set. Names are bytes, not `String` (OsString discipline).

## Test strategy

- **Reference corpus.** The libarchive test suite carries RAR 3, RAR 4 and
  RAR 5 files (BSD-2-Clause) including solid, encrypted, multi-volume, ARM
  filter and BLAKE2 cases, plus about two dozen small files that came from
  fuzzers (`bad_tables`, `distance_overflow`, `leftshift`, `loop_bug`,
  `readtables_overflow`, `invalid_dict_reference`, and others). They are
  stored uuencoded in `libarchive/test/test_read_format_rar*.rar.uu`. Decode
  them into the crate's fixtures with their provenance noted. Each file's
  expected listing and content hash comes from libarchive's own test
  assertions.
- **Differential check.** An `#[ignore]` test that runs the real `unrar` (if
  installed) and `bsdtar` over every fixture and compares name, size and
  CRC32 per entry. Never required in CI.
- **Fuzzing.** `cargo-fuzz` targets for `open` and `read_entry` seeded from
  the corpus. Acceptance: one hour of fuzzing with no panic, no hang over
  five seconds, no allocation above `max_dictionary_bytes` plus a fixed
  slack.
- **Hostile-input unit tests.** Huge declared sizes, header counts, name
  lengths; a dictionary field above the cap; a filter that writes past the
  window; a truncated final block.
- **Real-world sample.** A few RAR files made by the genuine WinRAR/`rar`
  at each of levels 1, 3 and 5 (maintainer supplies, or generated locally
  where `rar` is installed), kept out of the repository if licence unclear.

## Licence notes

- Format documents: the RAR 5 archive format is described in RARLAB's public
  technical note (not re-fetched in this survey; verify before relying on
  it). The RAR 3/4 format is documented by third parties; the RAR
  compression algorithms are **not** publicly specified by RARLAB.
- UnRAR source licence, quoted from `unrar/license.txt` in the UnRAR 7.1.8
  source archive downloaded from rarlab.com on 2026-11-20: UnRAR source code
  "may be used in any software to handle RAR archives without limitations
  free of charge, but cannot be used to develop RAR (WinRAR) compatible
  archiver and to re-create RAR compression algorithm, which is
  proprietary." Redistribution of modified UnRAR source must carry the full
  paragraph starting at "UnRAR source code".
- What follows for this crate: copying or translating UnRAR source is
  allowed only for a decoder, and carries the notice duty, which would make
  the crate's licence non-standard (not MIT or Apache-2.0 clean). So do not
  read or port UnRAR source. Work from the RAR 5 technote, from the
  documented behaviour of other independent decoders, and from the fixtures,
  and record in a `PROVENANCE.md` which sources each module used. The
  decoder-only scope also stays on the permitted side of the "RAR
  compatible archiver" clause. This is an engineering reading of the quoted
  text, not legal advice; the maintainer should decide whether a lawyer's
  view is needed before publishing.
- Provenance claims of the existing crates, read on 2026-11-20 and not
  verified. `rars` has a companion specification repository
  (`bitplane/rar-research`) that calls itself clean-room and says the UnRAR
  source and 7-Zip's `Rar*` decoders are off limits (7-Zip's own licence
  puts those files under "GNU LGPL with unRAR license restriction"). The
  same text says the author also studied old RAR and WinRAR binaries by
  reverse engineering; whether that is acceptable under those binaries'
  own licences is a legal question this survey does not answer. `rar-codec`
  says it began as a port of libarchive's BSD-2 RAR reader and was then
  rewritten.
- Licence of this crate: Apache-2.0 or MIT OR Apache-2.0, to match the
  Saola ecosystem. Use `aes`, `sha1`, `sha2`, `pbkdf2`, `blake2` from
  RustCrypto rather than writing primitives.

## Honest size estimate

For comparison: `rar-codec` is 7,000 lines (decode only, no encryption) and
reached its present state in about three weeks of history; `rars` is 131,000
lines of source plus 36,000 of tests, but includes writers, recovery and
bindings.

- Stages 1 to 2 (container, stored, RAR 5 decode): 3,000 to 5,000 lines,
  three to five weeks.
- Stage 3 (RAR 3/4 with PPMd and filters): 3,000 to 5,000 lines, four to
  six weeks. PPMd var.H alone is about 1,500 lines (`ppmd-turbo` 0.2.2 and
  `ppmd-rust` 1.5.2 are pure-Rust PPMd crates that could be reused after a
  licence and purity check).
- Stage 4 (encryption): 800 to 1,200 lines, one to two weeks.
- Fuzzing and corpus work: two weeks.

Total: roughly 8,000 to 12,000 lines and three months for one engineer. Not
worth starting unless the `rars` spike fails and `rar-codec` has not caught
up.
