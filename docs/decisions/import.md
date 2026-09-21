# Adding photographs

**Status:** in progress · **Started:** 2026-09-21 ·
**Code:** `rawkit-catalog::scan`, and the window's import on top of it

How photographs get into a catalog: from the terminal (`rawkit catalog --scan`,
`rawkit ingest`) and, from S8 of the interface work, from the window.

## What it had to satisfy

- **Originals are never modified, moved or renamed** by adding them.
- SQLite, one writer, no `busy_timeout`.
- A scan is one transaction. A half-added folder is worse than none.
- No schema change without a reason a query gives. The beta has not shipped, but
  the author's own library is on schema 5 and must migrate cleanly.

## The decisions

| | Decision | In one line |
|---|---|---|
| A1 | A volume's root **widens**; it is never re-pointed at the folder being scanned | Adding a second folder on a drive used to lose the first. |
| A2 | "Missing" is decided for the folder that was scanned, not for the volume | A scan says nothing about folders it did not look in. |
| A3 | A scan can be watched, stopped, and tried without being kept | `scan_watched`: progress, `Cancelled`, `dry_run`. One code path for the count before and the scan after. |

## A1 — the root widens

A volume row has one `last_mount_path`, and every folder on the volume is stored
relative to it. The scan set it to *whatever folder was being scanned*. That is
right for the only thing anybody had done — scanning one library root, again and
again — and it is what lets a library that has moved be found by scanning where
it went.

Scan `…/2025` and then `…/2026`, though, and the second scan re-pointed the
volume at `2026`. Every photograph from `2025` now resolved to a path under
`2026` where it had never been, and the sweep for missing files — which covered
the whole volume — flagged all of them. Two commands, and the first folder was
gone from the library. `rawkit ingest` scans its destination the same way and
had the same hole. Adding a folder is the first thing the window's import does,
so it has to be safe to do twice.

**Chosen:** paths stay relative to one root per volume, and the root *widens* to
the nearest folder containing both what was there and what is being added.
Every folder row on the volume is re-spelled from the new root, in the same
transaction as the scan.

| What is being added | What happens |
|---|---|
| the standing root, again | nothing changes |
| a folder inside it | scanned relative to the standing root |
| a folder beside it, above it, or elsewhere on the drive | the root moves up to their common ancestor — `/` or `C:/` if that is all they share |
| anything, when the standing root **is no longer there** | taken to be where the library went: the root is re-pointed, as before |

**Weighed against:** rooting every volume at its mount point. It is the cleaner
model — a remount is then unambiguous — and `VolumeId::resolve` already finds
the mount point on all three platforms. But existing catalogs store paths from
their scan root, and re-basing them needs the mount point *as it was*, which
for a drive that is not plugged in nobody knows; it needs a flag column to tell
re-based volumes from old ones; and it makes the scan depend on a canonical
spelling of the mount point that differs per platform (`\\?\C:\` against `C:\`).
Widening needs none of that: the invariant is the one the catalog already has,
the decision is a pure function over stored spellings (`Rooting::decide`), and
the one question asked of the disk is whether the standing root is still there.

**What it does not settle:** the last row of the table is a guess, the same one
the scan always made. A library folder *renamed* and a second folder *added* in
one step cannot be told apart from the spellings. Relink by content hash is the
answer to that and is separate work.

`widen` re-spells longest path first. A folder's new spelling is longer than its
old one, so the only row it can collide with is one longer than itself, which in
that order has already moved. The case that shows it is a library root holding a
subfolder with the same name as the prefix being added.

## A2 — missing, for the folder that was looked in

`mark_missing` swept every file on the volume that the walk had not reached.
While a volume only ever had one folder scanned into it that was the same as
"every file under the root". With two, scanning one flagged the other. It now
takes the scanned folder's spelling and sweeps only folders at or under it.

## A3 — watched, stopped, tried

`scan_watched(catalog, root, volume, metadata, dry_run, watch)`. `watch` hears
`Walking { found }` once a folder and `Reading { done, total, name }` once a
file, and answers whether to go on; `false` is `CatalogError::Cancelled` and,
because the scan is one transaction, nothing is kept. `dry_run` does everything
and rolls back: the count a person is shown before they press the button comes
from the code that will run when they do. With `no_metadata` it reads no files.

`scan_on` is `scan_watched` that is never stopped and always kept, so the
terminal and the tests are unchanged.

## If you change this

| Changing… | …is guarded by |
|---|---|
| where a volume's root goes | `where_the_root_goes_is_decided_from_spellings` (pure), `the_order_folders_are_added_in_does_not_matter` (five folders, every rotation, every file checked on disk after every step) |
| the two commands that lost a folder | `a_second_folder_on_the_same_drive_leaves_the_first_where_it_was` |
| how folder rows are re-spelled | `a_subfolder_named_like_the_folder_above_it_survives_the_root_moving_up` — and its check that no folder is left without a parent |
| what a scan may call missing | `a_file_missing_from_one_folder_is_not_every_other_folders_problem` |
| finding a library that moved | `a_library_that_has_moved_is_found_where_it_went` |
| stopping or trying a scan | `a_scan_that_is_stopped_keeps_nothing_and_says_how_far_it_got`, `a_dry_run_counts_and_keeps_nothing` |
