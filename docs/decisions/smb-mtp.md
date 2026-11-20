# Decision: SMB and MTP client crates

Date: 2026-11-20. Status: proposed (survey for issue #79). Authors: survey
task. Follow-ups: #80 (SMB module), #81 (MTP module).

## Verdict

| Protocol | Verdict                                                      |
| -------- | ------------------------------------------------------------ |
| SMB2/3   | Use `smb2` at `=0.43.0` (exact pin). Fallback: `smb 0.12.1`. |
| MTP      | Use `mtp-rs` at `=0.38.1` (exact pin), on `nusb 0.2.9`.      |

The premise of the backlog ("these crates likely do not exist") turned out
to be wrong on the survey date. Both protocols have a pure-Rust crate that
is recent, active, tokio-compatible and passes the "Pure Rust only" rule.
No external project is needed to start. Both are young (under 10 months),
pre-1.0 and break their API on most minor releases, so the module tasks pin
exact versions and carry an acceptance spike (below) that decides, with
evidence, whether the crate is fit before the module is built on it.

The two briefs in `docs/external-crates/` are therefore written as
contingency briefs: they are not blocking anything today. They activate only
if a spike fails. They double as the method-by-method `Backend` mapping.

Consequence for the board: #80 and #81 carry `blocked:external`. That label
no longer matches reality. This survey does not change labels; the
dispatcher should replace it with a dependency on the spike result.

## Method and what was verified

All facts below were collected on 2026-11-19/20.

- Versions, release dates, licences, downloads: the crates.io API and
  `cargo info`.
- Repository activity: the GitHub API (`gh api repos/...`).
- Purity: for each candidate, a scratch project (outside this repo) depending
  on that exact version, then `cargo tree -e normal,build` searched for
  `-sys` crates, `cc`, `cmake`, `bindgen`, `pkg-config`, `ring`, `aws-lc`,
  `openssl`, `libusb`, `rusb`, `krb5`, `gssapi`. Target: x86_64 Linux,
  default features.
- Protocol coverage: each crate's README and (for `smb2`, `mtp-rs`) the
  public method list in the downloaded source.

Not verified, and called out where it matters: nothing was run against a
real SMB server or a real phone (there is no Samba binary on the survey
machine, and no MTP device attached). No performance claims in the READMEs
were re-measured. Windows and macOS dependency trees were not checked
(saola-files targets Linux only).

## SMB2/3 candidates

| Crate              | Version | Released   | Licence        | Pure Rust (Linux, default) | Notes                         |
| ------------------ | ------- | ---------- | -------------- | -------------------------- | ----------------------------- |
| `smb2`             | 0.43.0  | 2026-11-06 | MIT OR Apache-2.0 | Yes (71 crates, none flagged) | Best fit. See below.       |
| `smb` (smb-rs)     | 0.12.1  | 2026-09-20 | MIT            | Yes (342 crates, none flagged) | Fallback. Heavier tree.    |
| `smb2-client`      | 0.2.5   | 2026-09-06 | MIT            | Yes (52 crates)            | Minimal: NTLMv2 + I/O only.   |
| `smolder-smb-core` | 0.4.0   | 2026-08-16 | MIT            | Yes (65 crates; 60 with the `kerberos` feature) | Aimed at offensive-security tooling; 520 downloads. |
| `smb3_client`      | 0.1.1   | 2024-09-23 | unknown        | Not checked                | Abandoned (2024).             |
| `pavao`, `smbc`, `remotefs-smb` | n/a | n/a  | n/a            | No: bind libsmbclient      | Disqualified by rule.         |

`libc` and `openssl-probe` appear in some trees. `libc` is the Rust-side
declaration of system calls, not a C library that must be installed;
`openssl-probe` only looks for certificate paths and links nothing. Neither
is a `-sys` crate. If Jordan's reading of the rule is stricter than that,
this is the one point to confirm.

### `smb2` 0.43.0 (recommended)

- Repository `vdavid/smb2`, 32 stars, 4 contributors, created 2026-04-08,
  last commit 2026-11-17, 81k recent downloads. Written for the Cmdr file
  manager, so its use case is ours. MSRV 1.90 (our toolchain is 1.100).
- Dialects: SMB 2.x and 3.x including 3.1.1. SMB1 is explicitly not planned
  (good). Signing: HMAC-SHA256, AES-CMAC, AES-GMAC. Encryption: AES-128/256
  CCM and GCM. LZ4 compression. Not supported: multi-channel, QUIC, RDMA.
- Auth: NTLMv2 only (NTLMv1 refused, as it should be); Kerberos with etypes
  17, 18 and 23, doing the AS and TGS exchange itself (password given) and
  reading an MIT credential cache (`kinit` tickets, no password); guest
  sessions, with a deliberate refusal when a guest session is returned for a
  named account. Kerberos is hand-written (about 6,000 lines), not a binding.
- Operations: list shares (IPC$ + srvsvc), list directory (also in batches),
  stat, read (whole, pipelined, streaming with progress and cancel), write
  (whole, streaming, resumable writer), delete, rename, mkdir, rmdir,
  `set_times` and attribute flags, server-side copy, disk space, DFS,
  reparse points (symlinks and junctions reported), named streams, change
  notification (`watch`), reconnect with durable handles.
- Async model: tokio by default (features `tokio/net`, `io-util`, `rt`,
  `time`); optional smol. It uses the ambient tokio runtime and does not
  start its own, so the "one runtime" rule holds when it is called from
  iced's tokio executor. Not verified: that every returned future is `Send`
  (needed for `async_trait`); the spike checks this by compiling.
- Weaknesses to weigh honestly:
  - Churn: 43 minor releases in seven months, each a possible API break.
    Pin exactly and bump deliberately.
  - Bus factor: effectively one maintainer.
  - The README states the code was written extensively with Claude and
    relies on its test suite (unit tests with a mock transport, proptest,
    Docker Samba integration tests). This is disclosed, not hidden; it is a
    reason for the spike, not a disqualifier.
  - API takes `&mut SmbClient` and `&mut Tree` for most calls, so one
    connection is used by one operation at a time; parallelism needs several
    connections (the crate says so itself).
  - File names are `&str`/`String`. SMB names are UTF-16 on the wire, so
    this is lossless except for unpaired surrogates; AGENTS.md's OsString
    rule must be applied at our boundary (`OsStr` to `str` conversion
    returning `VfsError` for non-UTF-8 names, which Linux can produce but an
    SMB server cannot store anyway). Names containing `? * " : < > \ |` are
    mapped to the Unicode private-use area automatically.
  - Lists a whole directory in one call (`list_directory`) or in batches
    (`open_directory_reader`); either fits our coarse `list`.

### `smb` (smb-rs) 0.12.1 (fallback)

- Repository `afiffon/smb-rs`, 88 stars, 9 contributors, created 2024-12,
  last commit 2026-10-13. MIT only. Documentation says "no direct
  dependencies on C libraries".
- Broader transport coverage than `smb2` (QUIC, NetBIOS, multi-channel).
  NTLM and Kerberos via Devolutions' `sspi` crate, which is a large, active
  project. Tree of 342 crates, heavier to build and audit.
- The `smb2` author reports that it sends one request at a time and is slow
  for downloads, and that it has few tests. This is a competitor's claim and
  was not re-measured; treat as unverified.
- Use only if the `smb2` spike fails.

### Rejected

`smb2-client` stops at basic I/O (no DFS, no watch, one contributor, two
stars). `smolder-smb-core` is built for security tooling and has almost no
users. `smb3_client` has not been touched since 2024.

## MTP candidates

| Crate       | Version | Released   | Licence         | Pure Rust (Linux, default) | Notes                    |
| ----------- | ------- | ---------- | --------------- | -------------------------- | ------------------------ |
| `mtp-rs`    | 0.38.1  | 2026-11-11 | MIT OR Apache-2.0 | Yes (45 crates)          | Recommended.             |
| `mtp` + `mtp_spec` | 0.1.0 | 2026-06-21 | MIT OR Apache-2.0 | Yes (90 crates, tokio pulled in) | Early; 8 recent downloads. |
| `mtpkit`    | 0.1.0   | 2026-08-09 | LGPL-2.1-or-later | Yes (22 crates)          | LGPL, no repository link. |
| `ptp`       | 0.5.0   | 2016-09-13 | MIT/Apache-2.0  | Not checked                | Abandoned (2016); PTP only. |
| `winmtp`, `mtp-mount` | n/a | n/a | n/a            | n/a                        | Windows API / FUSE; not applicable. |

USB layer: `nusb` 0.2.9 (released 2026-10-24, Apache-2.0 OR MIT, 545 stars,
27 contributors, 1.1M recent downloads, last commit 2026-11-08). On Linux it
talks to usbfs through `rustix`; the only low-level crates in its tree are
`rustix` and `linux-raw-sys` (Rust declarations of kernel constants, not
C). It is the clear choice and is what `mtp-rs` already uses.

### `mtp-rs` 0.38.1 (recommended)

- Repository `vdavid/mtp-rs`, same author as `smb2` (again for Cmdr), 50
  stars, 11 contributors, created 2026-01-30, last commit 2026-11-16.
  MSRV 1.90. Includes a conformance test suite and a virtual device mode.
- Operations: open (first, by serial, by location), list storages, list
  objects (also streamed, recursive, with cancel), object info, download
  (whole, ranged, streaming), thumbnails, upload (two-phase, with progress),
  create folder, delete, move, copy, rename, free space, device events
  (`ObjectAdded`, `ObjectRemoved`, `StoreAdded`, `StoreRemoved`,
  `StorageInfoChanged`, `ObjectInfoChanged`, `DeviceInfoChanged`,
  `DeviceReset`), USB link speed, PTP low-level for cameras.
- Not covered by design: MTPZ, playlists and albums, vendor extensions,
  pre-Android 5 quirks. It deliberately does not copy libmtp's
  device-quirk table (LGPL), so odd devices may misbehave. Unverified.
- Async model: runtime-agnostic. Its tree has `futures` and
  `futures-timer` but no `tokio`, so there is no second-runtime risk.
  `nusb` runs its own small background machinery on Linux; not verified
  beyond reading the dependency tree.
- Weaknesses to weigh honestly:
  - Handle-based, not path-based. `Backend` takes `Location` paths, so the
    module needs a path-to-handle cache and a rule for invalidating it from
    `ObjectAdded/Removed` events. This is the biggest piece of module work.
  - No way to set modification times (MTP has no general operation for it);
    `set_times` returns a worded "not supported" error.
  - Upload creates the object before the data arrives; a failed upload
    leaves a partial object that the module must delete (the crate returns
    its handle and never auto-deletes).
  - Rename and upload are not available on every device
    (`supports_rename`, `supports_upload`); `Caps` is per instance, so the
    module sets `RENAME_IN_PLACE` from the device's answer.
  - Linux access: phones are often already claimed by other software
    (for example a desktop automounter), in which case `nusb` cannot claim
    the interface; the crate reports this as `Error::is_exclusive_access`.
    The README's udev advice (mode 0666 for every USB device) is far too
    broad; our docs must recommend a narrow rule or the existing logind
    `uaccess` mechanism instead. Whether a stock Arch/niri session grants
    access to a plugged-in phone without extra rules was not verified.
  - Same single-maintainer correlation as `smb2`.

### Rejected

`mtpkit` is LGPL-2.1-or-later and has 60 downloads and no repository link.
The `mtp` and `mtp_spec` crates are 0.1.0, barely used, and pull in tokio.
`ptp` is ten years old.

## What `Backend` needs, and who supplies it

Detail per method is in the two briefs. Summary:

| `Backend` item | `smb2`                                   | `mtp-rs`                                   |
| -------------- | ---------------------------------------- | ------------------------------------------ |
| `scheme`       | `smb`                                    | `mtp`                                      |
| `caps`         | `RENAME_IN_PLACE`, `WATCH`; no `TRASH`, no `LOCAL_PATH` | `WATCH` (events), `THUMBNAILS`, `RENAME_IN_PLACE` if supported; no `TRASH`, no `LOCAL_PATH` |
| `list`         | `list_directory`; root of a server lists shares (`list_shares`) | `list_objects` per storage; device root lists storages |
| `metadata`     | `stat`                                   | `get_object_info` after path resolution    |
| `read`         | `download` (chunk stream)                | `download` (stream)                        |
| `write`        | `create_file_writer` (durable `close`)   | `upload` (needs size up front, see below)  |
| `mkdir`        | `create_directory`                       | `create_folder`                            |
| `rename`       | `rename`                                 | `rename` (same folder), `move_object`      |
| `remove`       | `delete_file` / `delete_directory`       | `delete`                                   |
| `set_times`    | `set_times`                              | not supported                              |
| `watch`        | `watch` (CHANGE_NOTIFY)                  | `next_event`                               |

Two trait-level frictions to resolve in the module tasks, not here:

- MTP `upload` needs the object size before the first byte. `Backend::write`
  does not carry a size. The MTP module either buffers to a temp file, or
  the trait grows an optional size hint. A trait change takes
  `lock:backend-trait`; prefer the temp-file buffer first.
- Both protocols need the connection setup to happen outside `resolve`, the
  same way SFTP does through `core::remote::RemoteManager`: SMB needs a
  credentials prompt, MTP needs a device picker and a "unlock your phone"
  state. The `ConnectEvent` prompt types may need an SMB variant (domain,
  user, password) and an MTP variant (none, or a worded wait).

## Acceptance spikes for the follow-up tasks

Each spike is a throwaway test binary or `#[ignore]`d integration test, run
locally, never against a production host.

### SMB spike (before #80 builds the module)

1. Compiles with the saola-files tokio executor, and every future used by
   the planned `Backend` mapping is `Send` (compile test).
2. Against a local Samba container (Docker is available on the survey
   machine; run it on localhost with a throwaway user and share):
   connect with NTLMv2 on SMB 3.1.1 with signing required, then with
   encryption required; list shares; create, list, stat, rename, delete a
   directory tree; stream-download and stream-upload a file of several
   hundred megabytes while checking memory stays flat; confirm that closing
   the write handle means the data is durable (reconnect and compare hash).
3. Names: a non-ASCII name, a name with `?` and `:`, a very long name, and
   a lone-surrogate-free round trip through the `OsString` boundary.
4. Errors: wrong password, no permission, share missing, server killed
   mid-transfer, all mapping to a worded `VfsError` with no panic.
5. `watch`: create a file from a second connection and observe the event.
6. Kerberos: only if a test KDC is available (a containerised MIT KDC with
   a Samba member); otherwise record "not tested" and keep Kerberos out of
   the first release (NTLMv2 only).
7. Record: build time and dependency count added to the tree (expected
   about 70 crates), and the licence list from `cargo deny` or equivalent.

### MTP spike (before #81 builds the module)

1. Compiles and runs inside the iced tokio executor; confirm no extra
   runtime thread beyond `nusb`'s own.
2. A real phone (Android, unlocked, "file transfer" mode): list storages;
   walk `DCIM`; time a listing of a folder with 1,000+ files; download a
   photo and compare its hash; upload a small file and a file larger than
   2 GiB if the phone allows; delete and rename; free space.
3. Events: copy a file to the phone with another tool and observe
   `ObjectAdded`; unplug the phone mid-listing and mid-download and confirm
   a worded error, no panic, no hang.
4. Access: record exactly what a stock session needs for the phone to be
   openable (udev rule, group, `uaccess`), and what happens when another
   program already holds the interface. This is the main unknown.
5. Without a phone (CI): the crate's virtual device mode, enabled only in a
   `dev-dependencies` feature set, for the path-cache logic.

## Risks and the reversal rule

- Both crates are young and share an author. If the author stops, we hold a
  pinned dependency with no upstream. Mitigation: exact pins, vendor-free
  (Cargo's registry copy is enough), and the contingency briefs.
- If a spike fails on a protocol-level defect, try an upstream issue and
  patch first. If that is not workable, open the external project from the
  brief, in its own repository.
- Re-run this survey when either crate reaches 1.0 or when a pin is bumped
  past a minor version.

## Sizes, for the record

If a crate has to be written (see the briefs): an SMB2/3 client of the
quality a file manager needs is roughly 25,000 to 40,000 lines of Rust
including tests and two to four person-months of focused work, more than
the rest of the saola-files backlog combined; the `smb2` crate alone is
about 90,000 lines including tests and docs. An MTP stack is much smaller:
roughly 6,000 to 10,000 lines and three to six weeks, plus ongoing device
testing that cannot be finished.
