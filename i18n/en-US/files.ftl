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
