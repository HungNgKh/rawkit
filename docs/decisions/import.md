# Adding photographs

**Status:** in progress · **Started:** 2026-09-21 ·
**Code:** `rawkit-catalog::scan`, `rawkit-shell::importing`, `rawkit_deliver::file_metadata`

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
| A4 | The window adds **in place** — or, since A8, copies off a card | In place: nothing is copied, moved or renamed. |
| A5 | Count first, then ask, on a button that says the number | The count is a dry run of the scan that follows. |
| A6 | The import has its own connection, and the window stands still for it | One transaction of a dozen seconds cannot sit under the keypress lock; and with no `busy_timeout`, nothing else may write while it runs. |
| A7 | An import ends in a relaunch | The library in the process is the one from before. Interface decision I16. |
| A8 | A card is copied, checked, filed by date and then added | The sheet offers copy or in place after the count; a folder with `DCIM` defaults to copy. The command line's ingest, with progress and Stop. |

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
| anything, when the standing root **is not found** | taken to be where the library went: the root is re-pointed, as before |
| anything, when the standing root **cannot be read** for any other reason | the scan stops and says so. A share that has gone to sleep is not a library that moved, and guessing that it is re-points the volume — the corruption this exists to prevent |

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

`widen` re-spells deepest folder first. A folder's new spelling has more names
in it than its old one, so the only row it can collide with is one deeper than
itself, which in that order has already moved. Counted in names and not in
bytes, because what must not collide is the key, and under a convention that
folds case and normalises accents a key and its spelling can differ in length.
The case that shows it is a library root holding a subfolder with the same name
as the prefix being added.

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

## A4 — in place

The owner's decision (Q5 of the interface plan): add-in-place for the beta,
because it is the half of "import" that can be trusted with a stranger's
library. The sheet says so in one line that never changes: *They stay where
they are. Nothing is copied, moved or renamed.*

With no catalog open there is nowhere to add to, and the command says that
*before* it opens a folder picker. A new catalog opens to a screen whose first
action is to add a folder; a folder dropped on the window is added.

## A5 — count, then ask

`Stage::Counted { fresh, already, unreadable }`, from a dry run with a reader
that reads no files — names and sizes are enough to count. One rule for the
count and the scan, but two looks at a disk that may still be changing, so the
sentence at the end gives the number that was added. The button reads
"Add 1 268 photographs". Folders that could not be listed are counted on the
sheet, because a number that is short with no reason given looks like a scan
that missed things. A folder with nothing new in it never reaches the question:
the status line says everything is already here, and the sheet closes.

## A6 — its own connection, and a window that stands still

A scan is one transaction and, at twenty thousand photographs, about twelve
seconds. Under the library's mutex that is every keypress blocked for twelve
seconds. So it runs on a thread with a connection of its own — and then the
constraint from the preview work applies in the other direction: **there is no
`busy_timeout`**, so while that transaction holds the write lock, a write from
the window does not wait, it fails.

So there are none. The render loop flushes the pending edit, then does nothing
until the import ends — no saver, no preview pump — and the page's sheet covers
the window and takes the keyboard. On Linux the canvas is unmapped for the
duration, because the sheet is the page's and the canvas is over the page.
An import is refused while an export is running, for the reason a relaunch is —
and while an import runs, the shell itself refuses a rating, an export, and
opening something else. The page learns of an import by asking, up to 150 ms
late, and a rule that protects a transaction should not depend on a poll.

One connection for both halves, held across the question between them: closing
a catalog writes a rolling backup, and an import should cost the rotation one
backup, not two.

**Weighed against:** scanning through the library's own connection, in slices,
between frames. It keeps one writer by construction, but the scan is one
transaction *because* a half-added folder is worse than none, and slicing it
either gives that up or holds a transaction open across frames with the
window's own writes landing inside it.

## A7 — and then a relaunch

`--added <n>` is how the next process knows to say "Added 1 268 photographs" —
a relaunch cannot carry a sentence any other way — and to open on the grid,
which is what "here is what arrived" looks like. The previews then build
themselves, what is on screen first.

## A8 — copied off a card

After the count, the sheet asks how: **copy them to** a folder (remembered in
`import.json` beside this machine's other settings; Pictures/rawkit until one is
chosen) or **leave them where they are**. A folder that has `DCIM` in it, or is
inside one, is taken for a camera's card and the copy is chosen first:
photographs left on a card go when it is formatted. Otherwise in place is.

The copy is `rawkit_catalog::ingest` — what `rawkit ingest` does — so there is
one rule for where a file lands (`2026/2026-08-30/`, the camera's name kept, a
suffix only when a different file already has it), for proving a copy (hashed
as read, hashed again where it landed, renamed into place only then), and for
what is already there (same bytes at the same place: left, and counted). Its
progress drives the sheet's bar and **Stop** is heard between files; a file
being copied is finished and checked first, because half a file is worse than
none. What arrived before a stop is still catalogued — it is on the disk.

The count says how many are *on the card*, not how many are new: that is only
known when each is compared with what the destination has, and the sentence at
the end says it — *"Copied and added 12 photographs; 3 were already there, and
were left; 1 could not be copied — DSC00012.ARW: …"*. When anything was added,
the sentence crosses the relaunch whole (`--said`); when nothing was, it is said
where the window is, as a failure if a file could not be copied. — as a failure (`--said-failed`) when a
file could not be copied, so the relaunched window says it at the level it
deserves and puts it in the log (found in review; it was said as news).

Checked in the window with a card made of copies of two sample photographs:
dropped on a new catalog, copy offered first, both filed under their day, the
card untouched, the catalog reopened with them; the same card again copied
nothing and said both were already there.

## Not built, and known

- **Duplicates across the library.** A copy is skipped only when the same bytes
  are at the same dated path under the destination. A photograph already
  catalogued from somewhere else is copied again (the catalog would then list
  it twice, as two files with one hash).
- **Choosing what to add.** The designer's sheet has a grid of thumbnails with a
  tick on each. This adds the folder or does not.
- **Stopping is per file.** A header read on a slow card is the longest a Stop
  can take to be heard.
- **The count holds the write lock** for as long as it takes — about a second
  at twenty thousand. Nothing in the window writes meanwhile, by A6.

## If you change this

| Changing… | …is guarded by |
|---|---|
| where a volume's root goes | `where_the_root_goes_is_decided_from_spellings` (pure), `the_order_folders_are_added_in_does_not_matter` (five folders, every rotation, every file checked on disk after every step) |
| the two commands that lost a folder | `a_second_folder_on_the_same_drive_leaves_the_first_where_it_was` |
| how folder rows are re-spelled | `a_subfolder_named_like_the_folder_above_it_survives_the_root_moving_up` — and its check that no folder is left without a parent |
| what a scan may call missing | `a_file_missing_from_one_folder_is_not_every_other_folders_problem` |
| finding a library that moved | `a_library_that_has_moved_is_found_where_it_went` |
| what "the standing root is not there" means | `a_root_that_cannot_be_read_stops_the_scan_rather_than_being_guessed_at` — not found is moved; anything else is *cannot tell*, and stops |
| the import's stages, and that nothing is kept until somebody says so | `it_counts_asks_and_then_adds`, `saying_no_keeps_nothing`, `stopped_part_way_it_keeps_nothing` |
| what happens when there is nothing new | `a_folder_already_in_the_catalog_is_nothing_to_add` — nobody is asked a question with one answer |
| a failure on the sheet | `a_folder_that_is_not_there_is_a_failure_that_stays_until_read` |
| anything the render loop does while `IMPORTING` | nothing automatic. **It must write nothing to the catalog.** Read the block in `tick` |
| stopping or trying a scan | `a_scan_that_is_stopped_keeps_nothing_and_says_how_far_it_got`, `a_dry_run_counts_and_keeps_nothing` |
