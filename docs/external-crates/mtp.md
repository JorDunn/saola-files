# Brief: MTP client crate

Date: 2026-11-20. Status: **contingency, not blocking.** The survey in
[`docs/decisions/smb-mtp.md`](../decisions/smb-mtp.md) found `mtp-rs
=0.38.1` on `nusb 0.2.9` (pure Rust, runtime-agnostic) fit to try. This brief
exists so that if the acceptance spike for issue #81 fails and upstream
cannot fix it, the crate can be written in its own repository without
another survey. It also holds the `Backend` mapping the MTP module uses with
`mtp-rs` today.

## Scope

A pure-Rust, async MTP (and PTP) client over USB. Built on `nusb` (pure Rust,
Linux usbfs through `rustix`). No `libmtp`, no `libusb`, no `-sys` crates.
Linux only is acceptable; other platforms are out of scope for saola-files.

Stage 1, the minimum a file manager can ship on:

- Find devices with an MTP or PTP interface (USB still-image class 6, or the
  vendor class with an `MTP` string descriptor, as Android uses); open one;
  claim the interface; open a session; read `GetDeviceInfo`.
- `GetStorageIDs`, `GetStorageInfo` (name, capacity, free space, read-only).
- `GetObjectHandles` per parent, `GetObjectInfo` (name, size, dates, format,
  parent), batch listing through `GetObjectPropList` where the device offers
  it, otherwise per-object calls.
- `GetObject` / `GetPartialObject` as a stream with progress and cancel
  (USB still-image class cancel request); `SendObjectInfo` + `SendObject`
  upload as a stream; `DeleteObject`; `GetThumb`.
- Errors that distinguish: no device, device busy or claimed by another
  program, locked phone (storage empty until unlocked), unplugged,
  unsupported operation, storage full, access denied.

Stage 2:

- Events from the interrupt endpoint (`ObjectAdded`, `ObjectRemoved`,
  `StoreAdded`, `StoreRemoved`, `ObjectInfoChanged`, `StorageInfoChanged`).
- `MoveObject`, `CopyObject`, rename through `SetObjectPropValue`
  (`ObjectFileName`), when the device lists the operation as supported.
- Hotplug notification, so the sidebar can show a phone appearing.

Stage 3 (may be left out): playlists and albums, device-specific quirks
table (derived from observation, not from libmtp), MTPZ, vendor extensions,
camera-specific PTP.

Out of scope: Windows (WPD) and macOS, DRM, syncing logic, anything that
needs libusb.

## API

Handle-based, because MTP is. The saola-files module owns the path to handle
mapping. `Backend` method in brackets.

```rust
pub struct Device;     // an open session
pub struct Storage;    // one storage on the device

impl Device {
    pub fn list_devices() -> Result<Vec<DeviceSummary>, Error>;
    pub async fn open(id: &DeviceId) -> Result<Device, Error>;
    pub async fn storages(&self) -> Result<Vec<Storage>, Error>;
    pub fn supports_rename(&self) -> bool;       // drives Caps
    pub fn supports_upload(&self) -> bool;
    pub async fn next_event(&self) -> Result<DeviceEvent, Error>; // [watch]
}

impl Storage {
    pub fn info(&self) -> &StorageInfo;          // name, free space
    // [list]: a stream, so a 3,000-photo folder shows first rows early.
    pub fn list(&self, parent: Option<Handle>)
        -> impl Stream<Item = Result<ObjectInfo, Error>>;
    pub async fn object_info(&self, h: Handle)   // [metadata]
        -> Result<ObjectInfo, Error>;
    pub async fn download(&self, h: Handle)      // [read]
        -> Result<ByteStream, Error>;
    pub async fn upload(&self, parent: Option<Handle>, name: &OsStr,
        size: u64, data: ByteStream) -> Result<Handle, UploadError>; // [write]
    pub async fn create_folder(&self, parent: Option<Handle>, name: &OsStr)
        -> Result<Handle, Error>;                // [mkdir]
    pub async fn rename(&self, h: Handle, name: &OsStr)
        -> Result<(), Error>;                    // [rename], same folder
    pub async fn move_object(&self, h: Handle, new_parent: Option<Handle>)
        -> Result<(), Error>;                    // [rename], across folders
    pub async fn delete(&self, h: Handle) -> Result<(), Error>; // [remove]
    pub async fn thumbnail(&self, h: Handle) -> Result<Vec<u8>, Error>;
}
```

`set_times` has no MTP equivalent for general files; the module returns a
worded "this device cannot change dates" error and does not set
`Caps::SET_PERMISSIONS`.

Requirements that come from our code:

- **No tokio in the crate's public surface and no second runtime.** Plain
  `futures`; works inside iced's tokio executor.
- **Cancel by drop** must not corrupt the session; a cancelled download
  sends the class cancel request and drains.
- **Upload size is known up front** (MTP needs it before the data phase).
  The module buffers to a temp file when the source size is not known; a
  failed upload returns the partial object's handle so the module can delete
  it. The crate never deletes by itself.
- **Names** are UTF-16 on the wire; expose them as `String` plus a lossless
  `OsString` conversion helper, and reject names the device cannot store with
  a typed error.
- **Slow devices:** every USB transfer has a timeout; a stalled phone yields
  `Error::TimedOut`, never a hang.
- **No panics** on malformed containers; fuzz the container and dataset
  parsers.

How the module sits on it: `src/modules/mtp.rs` registers `mtp` through
`core::remote::RemoteManager` (one session per device; the authority is the
device serial). The device root lists storages as folders; the rest is
`Storage` paths. A path-to-handle cache is filled by `list` and invalidated
by events; every `Backend` call resolves its path through it (falling back to
a parent walk on a miss). `caps`: `WATCH`, `THUMBNAILS`, and
`RENAME_IN_PLACE` only when `supports_rename()` is true; never `TRASH` or
`LOCAL_PATH`. A locked phone is an empty state with worded advice
("Unlock the phone and allow file transfer"), not an error dialog.

## Test strategy

- **Containers and datasets:** golden byte vectors for every operation,
  response and event, captured from real devices (scrubbed of serial numbers
  and personal file names) and from the PIMA 15740 and MTP 1.1 documents.
- **Virtual device:** an in-memory MTP responder, driven through a mock USB
  transport trait, covering storage trees, large listings, upload failure
  halfway, unplug, slow device, and every error code. This is what runs in
  CI.
- **Real hardware (manual, by a person):** at least two Android phones from
  different vendors, one e-reader, one camera, one locked phone, one phone in
  charge-only mode. Record results in the crate's repository. CI cannot do
  this and the brief must not pretend it can.
- **Fuzzing:** container parser, dataset parsers (`DeviceInfo`, `StorageInfo`,
  `ObjectInfo`, property lists).
- **In saola-files:** the path-cache and the `Backend` mapping are tested
  against a fake `Device` trait; the real-hardware run is a manual spike
  recorded in the issue as words.

## Licence notes

- Licence the crate MIT OR Apache-2.0.
- Do not copy `libmtp` (LGPL) code, its device table, or its test data.
  Re-derive device quirks from observation. The USB-IF still-image class
  and PTP (ISO 15740) and MTP specifications are public; keep a note of the
  versions consulted.
- Depend on `nusb` (Apache-2.0 OR MIT) and RustCrypto-free standard crates
  only; run `cargo deny` with a licence allow-list.
- Avoid `mtpkit` (LGPL-2.1-or-later) as a dependency or as a code source.

## Size estimate

Smaller than SMB, but not small, and its hard part is hardware, not code.

| Stage                      | Lines of Rust (with tests) | Focused effort |
| -------------------------- | -------------------------- | -------------- |
| Stage 1 (list, read, write, delete, errors) | 6,000 to 10,000 | 3 to 5 weeks |
| Stage 2 (events, move, copy, rename, hotplug) | 3,000 to 5,000 | 2 to 3 weeks |
| Device quirks                | open-ended               | ongoing        |

For scale, `mtp-rs` is about 32,000 lines including tests and docs. Real
phones differ in how they handle listing, unusual file names, partial
transfers, locking and rename; expect weeks of testing on physical devices
that cannot be replaced by a simulator. Writing this is a last resort.
