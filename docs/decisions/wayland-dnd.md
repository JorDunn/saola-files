# Decision: Wayland drag-and-drop beside iced 0.14

-   **Date:** 2026-11-20
-   **Issue:** #52 (spike). Feeds #53 (fork contingency), #54 (D2, in-app
    drag), #55 (D3, drag to and from other apps) and #56 (modifiers and
    drop menu).
-   **Status:** Recommended. Protocol mechanics proven against the running
    niri session without a window; the end-to-end drag with real pointer
    input is not yet proven (see [What is not proven](#what-is-not-proven)).

## Decision

Add a small **Wayland DnD bridge** in `src/integration/`. It attaches to the
`wl_display` that winit already opened, using
`wayland_backend::client::Backend::from_foreign_display`. It creates its
own event queue and binds its own `wl_seat`, `wl_pointer` and
`wl_data_device` on that queue. It runs on one dedicated thread, and talks
to the app only through bounded channels. This is the same technique iced
0.14 already uses in this process for its own clipboard
(`smithay-clipboard`).

**No fork of iced or winit is needed.** #53 can be closed as not needed.

One rule needs a decision from the maintainer, but it does not affect #53.
The bridge has to name `wayland-backend` with its `client_system` feature,
which is the libwayland-client binding. This library is already loaded in
the process: winit and iced's clipboard use it, and winit opens it with
`dlopen`. No new crate and no new system library enter the build. But a
direct dependency on a C-library binding goes against the "pure Rust only"
posture in `docs/external-crates/README.md`. A fork would not avoid this,
because winit's own Wayland connection is a libwayland connection, and
anything that shares it must use libwayland too. See
[Pure-Rust rule](#pure-rust-rule).

## What iced 0.14 and winit 0.30 give us today

Versions from `Cargo.lock`: iced 0.14.0, iced_winit 0.14.0, winit 0.30.13,
smithay-client-toolkit 0.19.2 (winit) and 0.20.0 (smithay-clipboard),
wayland-client 0.31.15, wayland-backend 0.3.16, raw-window-handle 0.6.2.
All the sources cited below are from `~/.cargo/registry/src/` at these
versions.

-   **Drop target: nothing on Wayland.** winit has
    `WindowEvent::DroppedFile`, `HoveredFile` and `HoveredFileCancelled`
    (`winit-0.30.13/src/event.rs:180-192`). iced maps them to
    `window::Event::FileDropped` and `FileHovered`
    (`iced_winit-0.14.0/src/conversion.rs:334-340`). winit's Wayland
    backend never sends these events: `src/platform_impl/linux/wayland/`
    has no `data_device` code at all. Only X11, Windows and macOS send
    them.
-   **Drag source: nothing on any platform.** No winit 0.30 or iced 0.14
    API starts an OS drag.
-   **No pointer serial.** winit stores the latest button serial in
    `WinitPointerDataInner::latest_button_serial`
    (`seat/pointer/mod.rs:159`, `:349`). That type is in the private
    `platform_impl` module. The public `platform::wayland` extension
    traits only give `is_wayland`, `xdg_toplevel`, `with_name` and
    `native_id` (`src/platform/wayland.rs`).
-   **Raw handles are available.** `iced::window::run(id, f)` gives a
    `&dyn Window`, which implements `HasWindowHandle` and
    `HasDisplayHandle` (`iced_runtime-0.14.0/src/window.rs:149`, `:196`,
    `:463-478`; implemented in `iced_winit-0.14.0/src/window.rs:307`,
    `:322`). On Wayland these give the `wl_display*` and the window's
    `wl_surface*` (`raw-window-handle-0.6.2/src/unix.rs:158`, `:186`).
-   **Precedent in this process.** iced connects its clipboard by passing
    the raw display pointer to `window_clipboard`
    (`iced_winit-0.14.0/src/clipboard.rs:28-35`), and from there to
    `clipboard_wayland` and to `smithay-clipboard`.
    `smithay_clipboard::Clipboard::new` calls
    `Backend::from_foreign_display` and starts a worker thread with its
    own queue (`smithay-clipboard-0.7.3/src/lib.rs:34-44`, `worker.rs`).
    That worker binds its own `wl_seat`, `wl_keyboard`, `wl_pointer` and
    `wl_data_device` (`state.rs:274-330`). It reads serials from its own
    pointer's `Press` events (`state.rs:357-385`) for `set_selection`. Its
    `DataDeviceHandler` ignores `enter`, `motion` and `drop`
    (`state.rs:391-410`). So the running app already has two
    `wl_data_device` objects for the seat: one from iced's clipboard, and
    the bridge would add a second.

## Can the bridge share winit's display?

Yes. Each thing the bridge needs, and where it comes from:

| Needs                           | Source                                         |
| ------------------------------- | ---------------------------------------------- |
| A connection on winit's socket  | `Backend::from_foreign_display(display)` (`wayland-backend-0.3.16/src/sys/mod.rs:102`). It does not own the display; dropping it never closes winit's socket. |
| Its own event queue             | `registry_queue_init` on that connection. libwayland's `prepare_read`/`read_events` protocol makes two readers on one fd safe (winit's loop and ours). |
| The window's `wl_surface`       | `ObjectId::from_ptr(WlSurface::interface(), ptr)` (`sys/mod.rs:57`; `sys/client_impl/mod.rs:99-142`). winit created the surface through the same wayland-backend crate, so `from_ptr` sees it as rust-managed and shares its liveness flag. Its events still go to winit's queue; we only pass it as the `origin` argument. |
| A `wl_seat`                     | Our own binding of the seat global. On the server this is the same seat. |
| The press serial for `start_drag` | Our own `wl_pointer`. See the next section. |
| Drop-target events              | Our own `wl_data_device`. See the next section. |

### Serials from a second `wl_pointer`

The compositor checks `start_drag` against the seat's current pointer grab.
It does not check which `wl_pointer` object the serial was sent to. From
smithay at checkout `ff5fa7d` (the copy in `~/.cargo/git/checkouts/`; niri
is built on smithay, but the exact revision niri 26.04 uses was not
checked):

-   `wl_data_device.start_drag` accepts the request only if
    `pointer.has_grab(serial)` (`src/wayland/selection/data_device/device.rs:64-113`).
    It does not check that the `origin` surface belongs to the grab.
-   `has_grab` compares the serial with the active grab's start serial
    (`src/input/pointer/mod.rs:187-193`).
-   A button press starts a `ClickGrab` with the press event's serial
    (`src/input/pointer/grab.rs:238-247`).
-   The press is sent to **every** `wl_pointer` of the focused client, all
    with the same serial (`src/wayland/seat/pointer.rs:107-110`, `:222-233`).

So the serial our pointer receives is the serial winit receives, and it is
valid for `start_drag` while the button is held.

During the click grab, motion keeps going to the pressed surface even
outside the window (`grab.rs:356-365`). winit reports any motion as
`CursorMoved` and does not check bounds (`seat/pointer/mod.rs:150-153`).
So iced keeps receiving cursor positions after the pointer leaves the
window, while the button is held.

### Drop-target events on a second `wl_data_device`

When a drag enters one of our surfaces, smithay sends `enter`, `motion`,
`leave` and `drop` to **every** data device of that client. It creates one
`wl_data_offer` per device, and all of them share one state object
(`src/wayland/selection/data_device/mod.rs:420-530`). `accept`,
`set_actions` and `finish` on any of these offers update the shared state
(`mod.rs:240-320`). iced's clipboard device never calls any of them. So our
device's calls decide the outcome, and the other device's offer does no
harm.

One known edge: the clipboard's sctk device destroys its old drag offer
when the next drag enters (`smithay-client-toolkit-0.20.0/src/data_device_manager/data_device.rs:136-141`).
If that happens after a drop but before we call `finish`, smithay cancels
the source (`mod.rs:284-289`). This needs a second drag to enter the window
while the first drop's data is still being read. It is unlikely, and its
only effect is a cancelled drop.

### What winit sees during a drag

When a DnD grab starts, smithay clears pointer focus
(`src/input/dnd/grab.rs:344-351`, `ptr_focus` returns `None` at
`:574-576`). The final button release is consumed by the DnD grab
(`:368-375`). As a result:

-   winit gets `wl_pointer.leave`, and iced gets `CursorLeft`.
-   iced **never** gets the `ButtonReleased` for the press that started the
    drag. Any "button is held" state in the app or in an iced widget must
    be reset from the bridge's `Finished`/`Cancelled` events, or on the
    next `CursorEntered`.
-   While a drag is over our own window, the only positions we get are the
    bridge's `DragMotion` events, not iced's `CursorMoved`.

## Prototype

A standalone crate, outside this repository, at
`dnd-spike/` in the spike session's scratchpad. The absolute path is
machine-specific, so it was handed to the orchestrator and is not recorded
here.
This is a session scratch directory, so it may be cleared. The parts that
matter are described here.

-   `src/lib.rs` is the bridge, about 600 lines. `unsafe fn spawn(RawHandles,
    mpsc::Sender<Event>)` attaches to the foreign display and recovers the
    window surface. It then starts a `saola-dnd` thread that runs a calloop
    loop with `WaylandSource` and a command channel. The thread binds the
    data device manager, seat, pointer, `wl_shm` and `wl_compositor` (the
    last two for the drag icon surface). It records the press serial (only
    for presses on the window surface; cleared on release or leave). On
    `StartDrag` it creates a `text/uri-list` source and calls
    `start_drag(origin = window surface, icon, serial)`. As a target, it
    accepts `text/uri-list` and sets `Copy | Move` with `Copy` preferred.
    It reads the drop pipe without blocking through calloop, then calls
    `finish`. A drop from our own drag skips the pipe. As a source, it
    writes the payload without blocking when asked.
-   `src/bin/app.rs` is the iced 0.14 wiring. `window::latest()` and
    `window::run(id, extract)` copy the two pointers out of iced's window.
    Then `Subscription::run_with(handles, bridge)` starts the bridge and
    forwards events as messages. Its first message hands the command
    sender back to `update`. A press arms a drag, and a 6 px movement
    while held sends `StartDrag`. It prints every bridge event to stdout.
-   `src/bin/probe.rs` is a headless protocol probe. It opens a plain
    libwayland connection, as winit would, and creates a `wl_surface` with
    no role and no buffer, so nothing appears on screen. It attaches the
    bridge to that display and surface, then:
    1.  waits for `Ready`;
    2.  sends `StartDrag` with no button held, and expects the bridge to
        refuse it locally;
    3.  sends a forced `start_drag` with a stale serial, then a
        `wl_display.sync`, and expects the sync to complete;
    4.  after the bridge exits, does a round trip on the owner connection.

### Results

-   **Builds** with `cargo build` and `cargo clippy --all-targets`, with no
    warnings. The build used a copy of saola-files' `Cargo.lock`. Every
    package in the prototype's resolved lock is already in saola-files'
    lock at the same version, except the prototype crate itself. Adopting
    this adds **no new crates**.
-   **Probe against niri 26.04** (run four times, same result each time):

    ```text
    event: Ready { data_device_manager_version: 3, seat_has_pointer: true, window_surface_protocol_id: 4 }
    event: StartRefused("no button held on the window")
    event: StartSent { serial: 1 }
    event: Synced
    PASS connection alive after start_drag with stale serial
    PASS owner connection healthy after bridge exit
    ```

    Wire log (`WAYLAND_DEBUG=1`, trimmed). The owner connection created
    `wl_surface#4`, and the bridge's queue then used it as `start_drag`'s
    origin:

    ```text
    -> wl_compositor#3.create_surface(new id wl_surface#4)
    wl_seat#7.capabilities(3)
    -> wl_data_device_manager#6.get_data_device(new id wl_data_device#10, wl_seat#7)
    -> wl_seat#7.get_pointer(new id wl_pointer#11)
    -> wl_data_device_manager#6.create_data_source(new id wl_data_source#12)
    -> wl_data_device#10.start_drag(wl_data_source#12, wl_surface#4, nil, 1)
    -> wl_display#1.sync(new id wl_callback#13)
    wl_callback#13.done(0)
    ```

-   **A bug the probe caught:** on the first runs, `Ready` sometimes
    arrived before the seat's capabilities, so there was no pointer yet.
    The fix is one blocking `roundtrip` on our queue before the queue goes
    into the event loop. The product code must keep this step.

### What is proven

-   We can attach to a display that another libwayland user opened, run a
    second queue on it, and bind the data device manager (v3 on niri), a
    seat with a pointer, a data device and a pointer, without disturbing
    the owner connection.
-   A `wl_surface` created by the owner can be recovered by pointer and
    used as a request argument from our queue. The wire log shows niri
    received it, and no protocol error followed.
-   The bridge refuses to start a drag when no button is held, before
    anything reaches the wire.
-   The iced 0.14 wiring compiles: handle extraction, the `run_with`
    subscription, and the command and event channels.
-   No new crates.

### What is not proven

These need real pointer input on a mapped window, which this spike was not
allowed to do:

1.  niri actually starts the drag when we send our pointer's press serial
    (the source analysis says it will, but niri's smithay revision was not
    checked).
2.  The `e.surface == window_surface` filter matches winit's surface for
    pointer events on our queue. It should, because both proxies wrap the
    same `wl_proxy`.
3.  The drag icon surface (shm buffer) is drawn under the pointer.
4.  Drops from another client reach our data device, and the
    `text/uri-list` body arrives.
5.  Other apps (a browser upload field, a GTK or Qt file manager) accept
    our `text/uri-list` drag.
6.  iced's behaviour after a drag: does an iced `button` stay drawn as
    pressed because it never saw `ButtonReleased`? Does `CursorEntered`
    come back cleanly after the drop?
7.  `CursorMoved` outside the window bounds during the click grab (needed
    by the escalation rule below).

### Live test for the orchestrator

Use the AGENTS.md live-GUI recipe, with two instances of the prototype app
as source and target. Same app-id twice is expected here; it is not a stale
instance.

If the scratch directory has been cleared, rebuild the prototype from the
description above first. `SCRATCH` is any directory under the session
scratchpad, never the repo.

```bash
D=<path to dnd-spike>   # from the orchestrator's notes
SCRATCH=<session scratchpad>/dnd-live
mkdir -p "$SCRATCH/dnd" && echo hello > "$SCRATCH/dnd/a.txt"
CARGO_TARGET_DIR=$D/target cargo build --manifest-path $D/Cargo.toml --bin app
$D/target/debug/app "$SCRATCH/dnd/a.txt" > "$SCRATCH/src.log" &   # source
$D/target/debug/app /unused > "$SCRATCH/dst.log" &                # target
```

1.  Tile the two windows side by side (`niri msg --json windows`; do not
    fullscreen them). Take a screenshot.
2.  Move the pointer onto "drag me" in the source window. Press and hold
    with `ydotool click 0x40`, move about 20 px inside the window, then
    move into the target window in a few steps
    (`ydotool mousemove --absolute ...`). Take a screenshot while the
    button is held: the 32 px terracotta tile should follow the pointer.
3.  Release with `ydotool click 0x80`.
4.  Expected `src.log`: `PointerPress`, `threshold crossed`, `StartSent`,
    then `SourceFinished { action: Copy }`.
    Expected `dst.log`: `DragEnter { offers_uri_list: true, from_self: false }`,
    `DragMotion`, then `Dropped` with body `"file:///…/a.txt\r\n"`.
5.  Repeat, dropping onto a browser's file-upload field, if one is
    running. The field should show `a.txt`.
6.  Repeat, ending the drag over the source window itself. Expected:
    `Dropped { uri_list: None }` (own drag, no pipe).
7.  After each drag, click the source window once and check that
    `PointerPress` is logged again. This shows the app is not stuck in a
    held-button state.
8.  Kill both instances.

Results 1 to 5 settle unproven items 1 to 5. Steps 6 and 7 cover item 6.

## Recommended architecture

### Layers and files

-   `src/integration/wayland_dnd.rs` is the bridge, the prototype's
    `lib.rs` made production-grade. It holds the only `unsafe` code (two
    calls, each with a `// SAFETY:` note) and is the only file that knows
    about Wayland objects. It exposes plain enums (`Command`, `Event`) and
    `spawn`. The rest of the app never sees Wayland types.
-   `src/core/uri_list.rs` is pure: `text/uri-list` encoding and decoding,
    following RFC 2483 (CRLF lines, `#` comment lines ignored), with
    byte-level percent-encoding to and from `OsString`. It has unit tests,
    including non-UTF-8 paths. `integration/dbus.rs` already has
    `encode_file_uri`/`decode_file_uri`. D3 should move those into this
    core module and have both callers use it, instead of writing a second
    copy.
-   `src/core/` gets the pure drop rules (#54, #56): move or copy by
    filesystem or backend, refusing a drop onto itself or a descendant,
    and the modifier-to-action table.
-   `src/ui/dirview/pointer.rs` is the drag-source seam. A press on a
    selected row arms a drag. A move past the threshold while held emits
    `dirview::Event::DragStarted { items }`. A pure
    `DirectoryView::drop_target_at(Point) -> Option<DropTarget>` does the
    hit-testing for both iced and bridge coordinates. `ui/` does not import
    `integration/`.
-   `src/app/dnd.rs` holds the App-level `DragState`: the source view, the
    items, the current target, and whether the drag is internal or
    Wayland. It also holds the bridge's command sender,
    `dnd_subscription()` (one line in `app/subscription.rs`, like D-Bus),
    and the one function that turns a finished drop into an ops-queue job.

### Message flow

-   **In-app drag (D2, #54).** This is iced only, with no Wayland. iced
    keeps receiving `CursorMoved` during the click grab, so `DragState`
    follows the pointer, highlights the target, and on `ButtonReleased`
    calls the drop function. This works when the bridge is unavailable
    (X11, no data device manager), and it can be tested with unit tests
    plus the live recipe.
-   **Leaving the window (D3, #55).** When a held drag's cursor goes
    outside the window bounds, `App` sends `Command::StartDrag { uri_list,
    allow_move, icon }`. The press serial is still valid, because the
    button is held and the click grab is active. From then on the
    compositor owns the pointer. The bridge's `SourceFinished { action }`
    tells us what the target did. For `Move`, D3 removes nothing in v1: a
    cross-app move is done as a copy, worded as such (see Risks).
-   **Coming back, or a drop from another app (D3).** The bridge's
    `DragEnter`, `DragMotion` and `Dropped` events feed the same
    `DragState` and `drop_target_at`. `Dropped { uri_list: None }` means
    our own drag came back, and the app uses its own item list.
    Otherwise the body goes through `core::uri_list::decode`. Malformed
    entries are skipped and reported with a worded notice.
-   **Per-target acceptance.** The prototype accepts for the whole window.
    The product should accept or reject per target: on each `DragMotion`,
    `App` hit-tests and sends `Command::SetAccept { accept, actions }`, and
    the bridge calls `accept`/`set_actions` on the live offer. This is one
    round trip through the app per motion event, which is cheap and keeps
    all logic in the app.

### Threading and the AGENTS.md rules

-   **One extra OS thread, not a second async runtime.** It runs a calloop
    loop that waits in `epoll` on the Wayland fd and on the command
    channel's eventfd. It wakes only when there is work, so it signals
    rather than polls. iced's clipboard already runs exactly this kind of
    thread in this process. A tokio task with `AsyncFd` was considered and
    rejected (see Alternatives).
-   **Commands in:** `calloop::channel::sync_channel(8)` (bounded), sent
    with `try_send`. The prototype used the unbounded `channel`; the
    product must not.
-   **Events out:** `iced::futures::channel::mpsc` (bounded), as AGENTS.md
    requires. Under backpressure, motion events may be merged into the
    latest one. Terminal events (`Dropped`, `DragLeave`,
    `SourceFinished`, `SourceCancelled`) must never be dropped. Send them
    with `futures::executor::block_on(sender.send(..))` on the bridge
    thread. This is safe, because calloop dispatches callbacks after
    `read_events` has finished, so winit's reader is never left waiting.
-   **No panics.** Every failure (no data device manager, the surface not
    recognised, the thread failing to spawn) becomes
    `Event::Unavailable(..)` or `None`. The UI then words cross-app drag
    as unavailable, and in-app drag still works. sctk 0.20 has internal
    `unwrap`s on its own mutexes. iced's clipboard already has the same
    exposure.
-   **Lifetime.** The raw pointers are valid while iced's window exists.
    `App` sends `Command::Exit` when it sees the window-close event, and
    joins the thread before returning from the close handling.

### Drag icon and feedback

-   **Inside the window,** iced draws everything: drop-target highlight,
    and optionally a ghost of the dragged items near the cursor. Any new
    highlight style is a saola-theme request, not local styling.
-   **Outside the window,** iced cannot draw into another surface. The
    bridge shows a separate `wl_surface` with an shm buffer as the
    compositor's drag icon (proven to build; not yet seen on screen). The
    pixels come from a pure `ui/` function that rasterises the Lucide
    glyph for the item's type with `resvg`. resvg 0.45.1 is already in the
    lock through iced's `svg` feature, so this adds a direct dependency
    and needs its survey comment. Colours come from `saola_theme::tokens`.
    The tile's look (size, border, a count for several items) should be a
    theme request. Set `wl_surface.set_buffer_scale` from the window's
    scale factor, so the icon is sharp on HiDPI.
-   **Coordinates.** `wl_data_device` motion is in surface-local logical
    pixels, the same space as iced's logical coordinates when the iced
    scale factor is 1. If the app ever sets its own scale factor, the
    bridge positions must be divided by it.

### MIME types and backends

-   **Offer `text/uri-list` only** for outgoing drags.
    `x-special/gnome-copied-files` is a clipboard convention (it carries
    the cut/copy verb), and a drag carries its action through
    `set_actions` instead. Leave it out unless live test step 5 finds a
    target that needs it. This was not checked against Nautilus or
    Dolphin.
-   **Outgoing drags carry local items only** (items whose backend has
    `LOCAL_PATH`). An `sftp://` URI is useless to almost every target, and
    downloading on demand cannot fit in `send_request`, which must answer
    the pipe promptly. A drag of remote items stays in-app, and leaving the
    window is worded: "Dragging to other apps works for local files only."
-   **Incoming:** accept `text/uri-list` and copy into the target folder
    through the ops queue. A `file://` URI with a non-empty host other than
    `localhost` is rejected with wording.
-   **Sandboxed (Flatpak) targets** may need the document portal's
    `application/vnd.portal.filetransfer` type to see host files. This is
    out of scope for v1; note it as a known gap.

### Notes for #56 (modifiers)

Wayland gives a drop target no keyboard modifiers, and the target window
usually does not have keyboard focus during a drag. Modifier-chosen actions
are therefore reliable only for in-app (iced) drags. For cross-app drags
the source offers actions, the target picks one with `set_actions`, and the
compositor chooses between them. For incoming drops, use the drop menu
(`Ask`) when the user wants a choice.

## Pure-Rust rule

The bridge must depend directly on `wayland-backend` with
`features = ["client_system"]`, because only the libwayland backend has
`from_foreign_display`. Facts for the maintainer's call:

-   libwayland-client is already loaded in this process. winit enables
    `wayland-dlopen` (`winit-0.30.13/Cargo.toml:117`), and `cargo tree -e
    features -i wayland-sys` in this repo shows `client`, `dlopen` and
    `once_cell` already on, through winit and smithay-clipboard.
-   Feature unification means **every** `wayland-client` in this binary,
    including the one under `wl-clipboard-rs`, already uses libwayland.
    The bridge changes nothing at runtime.
-   No in-process Wayland DnD can avoid this, forked or not. The drag must
    use winit's connection, and winit's connection is libwayland.

Suggested wording for the rule: "No new C libraries. Binding the
libwayland-client that the toolkit already loads is allowed only in
`integration/`." If that is not acceptable, cross-app DnD (D3) is out, and
D2 ships as iced-only in-app drag.

## Alternatives considered

1.  **Wait for upstream.** winit merged a cross-platform drag-and-drop API
    with Wayland source and target support on 2026-07-16
    (rust-windowing/winit#4571). It is in the 0.31 betas, not in 0.30.13.
    iced master is at 0.15.0-dev and pins winit to a git revision of the
    `iced-rs/winit` fork. Whether that fork has the new API, and when iced
    would expose it, is not known. This loses as a plan, because it blocks
    M6 for an unknown time. It is the **exit path**: once iced exposes
    DnD, replace `integration/wayland_dnd.rs` behind the same
    `Command`/`Event` seam.
2.  **Fork iced and/or winit** to expose the serial and the data device,
    or to carry winit 0.31's DnD. This would work, but loses on cost. It
    needs a fork rebase on every iced or winit release, and a `[patch]`
    pin. Every Saola surface that shares iced 0.14 from crates.io
    (saola-theme and its siblings) would be built against a different
    winit than in the rest of the family. It also gains nothing the bridge
    lacks.
3.  **Adopt the pop-os (libcosmic) iced fork.** It is based on iced 0.14
    and has DnD through `pop-os/window_clipboard`'s `dnd` crate and a
    `pop-os/winit` fork (its `Cargo.toml`, checked 2026-11-20). This
    replaces the toolkit with a moving fork and orphans the saola-theme
    design system. Rejected for the same reasons as 2, only stronger.
4.  **A second, independent Wayland connection** (like `wl-clipboard-rs`).
    This cannot work. Pointer events and DnD `enter`/`drop` go only to the
    client that owns the surface under the pointer. A separate connection
    is a different client, so it never sees our window's press serial or
    drops onto our window.
5.  **`wlr-data-control` / `ext-data-control`** (what `wl-clipboard-rs`
    uses). These are clipboard-only protocols, with no drag-and-drop.
6.  **The bridge as a tokio task with `AsyncFd`** instead of a thread.
    This keeps everything on iced's runtime. It loses because the task
    must hold a libwayland "prepared read" across an `.await`. If the task
    is not scheduled promptly, winit's own `read_events` blocks behind it
    and the UI stalls. A dedicated thread is the proven pattern:
    smithay-clipboard's worker thread runs calloop with
    `calloop_wayland_source::WaylandSource`, the same socket reader winit's
    own loop uses
    (`winit-0.30.13/src/platform_impl/linux/wayland/event_loop/mod.rs:19`,
    `:106`).
    Worth revisiting only if a second Wayland integration appears and
    could share one thread.
7.  **Always use a Wayland drag, even inside the window** (the GTK model).
    One code path for feedback. It loses for D2: in-app drag would depend
    on the bridge and on the unproven items above, iced would lose cursor
    events for the whole drag, and it would not work without the bridge.
    It stays available as a policy switch: start the Wayland drag at the
    threshold instead of at the window edge.

## Risks

-   **Unproven until the live test.** If niri rejects our serial, the
    likely cause is a smithay or niri revision that checks the `origin`
    surface or the pointer resource. Fallback: D2 ships iced-only, D3
    takes alternative 1 (wait for upstream), and #53 is reopened only if
    the maintainer wants to fork rather than wait.
-   **Lost `ButtonReleased`.** After any Wayland drag, iced widgets may
    believe a button is still pressed. Mitigation: reset app drag and press
    state on `SourceFinished`, `SourceCancelled` and `CursorEntered`. Check
    iced widgets in live test step 7.
-   **A refused drag leaves a dead source.** smithay ignores a
    `start_drag` it refuses, so the source gets neither `cancelled` nor
    `dnd_finished` (this is what the probe's stale-serial step showed).
    The bridge must destroy the pending source when our pointer sees the
    button release without an intervening `leave`.
-   **Two data devices.** iced's clipboard device shares each offer's
    state (analysed above). There is only a rare cancellation race.
-   **Compositor coupling.** The serial and fan-out behaviour is verified
    in smithay, which niri uses. Other compositors are not a target.
-   **Cross-app move semantics.** A target that chooses `Move` expects the
    source to delete its files after `dnd_finished`. v1 should offer
    `Copy` only on outgoing drags (`allow_move: false`) until #56 decides
    otherwise. Then no file is ever deleted because of another app's
    choice.
-   **Upstream drift.** When iced exposes DnD, this bridge becomes dead
    weight. The narrow `Command`/`Event` seam keeps the replacement to one
    file.
