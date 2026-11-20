# Brief: SMB2/3 client crate

Date: 2026-11-20. Status: **contingency, not blocking.** The survey in
[`docs/decisions/smb-mtp.md`](../decisions/smb-mtp.md) found `smb2 =0.43.0`
(pure Rust, tokio, NTLMv2 and Kerberos) fit to try. This brief is written so
that if the acceptance spike for issue #80 fails and upstream cannot fix it,
the crate can be built in its own repository without another survey. It also
holds the `Backend` mapping the SMB module uses with `smb2` today.

## Scope

A pure-Rust, async (tokio) SMB2/3 client library. No C dependencies, no
`-sys` crates, no `build.rs` that needs a system tool.

Stage 1, the minimum a file manager can ship on:

- TCP transport on port 445 only. Dialects 2.1, 3.0, 3.0.2, 3.1.1 with
  secure negotiate and pre-authentication integrity (3.1.1). No SMB1, ever.
- NTLMv2 inside SPNEGO, and guest sessions. Signing (HMAC-SHA256 for 2.x,
  AES-CMAC for 3.x) always verified when negotiated. Refuse to downgrade.
- Tree connect, share enumeration (`IPC$` and the `srvsvc` RPC
  `NetShareEnum`), create/open, read, write, close, flush, query directory,
  query info, set info (rename, delete-on-close, times, attributes),
  credits and request pipelining (a sliding window for large reads and
  writes), cancel.
- A file-name type that carries UTF-16 faithfully (`Vec<u16>` or
  `OsString`-like), with `&str` convenience.

Stage 2:

- Encryption (AES-128/256-CCM and GCM), then Kerberos (AES etypes 17/18, RC4
  only if unavoidable; AS and TGS exchange over KDC TCP, plus reading an MIT
  credential cache), then change notification (`watch`), then reconnect with
  durable handles, then DFS referrals.

Stage 3 (may be left out): LZ4 compression, server-side copy, named streams,
multichannel, QUIC, RDMA, NetBIOS transport on 139.

Explicitly out of scope: a server, SMB1/CIFS, printing, general RPC beyond
share enumeration, mounting.

## API

What the saola-files module `src/modules/smb.rs` needs. Signatures are
sketches; the crate may shape them differently as long as each capability
exists. `Backend` method in brackets.

```rust
pub struct Client;           // one TCP connection + one session
pub struct Share;            // a tree connect

impl Client {
    pub async fn connect(cfg: ClientConfig) -> Result<Client, Error>;
    pub async fn list_shares(&self) -> Result<Vec<ShareInfo>, Error>;
    pub async fn connect_share(&self, name: &str) -> Result<Share, Error>;
}

impl Share {
    // [list]: one call per directory, not one per entry.
    pub async fn list_dir(&self, path: &SmbPath)
        -> Result<Vec<DirEntry>, Error>;
    // [metadata]
    pub async fn stat(&self, path: &SmbPath) -> Result<DirEntry, Error>;
    // [read]: chunked, pipelined, cancel by drop.
    pub async fn open_read(&self, path: &SmbPath)
        -> Result<ReadStream, Error>;
    // [write]: close() resolves only after flush completes (durability
    // contract on `WriteSink`, see `src/core/vfs.rs`).
    pub async fn open_write(&self, path: &SmbPath, create_new: bool)
        -> Result<WriteHandle, Error>;
    pub async fn mkdir(&self, path: &SmbPath) -> Result<(), Error>;
    pub async fn rename(&self, from: &SmbPath, to: &SmbPath)
        -> Result<(), Error>;
    pub async fn remove(&self, path: &SmbPath, is_dir: bool)
        -> Result<(), Error>;
    pub async fn set_times(&self, path: &SmbPath, t: FileTimes)
        -> Result<(), Error>;
    // [watch], stage 2
    pub async fn watch(&self, path: &SmbPath, recursive: bool)
        -> Result<Watcher, Error>;
}
```

Requirements that come from our code, not from the protocol:

- **Errors** are a typed `ErrorKind` (not found, access denied, already
  exists, is a directory, not a directory, disk full, connection lost,
  auth required, signature invalid, timed out, cancelled) mapped one-to-one
  to `VfsError`. Never a panic, never an `unwrap` on server input. Server
  bytes are hostile.
- **One runtime:** use the ambient tokio runtime; never construct one.
  Futures `Send`; handles `Send + Sync` or clearly documented otherwise.
- **Bounded memory:** streaming read and write with a fixed window; a
  directory listing in batches so a 100,000-entry folder works.
- **Cancellation by drop** must leave the connection usable (cancel-safe).
- **Credentials** are never logged, and are zeroised on drop where cheap.
- Secrets prompts stay in saola-files; the crate takes them as parameters
  (user, domain, password, or a credential-cache path).

How the module sits on it: `SmbBackend` holds an `Arc<Mutex<Client>>`-style
pool through `core::remote::RemoteManager` (one connection per authority;
more connections when the ops engine runs transfers in parallel). `list` of
the authority root lists shares as folders; a path's first segment is the
share. `caps`: `RENAME_IN_PLACE` always, `WATCH` once stage 2 lands, never
`TRASH` or `LOCAL_PATH` (open = download to temp, read-only caveat).

## Test strategy

- **Wire codecs:** golden byte vectors from the MS-SMB2 specification
  examples and from captured Samba and Windows sessions (scrubbed), for every
  message in both directions; property tests that encode-decode round trip
  and that no input panics; fuzz targets for every parser (negotiate,
  session setup, create contexts, query directory, SPNEGO, NTLM, Kerberos
  ASN.1).
- **Crypto:** the published test vectors (NTLM from MS-NLMP section 4.2,
  SP800-108 KDF, AES-CMAC and GCM and CCM from NIST, Kerberos RFC 3961 and
  RFC 3962 vectors).
- **Against a real server:** a Samba container (`smbd`) on localhost, run in
  CI and locally: dialect pinning (2.1, 3.0, 3.1.1), signing and encryption
  required, guest, bad password, share without access, long names, names
  with reserved characters, large files, many files, kill the server
  mid-transfer. A second container with a Samba domain member and an MIT
  KDC for Kerberos. A Windows server cannot be scripted in CI; keep a manual
  checklist.
- **Fakes:** an in-memory mock transport for credit accounting, reconnect
  and error paths that a real server will not produce on demand.
- **In saola-files:** `Backend` tests use a fake `Client` trait; the
  container tests are `#[ignore]`d and run on request. Never against a real
  production share.

## Licence notes

- Licence the crate MIT OR Apache-2.0, as the ecosystem expects.
- [MS-SMB2] and the related Microsoft Open Specifications are published
  under the Open Specification Promise, which covers implementing the
  protocol; read the current terms before starting and keep a copy of the
  version consulted. Do not copy Samba (GPLv3) or libsmb2 (LGPL) code or
  test files; re-derive from the specification. Check the licences of any
  captured traffic before publishing it.
- Permitted dependencies are RustCrypto crates and `tokio`; run
  `cargo deny` with a licence allow-list in the crate's CI.
- The Kerberos code reads MIT credential cache files, a documented format;
  no MIT Kerberos code is needed.

## Size estimate

Do not plan this as a side project. Rough, honest figures for a crate that
passes the stage 1 and stage 2 scope with the test strategy above:

| Stage                         | Lines of Rust (with tests) | Focused effort    |
| ----------------------------- | -------------------------- | ----------------- |
| Stage 1 (NTLMv2, signing, file ops, pipelining) | 12,000 to 18,000 | 6 to 10 weeks |
| Stage 2 (encryption, Kerberos, watch, reconnect, DFS) | 12,000 to 20,000 | 8 to 14 weeks |
| Stage 3                       | 8,000+                     | open-ended        |

For scale, the existing `smb2` crate is about 90,000 lines including tests
and docs, and was built over seven months with heavy AI assistance. The
largest risks are not the happy path: they are server quirks (NAS firmware,
old Samba, Windows differences), credit and pipelining deadlocks, and
Kerberos edge cases (clock skew, referrals, cross-realm). Expect a long tail
of interop bugs that only real devices reveal. Writing this is a last resort.
