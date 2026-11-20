# saola-files — English (en-US) message catalogue.
#
# One message per user-visible string in src/ui/ and src/app/. Looked up with
# `tr!("id")` / `tr!("id", name = value)` (see src/i18n.rs). Rules:
#
# - Ids are kebab-case, prefixed by the surface that owns them. Strings that
#   several surfaces share use the `action-` / `status-` prefixes.
# - Filenames and paths are never written here; they arrive as arguments.
# - Plurals are selectors in the message, not branches in the code.
# - Wording is the user-facing contract: change it deliberately, not as a
#   side effect of a refactor.

## Window

window-title = Files

## Shared

action-cancel = Cancel
action-close = Close
status-loading = Loading…

## Header and breadcrumbs

header-hidden = Hidden
breadcrumb-path-placeholder = scheme://host/path or /a/local/path

## Sidebar

sidebar-removable = REMOVABLE
sidebar-connect = Connect to Server…

## Directory view

dirview-empty = This folder is empty
dirview-column-name = Name
dirview-column-size = Size
dirview-column-modified = Date modified
dirview-rename-placeholder = Name
dirview-rename-invalid = Enter a valid name

# The names a new folder / file is created with, before the user renames it.
dirview-new-folder-name = New Folder
dirview-new-file-name = New File

# A name that is already taken gets a counter: "New Folder (2)".
dirview-name-with-counter = { $base } ({ $n })

## Errors that surface in the UI

error-no-backend = no backend for scheme "{ $scheme }"
error-create-failed = creating { $location } failed
error-trash-internal = internal error talking to the trash: { $detail }

## Context menu

menu-open = Open
menu-open-with = Open with…
menu-open-in-terminal = Open in terminal
menu-copy = Copy
menu-cut = Cut
menu-paste = Paste
menu-rename = Rename…
menu-rename-many = Rename selected…
menu-move-to-trash = Move to Trash
menu-delete = Delete
menu-new-folder = New Folder
menu-new-file = New File
menu-properties = Properties
menu-folder-properties = Folder properties
menu-nothing-selected = Nothing selected
menu-no-known-app = No known app for this file type

## Conflict dialog

conflict-title = "{ $name }" already exists
conflict-body = { $dest } is already there. What should happen to { $source }?
conflict-keep-both = Keep both
conflict-overwrite = Overwrite
conflict-skip = Skip
conflict-apply-to-all = Apply to all conflicts in this operation

## Connect dialog

connect-title = Connect to Server
connect-connecting = Connecting…
connect-uri-placeholder = sftp://user@host/path
connect-enter-uri = Enter a server URI, e.g. sftp://user@host/path
connect-no-saved-servers = No saved servers yet — add a [[server]] entry to files.toml.
connect-saved-servers = SAVED SERVERS
connect-connect = Connect
connect-host-key-title = { $host }:{ $port } presented a new host key
connect-host-key-detail =
    { $key_type } { $fingerprint }
    This server has never been connected to before. Only trust it if you recognize this fingerprint.
connect-trust = Trust & Connect
connect-dont-trust = Don't Trust
connect-passphrase-for = Passphrase for { $path }
connect-passphrase-placeholder = Passphrase
connect-password-for = Password for { $user }
connect-password-placeholder = Password
connect-submit = Submit
connect-skip-key = Skip This Key

## Copy and move progress

progress-copying = Copying… { $done } of { $total }
progress-copying-named = Copying { $name }… { $done } of { $total }
progress-moving = Moving… { $done } of { $total }
progress-moving-named = Moving { $name }… { $done } of { $total }
progress-downloading = Downloading… { $done } of { $total }
progress-downloading-named = Downloading { $name }… { $done } of { $total }

# Reasons for an Overwrite on a server that can't replace a file in one
# step (src/core/fs/ops.rs, `place_over`). `$new` and `$old` are temporary
# file names in the destination folder.
copy-replace-stranded = the new copy couldn't be put in place; it is saved as “{ $new }”, and the file it was replacing as “{ $old }”
copy-replaced-file-kept = the copy is in place, but the file it replaced couldn't be removed and is still there as “{ $old }”

## Opening a file that has no local path (a downloaded read-only copy)

notice-opened-copy = Opened a read-only copy of “{ $name }” — changes won't be saved to the server.
notice-opened-copy-many =
    { $count ->
        [one] Opened a read-only copy of { $count } item — changes won't be saved to the server.
       *[other] Opened read-only copies of { $count } items — changes won't be saved to the server.
    }
open-busy = another copy or download is still running
open-cancelled = the download was cancelled
open-unsafe-name = its name can't be used as a file name here
open-terminal-operation = opening a terminal here
open-no-cache = there is no cache folder to keep the downloaded copy in
open-slot-not-private = the folder for downloaded copies is not private to this user
open-slot-no-name = a folder for the download could not be created
open-slot-location = the folder for downloaded copies
open-truncated = the download did not come out complete
open-no-space = there is not enough free space to download it
open-duplicate-name = another file with the same name is being opened with it
open-conflict = a file with that name was already downloaded

## Properties dialog

properties-calculating = Calculating…
properties-size-and-count =
    { $size } — { $count ->
        [one] { $count } item
       *[other] { $count } items
    }
properties-items-selected = { $count } items selected
properties-location = Location
properties-kind = Kind
properties-size = Size
properties-modified = Modified
properties-permissions = Permissions
properties-kind-symlink = Symlink
properties-kind-folder = Folder
properties-mode = { $symbolic } ({ $octal })

## Undo toast

undo-button = Undo
undo-bulk-rename =
    { $count ->
        [one] Renamed { $count } item
       *[other] Renamed { $count } items
    }
undo-bulk-rename-skipped =
    { $count ->
        [one] “{ $name }” has changed since it was renamed, so it was left as it is
       *[other] { $count } items have changed since they were renamed, so they were left as they are (starting with “{ $name }”)
    }

## Bulk rename dialog

bulk-rename-title =
    { $count ->
        [one] Rename { $count } item
       *[other] Rename { $count } items
    }
bulk-rename-mode-find = Find and replace
bulk-rename-mode-affix = Prefix and suffix
bulk-rename-mode-number = Numbering
bulk-rename-mode-case = Change case
bulk-rename-find-placeholder = Find
bulk-rename-replace-placeholder = Replace with
bulk-rename-regex = Regular expression
bulk-rename-match-case = Match case
bulk-rename-prefix-placeholder = Add before the name
bulk-rename-suffix-placeholder = Add after the name
bulk-rename-pattern-placeholder = Holiday {"{"}n:03{"}"}
bulk-rename-start-label = Start at
bulk-rename-number-hint = {"{"}n{"}"} is the number, {"{"}n:03{"}"} pads it to 3 digits, {"{"}name{"}"} is the old name.
bulk-rename-case-lower = lowercase
bulk-rename-case-upper = UPPERCASE
bulk-rename-case-title = Title Case
bulk-rename-keep-extension = Keep file extensions
bulk-rename-preview-label = Preview
bulk-rename-unchanged = no change
bulk-rename-apply =
    { $count ->
        [one] Rename { $count } item
       *[other] Rename { $count } items
    }
bulk-rename-status-nothing = Nothing to rename yet.
bulk-rename-status-ready =
    { $count ->
        [one] { $count } item will be renamed.
       *[other] { $count } items will be renamed.
    }
bulk-rename-problem-empty = “{ $name }” would have no name.
bulk-rename-problem-dot = “{ $name }” would be named “.” or “..”, which isn't allowed.
bulk-rename-problem-character = “{ $name }” would contain a “/” or a null character, which a name can't have.
bulk-rename-problem-long = “{ $name }” would be too long for a file name.
bulk-rename-problem-duplicate = More than one item would be named “{ $name }”.
bulk-rename-problem-exists = “{ $name }” already exists in this folder.
bulk-rename-problem-count =
    { $count ->
        [one] Nothing is renamed until this is fixed: { $reason }
       *[other] Nothing is renamed until these { $count } problems are fixed. First: { $reason }
    }
bulk-rename-error-regex = That isn't a valid regular expression: { $detail }.
bulk-rename-error-unknown-token = “{ $token }” isn't a known placeholder. Use {"{"}n{"}"}, {"{"}n:03{"}"} or {"{"}name{"}"}.
bulk-rename-error-unclosed = A “{"{"}” is missing its closing “{"}"}”.
bulk-rename-error-stray = A “{"}"}” has no opening “{"{"}”. Write “{"}}"}” for a literal one.
bulk-rename-error-width = “{ $width }” isn't a valid width. Use a number from 1 to 32.
bulk-rename-error-start = The starting number must be a whole number.
bulk-rename-cancel = Cancel
bulk-rename-preview-more =
    { $count ->
        [one] and { $count } more item not shown here (it is still checked and renamed)
       *[other] and { $count } more items not shown here (they are still checked and renamed)
    }
bulk-rename-regex-hint = In the replacement, ${"{"}name{"}"} or $1 inserts a matched group. Write ${"{"}1{"}"}abc when letters follow the number.
bulk-rename-parked = it is now stored as “{ $temp }”, a hidden file in the same folder
undo-bulk-rename-stuck = { $reason }; for “{ $name }”, { $parked }
bulk-rename-busy = another operation is still running; try again when it has finished
bulk-rename-operation = renaming items
bulk-rename-no-backend = nothing is connected for “{ $scheme }” locations

## Trash

trash-title = Trash
trash-empty-state = Trash is empty
trash-empty-button = Empty Trash
trash-confirm-empty = Permanently delete everything in the trash?
trash-restore = Restore

## Failures
##
## Sentences for operations that didn't work (src/core/failure.rs). Severity
## is carried by the wording alone: no colour. `{ $reason }` is one of the
## `failure-reason-*` phrases below.

failure-copy-to = Couldn't copy “{ $name }” to “{ $destination }”: { $reason }.
failure-copy = Couldn't copy “{ $name }”: { $reason }.
failure-move-to = Couldn't move “{ $name }” to “{ $destination }”: { $reason }.
failure-move = Couldn't move “{ $name }”: { $reason }.
failure-rename-to = Couldn't rename “{ $name }” to “{ $destination }”: { $reason }.
failure-rename = Couldn't rename “{ $name }”: { $reason }.
failure-trash = Couldn't move “{ $name }” to the trash: { $reason }.
failure-delete = Couldn't delete “{ $name }”: { $reason }.
failure-create-in = Couldn't create “{ $name }” in “{ $destination }”: { $reason }.
failure-create = Couldn't create “{ $name }”: { $reason }.
failure-open = Couldn't open “{ $name }”: { $reason }.
failure-open-with = Couldn't open “{ $name }” with { $destination }: { $reason }.
failure-open-terminal = Couldn't open a terminal in “{ $name }”: { $reason }.
failure-run-action = Couldn't run “{ $name }”: { $reason }.
failure-undo = Couldn't undo that: { $reason }.
failure-restore = Couldn't restore “{ $name }” from the trash: { $reason }.
failure-restore-unnamed = Couldn't restore an item from the trash: { $reason }.
failure-empty-trash = Couldn't empty the trash: { $reason }.
failure-clipboard = Couldn't share the copied items with other apps: { $reason }.

failure-reason-permission-denied = permission denied
failure-reason-not-found = it no longer exists
failure-reason-already-exists = something with that name already exists
failure-reason-no-space = there isn't enough space
failure-reason-cross-device = the folders are on different drives
failure-reason-read-only = the location is read-only
failure-reason-disconnected = the connection was lost
failure-reason-not-a-folder = it isn't a folder
failure-reason-is-a-folder = it is a folder
failure-reason-no-default-app = no app is set up to open this kind of file
failure-reason-app-uninstalled = that app is no longer installed
failure-reason-program-missing = the app couldn't be found
failure-reason-other = { $message }
failure-reason-unknown = something went wrong
failure-reason-unsupported = this location doesn't support { $operation }

failure-summary-copy =
    { $count ->
        [one] { $count } item couldn't be copied
       *[other] { $count } items couldn't be copied
    }
failure-summary-move =
    { $count ->
        [one] { $count } item couldn't be moved
       *[other] { $count } items couldn't be moved
    }
failure-summary-rename =
    { $count ->
        [one] { $count } item couldn't be renamed
       *[other] { $count } items couldn't be renamed
    }
failure-summary-trash =
    { $count ->
        [one] { $count } item couldn't be moved to the trash
       *[other] { $count } items couldn't be moved to the trash
    }
failure-summary-delete =
    { $count ->
        [one] { $count } item couldn't be deleted
       *[other] { $count } items couldn't be deleted
    }
failure-summary-create =
    { $count ->
        [one] { $count } item couldn't be created
       *[other] { $count } items couldn't be created
    }
failure-summary-open =
    { $count ->
        [one] { $count } item couldn't be opened
       *[other] { $count } items couldn't be opened
    }
failure-summary-restore =
    { $count ->
        [one] { $count } item couldn't be restored
       *[other] { $count } items couldn't be restored
    }
failure-summary-other =
    { $count ->
        [one] { $count } thing went wrong
       *[other] { $count } things went wrong
    }

## Error notice

error-notice-dismiss = Dismiss
error-notice-details = Details
error-notice-hide-details = Hide details
error-notice-more = …and { $count } more

## Status bar

## The window's bottom band: what the folder holds, what is selected, and the
## free space. Sizes are formatted by `core::fs::format::human_size`. A
## selection's size counts files only: a folder's size is not known without
## walking it, and the bar never does.

statusbar-items =
    { $count ->
        [0] No items
        [one] { $count } item
       *[other] { $count } items
    }
statusbar-items-hidden =
    { $count ->
        [one] { $count } item, { $hidden } hidden
       *[other] { $count } items, { $hidden } hidden
    }
statusbar-selected = { $count } of { $total } selected
statusbar-selected-size = { $count } of { $total } selected, { $size }
statusbar-selected-size-files = { $count } of { $total } selected, { $size } in files
statusbar-unavailable = Couldn't list this folder
statusbar-free = { $size } free
statusbar-trash-items =
    { $count ->
        [0] Trash is empty
        [one] { $count } item in the trash
       *[other] { $count } items in the trash
    }
statusbar-trash-unavailable = Couldn't read the trash

## Keyboard shortcuts
##
## Labels for the rows of `keymap::BINDINGS`, one per action, id `keys-` plus
## the action's name with `-` for `_`. Read by a shortcut editor, not shown
## in the app today.

keys-move-cursor-up = Move cursor up
keys-move-cursor-down = Move cursor down
keys-move-cursor-left = Move cursor left
keys-move-cursor-right = Move cursor right
keys-move-cursor-home = Move cursor to first item
keys-move-cursor-end = Move cursor to last item
keys-move-cursor-page-up = Move cursor up a page
keys-move-cursor-page-down = Move cursor down a page
keys-extend-selection-up = Extend selection up
keys-extend-selection-down = Extend selection down
keys-extend-selection-left = Extend selection left
keys-extend-selection-right = Extend selection right
keys-extend-selection-home = Extend selection to first item
keys-extend-selection-end = Extend selection to last item
keys-extend-selection-page-up = Extend selection up a page
keys-extend-selection-page-down = Extend selection down a page
keys-toggle-cursor-selected = Toggle selection of the cursor item
keys-select-all = Select all
keys-descend = Open selection
keys-ascend = Go to parent folder
keys-history-back = Back
keys-history-forward = Forward
keys-edit-path = Edit location
keys-refresh = Refresh
keys-toggle-hidden = Show or hide hidden files
keys-set-view-list = List view
keys-set-view-grid = Grid view
keys-copy = Copy
keys-cut = Cut
keys-paste = Paste
keys-rename = Rename
keys-new-folder = New folder
keys-delete = Move to Trash or delete
keys-permanent-delete = Delete permanently
keys-undo = Undo
keys-properties = Properties
keys-open-context-menu = Open context menu
