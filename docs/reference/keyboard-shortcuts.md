# Keyboard shortcuts

Shortcuts are written with macOS symbols: `⌘` Command, `⌥` Option, `⌃` Control, `⇧` Shift, `↵` Return, `⌫` Delete.

!!! note "Linux and Windows"
    On Linux and Windows every shortcut is the same, with Ctrl in place of `⌘` and Alt in place of `⌥`. For example, `⌘⇧L` is Ctrl+Shift+L and `⌥⌘L` is Ctrl+Alt+L. Tooltips and menus in the application show shortcuts in this form.

Most commands are also available by name in **Find Action** (`⌘⇧A`), which shows each command's shortcut next to it.

## General

| Shortcut | Action |
|---|---|
| `⌘⇧A` | Find Action |
| `⌘⇧L` | Open New Query Console |
| `⌘⇧[` / `⌘⇧]` | Previous Tab / Next Tab (wraps around) |
| `⌘W` | Close Tab |

## Tool windows

| Shortcut | Action |
|---|---|
| `⌘1` | Database Explorer |
| `⌘2` | Files |
| `⌘4` | Run |
| `⌘8` | Services |

Database Explorer and Files share the left panel, so opening one replaces the other.

## Zoom IDE

| Shortcut | Action |
|---|---|
| `⌃⌥⇧=` | Zoom In IDE |
| `⌃⌥⇧-` | Zoom Out IDE |
| `⌃⌥⇧0` | Reset IDE Zoom |

On Linux and Windows these are Ctrl+Alt+Shift with the same keys. They match the physical key. See [Appearance](../guide/appearance.md#zoom-ide).

## SQL console

| Shortcut | Action |
|---|---|
| `⌘↵` | Execute the statement under the cursor, or the selection |
| `⌘⇧↵` | Execute Script (the whole console from top to bottom) |
| `⌘F2` | Stop the running query |
| `⌥⌘L` | Reformat Code (the selection, or the whole console) |
| `⌘Z` / `⌘⇧Z` | Undo / Redo, including replacements and folded code |
| `⌘-` / `⌘=` | Fold / unfold the region under the cursor |
| `⌘⇧-` / `⌘⇧=` | Fold All / Unfold All |
| `⌃Space` | Code completion: arrows select, `Enter` or `Tab` inserts, `Escape` closes |

## Find and navigation in the console

| Shortcut | Action |
|---|---|
| `⌘F` | Find |
| `⌘R` | Replace |
| `⌘G` / `⌘⇧G` | Next / previous match; in the Find field also `Enter` / `⇧Enter` |
| `⌘L` | Go to Line/Column |
| `Escape` | Close Find and return to the console or grid; close Go to Line/Column |

## Results grid

These shortcuts work only while the grid has focus, so they never capture typing in SQL or filter fields.

| Shortcut | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move to the adjacent cell, also in Transpose |
| `⇧` + arrows, `⇧`-click | Extend the selection to a rectangle |
| `⌘A` | Select all cells of the visible page |
| `⌘C` | Copy the selected value or range |
| `Enter` / `Escape` | Start / cancel editing a cell; invalid drafts remain open |
| `Space` / `Enter` | Toggle the selected boolean cell while the grid has focus |
| `⇧↵` | Open in Value Editor |
| `⌘⇧↵` | Show Record View |
| `⌘↵` | Submit pending changes |
| `⌘F` | Find on Current Page |
| `⌘G` / `⌘⇧G` | Next / previous match |
| `F4` | Related Rows; in the relation chooser, arrows select, `Enter` opens and `Escape` closes |

## Table editor

| Shortcut | Action |
|---|---|
| `⌘N` | Add Row |
| `⌘⌫` | Delete Row |
| `⌘D` | Clone Row |

They act on the selected cell when no text field has keyboard focus.

## Database Explorer

| Shortcut | Action |
|---|---|
| `↑` / `↓` | Select the previous / next node |
| `←` / `→` | Collapse / expand |
| `Home` / `End` | First / last node |
| `Page Up` / `Page Down` | Move by one visible page |
| `Enter` / `F4` | Open the selected table or view |
| `⌘F` or typing | Speed Search |
| `Escape` | Clear Speed Search; with an empty field, return to the editor |
| `⇧Escape` | Hide the Explorer and return to the editor |

## Query plan diagram

With the diagram focused:

| Shortcut | Action |
|---|---|
| `↑` / `↓` | Previous / next operation |
| `←` / `→` | Parent / first child |
| `Home` / `End` | First / last operation |

## DDL Diff

| Shortcut | Action |
|---|---|
| `F7` / `⇧F7` | Next / previous difference |

## Depth View

| Shortcut | Action |
|---|---|
| `←` / `→` | Move the time window by one step |
