//! UDisks2 mount discovery: which user-relevant filesystems are currently
//! mounted, fed to the places sidebar as a live stream. iced-free, like
//! every other `core/` module — `ui::sidebar` is the only thing that turns
//! a snapshot into widgets.
//!
//! # The `MountsSource` seam (CLAUDE.md: "D-Bus … behind traits with
//! fakes")
//!
//! [`MountsSource`] is the trait: [`UdisksMounts`] is the real zbus-backed
//! implementation, [`FakeMountsSource`] drives it from a plain, pre-scripted
//! `Vec` of snapshots for unit tests. Nothing in this crate needs a real
//! bus to exercise "a drive gets plugged in, then unplugged" — see this
//! module's own tests, and `ui::sidebar`'s.
//!
//! # Why whole-snapshot-replace, not fine-grained add/remove signals
//!
//! Follows the same ObjectManager fan-in shape `saola-panel`'s
//! `bluetooth.rs`/`network.rs` establish (see their module docs for the
//! fuller teaching note): UDisks reports adds, removes, and property
//! changes (a drive's label, its `Removable` flag) through three different
//! signals. Rather than hand-maintaining a partial mirror of its object
//! tree — a separate bug lurking behind each signal shape — every signal
//! here triggers one `GetManagedObjects()` re-read and the whole mount
//! list is rebuilt from scratch. A udisks tree is a handful of objects, so
//! one extra round trip per event is cheap, and the rebuilt snapshot is
//! immune to partial-update bugs by construction. This is still strictly
//! **signal-driven, never a poll** (CLAUDE.md) — every rebuild is a
//! *response to a bus signal*, nothing here ticks.
//!
//! No UDisks on the bus, and nothing currently mounted, both look
//! identical: an empty (or immediately-ended) stream. There is no separate
//! "absent" signal — the sidebar's mounts section simply doesn't render
//! (CLAUDE.md's degrade-to-nothing rule), the same contract
//! `Backend::watch`'s `None` return gives the directory view.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use futures::SinkExt;
use futures::channel::mpsc;
use futures::stream::{self, BoxStream, StreamExt};
use zbus::fdo::{ManagedObjects, ObjectManagerProxy};
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{Array, ObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

/// UDisks2's D-Bus service name — a **system** service, like UPower and
/// BlueZ.
const UDISKS_SERVICE: &str = "org.freedesktop.UDisks2";

const FILESYSTEM_INTERFACE: &str = "org.freedesktop.UDisks2.Filesystem";
const BLOCK_INTERFACE: &str = "org.freedesktop.UDisks2.Block";
const DRIVE_INTERFACE: &str = "org.freedesktop.UDisks2.Drive";

/// How many pending snapshots [`UdisksMounts::watch`]'s bridge channel
/// buffers before a slow consumer would block the worker — mirrors
/// `modules::local`'s `WATCH_CHANNEL_CAPACITY` and the panel's D-Bus
/// modules (`battery.rs`/`bluetooth.rs`'s `iced::stream::channel(8, ..)`):
/// a handful of in-flight snapshots is plenty, since each one already
/// supersedes the last (CLAUDE.md's bounded-channel rule).
const MOUNTS_CHANNEL_CAPACITY: usize = 8;

/// One currently-mounted, user-relevant filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// The filesystem's label if it has one (UDisks' `Block.IdLabel`),
    /// else a fallback derived from the mount point.
    pub label: String,
    /// Where it's mounted — sidebar clicks navigate here via
    /// [`crate::core::vfs::Location::local`].
    pub mount_point: PathBuf,
    /// Whether the underlying drive reports itself as removable
    /// (`Drive.Removable`) — the sidebar picks between the hard-drive and
    /// USB-stick glyphs on this (`crate::icons::for_mount`), never a
    /// color (style guide §1: shape, never hue).
    pub removable: bool,
}

/// The CLAUDE.md-mandated seam: real udisks access lives entirely behind
/// this trait, so nothing that consumes it needs a bus to be tested.
pub trait MountsSource: Send + Sync {
    /// A stream of full mount-list snapshots — see the module docs for why
    /// whole-snapshot-replace, not per-mount add/remove events. An empty or
    /// immediately-ended stream is exactly "no mounts to show", covering
    /// both "udisks isn't on the bus" and "nothing is mounted right now".
    fn watch(&self) -> BoxStream<'static, Vec<Mount>>;
}

/// The real implementation, backed by a live system-bus connection to
/// `org.freedesktop.UDisks2`.
pub struct UdisksMounts;

impl MountsSource for UdisksMounts {
    fn watch(&self) -> BoxStream<'static, Vec<Mount>> {
        let (tx, rx) = mpsc::channel(MOUNTS_CHANNEL_CAPACITY);
        // Detached, same posture as `modules::local::LocalBackend::watch`'s
        // spawned inotify-translation task: the returned `rx` governs how
        // long this runs. Runs on the shared tokio runtime iced's own
        // `tokio` feature provides (CLAUDE.md: "one async runtime") — this
        // is called from inside `ui::sidebar`'s `Subscription::run`
        // builder, which iced polls on that runtime.
        tokio::spawn(watch_udisks(tx));
        rx.boxed()
    }
}

/// Drives [`MountsSource`] from a fixed, pre-scripted sequence of
/// snapshots. Building one with the snapshots a real udisks session would
/// emit for "a drive gets plugged in, then unplugged" and asserting the
/// stream yields them in order is the whole "fake-driven mount add/remove"
/// unit test PLAN.md calls for — no bus of any kind involved.
pub struct FakeMountsSource {
    snapshots: Vec<Vec<Mount>>,
}

impl FakeMountsSource {
    pub fn new(snapshots: Vec<Vec<Mount>>) -> Self {
        FakeMountsSource { snapshots }
    }
}

impl MountsSource for FakeMountsSource {
    fn watch(&self) -> BoxStream<'static, Vec<Mount>> {
        stream::iter(self.snapshots.clone()).boxed()
    }
}

/// The worker proper: connect, read the tree once, then rebuild and push a
/// snapshot on every udisks signal, forever. Any failure — no system bus,
/// udisksd not running — simply ends the function, which drops `sender`
/// and ends the stream `UdisksMounts::watch` returned; the degrade-to-
/// nothing behavior lives entirely in that empty-stream contract, not in a
/// fallback value sent here (contrast `battery_stream`'s explicit
/// `Battery::default()` push — there is no equivalent "known absent" value
/// for a *list*, an empty `Vec` sent once would look identical to "still
/// connecting").
async fn watch_udisks(mut sender: mpsc::Sender<Vec<Mount>>) {
    let _ = run(&mut sender).await;
}

async fn run(sender: &mut mpsc::Sender<Vec<Mount>>) -> zbus::Result<()> {
    // System bus: udisksd (like UPower and BlueZ) is a system service.
    let connection = Connection::system().await?;

    let object_manager = ObjectManagerProxy::builder(&connection)
        .destination(UDISKS_SERVICE)?
        .path("/")?
        .build()
        .await?;

    let mut current = snapshot(&object_manager.get_managed_objects().await?);

    // Teaching note (the three-way fan-in): udisks announces changes
    // through `InterfacesAdded`/`InterfacesRemoved` (a device plugged in
    // or removed) and `PropertiesChanged` (a filesystem got mounted or
    // unmounted, a label changed) — see `bluetooth.rs`'s doc comment for
    // the fuller version of this exact shape. All three are normalized to
    // `()` and merged, since the loop below always re-reads the whole tree
    // anyway; the `PropertiesChanged` rule has no proxy behind it because
    // the set of object paths changes at runtime (same reasoning
    // `bluetooth.rs`'s `MatchRule` doc comment gives).
    let added = object_manager.receive_interfaces_added().await?;
    let removed = object_manager.receive_interfaces_removed().await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(UDISKS_SERVICE)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    let properties = MessageStream::for_match_rule(rule, &connection, Some(8)).await?;

    let mut events = stream::select(
        stream::select(added.map(|_| ()), removed.map(|_| ())),
        properties.map(|_| ()),
    );

    // Dedupe against the last snapshot sent — a `PropertiesChanged` fires
    // for plenty of things this list doesn't care about (a drive's
    // `TimeDetected`, say), and without this every one would push an
    // identical snapshot.
    let mut last_sent: Option<Vec<Mount>> = None;

    loop {
        if last_sent.as_ref() != Some(&current) {
            if sender.send(current.clone()).await.is_err() {
                return Ok(());
            }
            last_sent = Some(current.clone());
        }
        if events.next().await.is_none() {
            return Ok(());
        }
        current = snapshot(&object_manager.get_managed_objects().await?);
    }
}

/// One object's interfaces, as `GetManagedObjects` hands them back —
/// mirrors `bluetooth.rs`'s identical local alias.
type Interfaces = HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>;

/// UDisks' whole object tree → the mount list the sidebar renders. A pure
/// function of its argument (no connection, no `await`) — the unit tests
/// below build fixture trees by hand, the same technique
/// `bluetooth.rs::snapshot`'s tests use.
fn snapshot(objects: &ManagedObjects) -> Vec<Mount> {
    let mut mounts: Vec<Mount> = objects
        .values()
        .filter_map(|interfaces| mount_from_object(objects, interfaces))
        .collect();
    // Alphabetical by label: object-path order (a `HashMap`'s own
    // iteration order, randomized per process) would reshuffle the
    // sidebar for no reason and defeat the worker's dedupe by making two
    // rebuilds of the same tree compare unequal.
    mounts.sort_by(|a, b| a.label.cmp(&b.label));
    mounts
}

/// One managed object → a [`Mount`], or `None` when it isn't a mounted
/// filesystem udisks-hosted objects the sidebar should show at all.
fn mount_from_object(objects: &ManagedObjects, interfaces: &Interfaces) -> Option<Mount> {
    let filesystem_props = interface(interfaces, FILESYSTEM_INTERFACE)?;
    let mount_point = first_mount_point(filesystem_props)?;

    let block_props = interface(interfaces, BLOCK_INTERFACE);

    // Objects udisks itself flags as not-for-end-users (loop devices, LVM
    // physical volumes, swap, …) — the same `HintIgnore` signal GNOME/
    // Nautilus honors to keep this exact kind of clutter out of a places
    // sidebar.
    if block_props
        .and_then(|properties| property_bool(properties, "HintIgnore"))
        .unwrap_or(false)
    {
        return None;
    }

    let label = block_props
        .and_then(|properties| property_str(properties, "IdLabel"))
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback_label(&mount_point));

    let removable = block_props
        .and_then(|properties| property_object_path(properties, "Drive"))
        .and_then(|drive_path| objects.get(&*drive_path))
        .and_then(|drive_interfaces| interface(drive_interfaces, DRIVE_INTERFACE))
        .and_then(|drive_properties| property_bool(drive_properties, "Removable"))
        .unwrap_or(false);

    Some(Mount {
        label,
        mount_point,
        removable,
    })
}

/// A mount point's own last path segment, for filesystems that report no
/// `IdLabel` (common for e.g. Btrfs subvolumes mounted by UUID). Falls
/// back to the whole mount point only in the degenerate case of a root-
/// level mount (`/mnt`, unlikely but not impossible).
fn fallback_label(mount_point: &std::path::Path) -> String {
    mount_point
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| mount_point.to_string_lossy().into_owned())
}

/// One interface's property map out of an object's interface map, by
/// name — identical technique to `bluetooth.rs::interface` (a linear scan
/// rather than a `HashMap::get`, since the keys are `OwnedInterfaceName`
/// and an object here has under a dozen interfaces).
fn interface<'a>(
    interfaces: &'a Interfaces,
    name: &str,
) -> Option<&'a HashMap<String, OwnedValue>> {
    interfaces
        .iter()
        .find(|(interface_name, _)| interface_name.as_str() == name)
        .map(|(_, properties)| properties)
}

fn property_bool(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties.get(name)?.downcast_ref::<bool>().ok()
}

fn property_str<'a>(properties: &'a HashMap<String, OwnedValue>, name: &str) -> Option<&'a str> {
    properties.get(name)?.downcast_ref::<&str>().ok()
}

/// `Block.Drive`'s value (D-Bus type `o`, an object path) → an owned copy,
/// so it can outlive the borrow of `properties` long enough to index back
/// into `objects` for the drive's own interfaces.
fn property_object_path(
    properties: &HashMap<String, OwnedValue>,
    name: &str,
) -> Option<zbus::zvariant::OwnedObjectPath> {
    properties
        .get(name)?
        .downcast_ref::<ObjectPath>()
        .ok()
        .map(zbus::zvariant::OwnedObjectPath::from)
}

/// `Filesystem.MountPoints`' value (D-Bus type `aay`: an array of
/// NUL-terminated byte-string paths) → the first mount point, as a real
/// `PathBuf`. Only the first is used — a filesystem bind-mounted in
/// several places at once is rare enough that showing just one entry for
/// it is an acceptable simplification for a v0.1 sidebar. `None` covers
/// both "no `MountPoints` property" (this object isn't actually mounted —
/// udisks still lists an unmounted filesystem's `Filesystem` interface,
/// just with an empty array) and "empty array".
fn first_mount_point(properties: &HashMap<String, OwnedValue>) -> Option<PathBuf> {
    let outer: Array = properties
        .get("MountPoints")?
        .downcast_ref::<Array>()
        .ok()?;
    let first: &Value = outer.first()?;
    let inner: Array = first.downcast_ref::<Array>().ok()?;
    let mut bytes: Vec<u8> = Vec::<u8>::try_from(inner).ok()?;
    // Each path udisks reports is a NUL-terminated C string; trim the
    // trailing NUL(s) so it doesn't end up embedded in the `OsString`.
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{OwnedObjectPath, Value};

    fn mount(label: &str, mount_point: &str, removable: bool) -> Mount {
        Mount {
            label: label.to_owned(),
            mount_point: PathBuf::from(mount_point),
            removable,
        }
    }

    // ── FakeMountsSource: the fake-driven add/remove contract ───────────

    #[tokio::test]
    async fn a_fake_source_streams_a_mount_appearing_then_disappearing() {
        let usb = mount("USB Drive", "/media/jordan/USB Drive", true);
        let source = FakeMountsSource::new(vec![vec![], vec![usb.clone()], vec![]]);

        let mut stream = source.watch();
        assert_eq!(stream.next().await, Some(vec![]));
        assert_eq!(stream.next().await, Some(vec![usb]));
        assert_eq!(stream.next().await, Some(vec![]));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn a_fake_source_with_no_scripted_snapshots_ends_immediately() {
        // The "no udisks on the bus" contract: an empty/ended stream, no
        // panic, no special-cased "absent" value.
        let source = FakeMountsSource::new(vec![]);
        assert_eq!(source.watch().next().await, None);
    }

    // ── `snapshot`: the D-Bus tree → mount-list mapping ──────────────────

    /// Builds one entry of a `GetManagedObjects` reply — same technique as
    /// `bluetooth.rs`'s test helper of the same shape.
    fn object(
        path: &str,
        interfaces: &[(&str, &[(&str, Value<'static>)])],
    ) -> (OwnedObjectPath, Interfaces) {
        let interfaces = interfaces
            .iter()
            .map(|(name, properties)| {
                let properties = properties
                    .iter()
                    .map(|(key, value)| {
                        (
                            (*key).to_string(),
                            OwnedValue::try_from(value.try_clone().expect("cloneable fixture"))
                                .expect("fixture value converts"),
                        )
                    })
                    .collect();
                (
                    OwnedInterfaceName::try_from(*name).expect("valid interface name"),
                    properties,
                )
            })
            .collect();
        (
            OwnedObjectPath::try_from(path).expect("valid object path"),
            interfaces,
        )
    }

    fn mount_points_value(path: &str) -> Value<'static> {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        Value::from(Array::from(vec![Value::from(Array::from(bytes))]))
    }

    #[test]
    fn a_mounted_labeled_filesystem_on_a_removable_drive_is_reported() {
        let objects = ManagedObjects::from_iter([
            object(
                "/org/freedesktop/UDisks2/drives/USB_Drive",
                &[(
                    "org.freedesktop.UDisks2.Drive",
                    &[("Removable", Value::from(true))],
                )],
            ),
            object(
                "/org/freedesktop/UDisks2/block_devices/sdb1",
                &[
                    (
                        "org.freedesktop.UDisks2.Block",
                        &[
                            ("IdLabel", Value::from("USB Drive")),
                            (
                                "Drive",
                                Value::from(
                                    ObjectPath::try_from(
                                        "/org/freedesktop/UDisks2/drives/USB_Drive",
                                    )
                                    .unwrap(),
                                ),
                            ),
                        ],
                    ),
                    (
                        "org.freedesktop.UDisks2.Filesystem",
                        &[("MountPoints", mount_points_value("/media/jordan/USB Drive"))],
                    ),
                ],
            ),
        ]);

        let mounts = snapshot(&objects);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].label, "USB Drive");
        assert_eq!(
            mounts[0].mount_point,
            PathBuf::from("/media/jordan/USB Drive")
        );
        assert!(mounts[0].removable);
    }

    #[test]
    fn an_unmounted_filesystem_is_not_reported() {
        let objects = ManagedObjects::from_iter([object(
            "/org/freedesktop/UDisks2/block_devices/sdb1",
            &[(
                "org.freedesktop.UDisks2.Filesystem",
                &[("MountPoints", Value::from(Array::from(Vec::<Value>::new())))],
            )],
        )]);
        assert!(snapshot(&objects).is_empty());
    }

    #[test]
    fn a_non_filesystem_object_is_not_reported() {
        let objects = ManagedObjects::from_iter([object(
            "/org/freedesktop/UDisks2/drives/USB_Drive",
            &[(
                "org.freedesktop.UDisks2.Drive",
                &[("Removable", Value::from(true))],
            )],
        )]);
        assert!(snapshot(&objects).is_empty());
    }

    #[test]
    fn hint_ignore_hides_a_mounted_filesystem() {
        let objects = ManagedObjects::from_iter([object(
            "/org/freedesktop/UDisks2/block_devices/loop0",
            &[
                (
                    "org.freedesktop.UDisks2.Block",
                    &[("HintIgnore", Value::from(true))],
                ),
                (
                    "org.freedesktop.UDisks2.Filesystem",
                    &[("MountPoints", mount_points_value("/var/lib/snapd/snap"))],
                ),
            ],
        )]);
        assert!(snapshot(&objects).is_empty());
    }

    #[test]
    fn a_missing_label_falls_back_to_the_mount_point_basename() {
        let objects = ManagedObjects::from_iter([object(
            "/org/freedesktop/UDisks2/block_devices/sda1",
            &[(
                "org.freedesktop.UDisks2.Filesystem",
                &[("MountPoints", mount_points_value("/mnt/backup"))],
            )],
        )]);
        let mounts = snapshot(&objects);
        assert_eq!(mounts[0].label, "backup");
        // No Drive property at all: conservatively not removable.
        assert!(!mounts[0].removable);
    }

    #[test]
    fn an_internal_drive_is_not_removable() {
        let objects = ManagedObjects::from_iter([
            object(
                "/org/freedesktop/UDisks2/drives/Internal",
                &[(
                    "org.freedesktop.UDisks2.Drive",
                    &[("Removable", Value::from(false))],
                )],
            ),
            object(
                "/org/freedesktop/UDisks2/block_devices/nvme0n1p2",
                &[
                    (
                        "org.freedesktop.UDisks2.Block",
                        &[(
                            "Drive",
                            Value::from(
                                ObjectPath::try_from("/org/freedesktop/UDisks2/drives/Internal")
                                    .unwrap(),
                            ),
                        )],
                    ),
                    (
                        "org.freedesktop.UDisks2.Filesystem",
                        &[("MountPoints", mount_points_value("/data"))],
                    ),
                ],
            ),
        ]);
        assert!(!snapshot(&objects)[0].removable);
    }

    #[test]
    fn mounts_are_sorted_alphabetically_by_label() {
        let objects = ManagedObjects::from_iter([
            object(
                "/org/freedesktop/UDisks2/block_devices/sdb1",
                &[(
                    "org.freedesktop.UDisks2.Filesystem",
                    &[("MountPoints", mount_points_value("/media/zeta"))],
                )],
            ),
            object(
                "/org/freedesktop/UDisks2/block_devices/sdc1",
                &[(
                    "org.freedesktop.UDisks2.Filesystem",
                    &[("MountPoints", mount_points_value("/media/alpha"))],
                )],
            ),
        ]);
        let mounts = snapshot(&objects);
        let labels: Vec<&str> = mounts.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "zeta"]);
    }

    #[test]
    fn an_empty_tree_yields_no_mounts() {
        assert!(snapshot(&ManagedObjects::new()).is_empty());
    }

    #[test]
    fn rebuilding_the_same_tree_produces_an_equal_snapshot() {
        let objects = ManagedObjects::from_iter([object(
            "/org/freedesktop/UDisks2/block_devices/sdb1",
            &[(
                "org.freedesktop.UDisks2.Filesystem",
                &[("MountPoints", mount_points_value("/media/usb"))],
            )],
        )]);
        assert_eq!(snapshot(&objects), snapshot(&objects));
    }
}
