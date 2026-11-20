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
menu-move-to-trash = Move to Trash
menu-delete = Delete
menu-new-folder = New Folder
menu-new-file = New File
menu-properties = Properties
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
