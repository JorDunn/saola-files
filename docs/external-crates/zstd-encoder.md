# Brief: a pure-Rust zstd encoder with real levels (contingency)

Date: 2026-11-20. Status: **contingency, low priority**. Nothing is blocked.
Decoding zstd is covered by `ruzstd`; a first encoder exists too but is
level-1 only and can panic. This brief records what a better encoder would
need, and says to try an upstream contribution to `ruzstd` before starting a
new crate. Written for #67 (compress). See
[`docs/decisions/archive-codecs.md`](../decisions/archive-codecs.md).

## What exists today

`ruzstd` 0.9.1 (MIT, released 2026-10-11, 460 stars, last commit
2026-11-17, one main author):

- Decoder: complete enough to read every `.tar.zst` made by the reference
  `zstd` tool in the survey, including `--ultra -22 --long=27` (see the
  decision for the measurements). Streaming (`StreamingDecoder`) with
  `new_with_max_window_size`.
- Encoder: `ruzstd::encoding::compress(source, sink, level)` and
  `FrameCompressor`. `CompressionLevel::Uncompressed` and `Fastest`
  (about zstd level 1) work. `Default`, `Better` and `Best` are marked
  `UNIMPLEMENTED` in the source and, run on 2026-11-20, hit
  `not implemented` and **panic**.
- The same `compress` call returns `()` and `unwrap()`s every read and
  write inside, so a full disk or a closed pipe **panics** instead of
  returning an error (run on 2026-11-20: `called Result::unwrap() on an
  Err value: ... "disk full"`). This breaks the no-panic rule on a normal
  runtime path.
- Output validity: a `tar.zst` written at `Fastest` passed `zstd -t` and
  extracted identically through the reference `zstd` and `tar`.

The C-backed `zstd` crate is disqualified: its default build compiles
`zstd-sys` with `cc`. Its `rust-backend` feature swaps in
`libzstd-rs-sys` 0.0.1-prerelease.2 (BSD-3-Clause, released 2026-06-01),
a machine translation of the C library into Rust. That crate's `build.rs`
still calls `cc` to assemble `huf_decompress_amd64.S` on x86-64, and the
version is a prerelease whose own README calls the encoder "mostly just the
raw c2rust translation and hence full of unsafe code". Revisit it when it
has a release with no `cc` build step.

## Do we need this

For the first release, no. #67 requires zip and tar.gz. A `tar.zst` choice
can ship with `Fastest` using the workaround below, but the ratio is poor.
Measured on 2026-11-20 on a 2.6 MB tar of Rust source (zip and lzma-rust2
crates): `ruzstd` `Fastest` 802,079 bytes; reference `zstd -1` 553,105;
`zstd -3` 498,671; `gzip -6` 509,888. So `Fastest` is about 45 percent
larger than zstd level 1 and worse than gzip on this input. One input only;
repeat on a real corpus. Consider not offering `tar.zst` until the encoder
improves.

Workaround for the panics, to be used by #67 if it offers `tar.zst`:

- Feed `compress` a source reader that never returns `Err` (on cancel it
  returns `Ok(0)`, which is end of input) and a sink that never returns
  `Err` (it records the first error in a flag and swallows later writes).
  After `compress` returns, the task checks the flag and the cancel flag,
  discards the output file on either, and reports a worded error.
- Run the whole thing in `spawn_blocking` and turn a `JoinError` that
  `is_panic()` into a worded "compression failed" error, as a second line
  of defence, never as the plan.
- Never pass `Default`, `Better`, `Best`; map the UI's level choice to
  `Fastest` and hide the slider for this format.

## Scope

Preferred route: contribute to `ruzstd` (open an issue first, offer the
patches as pull requests). Fallback route: a new crate in its own
repository. Either way, in stages:

1. A fallible encoder API (no `unwrap`, no `unimplemented!`), streaming
   `Write` adapter (`ZstdEncoder<W: Write>` with `finish() -> io::Result<W>`).
2. Levels 3 to 9 with a hash-chain or double-fast matcher, lazy matching,
   FSE and Huffman table encoding with repeat offsets and Huffman table
   reuse, and block splitting.
3. Levels 10 to 19 (binary-tree matcher, optimal parsing). Optional.
4. Long-distance matching and multithreaded blocks. Optional.

May leave out: dictionary training, legacy frame formats, magic-less frames.

## API

What `modules/archive.rs` (compression side, #67) needs:

```rust
pub struct ZstdEncoder<W: Write> { /* ... */ }

impl<W: Write> ZstdEncoder<W> {
    pub fn new(sink: W, level: u8) -> io::Result<Self>; // Err for unsupported level
    pub fn finish(self) -> io::Result<W>;               // writes the frame end and checksum
}
impl<W: Write> Write for ZstdEncoder<W> { /* streaming, bounded memory */ }

pub fn max_supported_level() -> u8;
```

Memory must be bounded by the level's window (state it in the docs), the
encoder must never panic on any input or any sink error, and output must be
a standard frame with a content checksum so the reference decoder verifies
it.

## Test strategy

- Round trip: random and structured inputs (empty, 1 byte, 128 KiB boundary,
  exactly one block, multi-block, 1 GiB streamed) through the encoder and the
  reference decoder (`zstd -d` via an `#[ignore]` test, plus `ruzstd`'s own
  decoder in the normal test run).
- Reference corpora: Silesia and the Canterbury large corpus, compared on
  size and speed against `zstd` levels 1, 3, 9 and 19. Targets: within 10
  percent of the reference size at the same nominal level.
- Fuzz: `cargo-fuzz` round-trip target; error injection on source and sink
  (the failure seen in the survey), asserting an `Err`, never a panic.
- Interop: output of every level decoded by `zstd` 1.5.x, by `ruzstd`, and
  by the `zip` crate when used as method 93.

## Licence notes

The zstd format is an open specification (RFC 8878) with a BSD-3-Clause
reference implementation (dual BSD or GPL-2.0). Writing an encoder from the
RFC is unencumbered. Copying code from the C library or from
`libzstd-rs-sys` is allowed but brings BSD-3-Clause notice duties; prefer
the RFC. `ruzstd` is MIT. Target licence for a new crate: MIT OR Apache-2.0.
RFC 8878 and the reference licence were not re-fetched in this survey.

## Honest size estimate

`ruzstd` today: 12,600 lines, of which the encoder is 2,100 and the decoder
with FSE and Huffman is 7,400. A real level 3 to 9 encoder: 3,000 to 5,000
more lines and four to eight weeks, mostly the matcher and the entropy
coder's cost model. Levels 10 to 19: another 2,000 lines and four weeks.
A fallible API cleanup alone (stage 1): a few days.
