# Looking after a catalog

**Status:** in progress · **Started:** 2026-09-23 ·
**Code:** `rawkit-catalog::relink`, `rawkit-catalog::backup`, `rawkit-shell::care`

What the window does about the two ways a library goes wrong without anybody
touching a photograph: **the files move**, and **a write goes bad**. Both were
mechanisms in the catalog with nothing in the window to reach them, which is the
same as not having them — a beta is other people's photographs, and the
roadmap's own line is that a catalog application that eats a library is
unrecoverable reputationally.

## What it had to satisfy

- Photographs are never modified, moved or renamed by any of this.
- Nothing a person can press may lose work that cannot be got back, including
  when they press the wrong one.
- A library whose files have all moved must be able to say so, from the screen
  it lands on — which is the one with the fewest ways out.
- SQLite, one writer, no `busy_timeout`: anything long-running holds the write
  lock, so the window must stand still for it.

## The decisions

| | Decision | In one line |
|---|---|---|
| S1 | A photograph is recognised by its **contents**, never its name | A name is what changed in half the cases this exists for. |
| S2 | What a photograph is has to be written down **while it is still there** | A copy off a card records it as it verifies; a library added in place needs the job that reads them once. |
| S3 | A volume's root **widens**, never re-points — in the relink as well as the scan | One function both call. Pointing the search at one folder used to move the whole volume under it. |
| S4 | Two missing photographs whose files are identical are **left alone** | Nothing on the disk says which is which, and the wrong answer is worse than none. |
| S5 | Restoring a backup **never overwrites** | The copy opens as a new catalog beside the old one; the old one is untouched. |
| S6 | A library that looks empty because its files moved says so, and offers the way out | The welcome screen is where that library lands, and the palette does not open there. |

## S1 — by contents

`relink::search` lists the photographs whose files have gone, walks the folder
it is given, and reads **only** the files whose size matches one of them — one
hash per plausible file rather than one per file in the folder. A file whose
hash matches exactly one missing photograph re-points that row: folder rows and
the volume made as needed, all in one transaction, because a half-relinked
library is one whose photographs are in two places according to itself.

Names are not consulted at all. A renamed folder, a library moved to a bigger
disk and a card import tidied up afterwards are the cases this exists for, and
in every one of them the path is what changed.

## S2 — writing it down in time

The catch in S1 is the order: a photograph can only be recognised later if what
it is was recorded **before** it moved. A scan reads headers rather than whole
files — deliberately, it is two hundred times cheaper — so a library added in
place has no hashes at all until something reads it.

So: a copy off a card writes down the hash its verification already computed
(free, and it is where new photographs arrive from), and the window has
**"Write down what each photograph is"** for everything else: one pass, one read
per photograph, progress and Stop, resumable because each file is its own row.
The relink's sentence says how many missing photographs had no note, because
that is the number nothing can help with.

## S3 — widen, never re-point

The mistake the scan made once (`import.md` A1), made again here and found by
hand in the window: pointing the search at one folder of a library re-pointed
the volume's `last_mount_path` at that folder, so every photograph *not* found —
the ones still missing — came to claim a path under it that it had never had.
`scan::root_for` is now the one place that answers "where is this volume
measured from, now that this folder is part of it", and the scan and the relink
both call it. Guarded by `finding_one_folder_does_not_move_what_is_still_missing`.

## S4 — ambiguity is left alone

Two photographs in the catalog with the same contents — the same frame imported
twice, a deliberate copy — and a file that matches both: the file is counted as
ambiguous and nothing is written. Putting a photograph back on the wrong file is
worse than leaving it missing, and nothing about the folder says which is which.

## S5 — restoring never overwrites

A catalog is copied before every migration and on close; the window now lists
those copies, says where they are kept and how many are held, and can make one
on the spot. **Opening one copies it to a new catalog beside the open one** —
`library-from-2026-09-23T14-42-26Z.rawkit` — and opens that. The catalog that
was open is left exactly as it is.

Restoring *over* a library is the one operation here that could lose work
outright, including for somebody who opens the wrong copy to see what is in it.
A file to delete by hand afterwards is a cheap price, and it sidesteps the race
between a process that is closing a catalog and the one that is reopening it.

## S6 — the screen a moved library lands on

A catalog whose files have all moved opens with nothing to show, and said
"…is open, and has no photographs in it yet" — with no way out, because the
command palette does not open on the welcome screen. It now counts what is
missing as it opens and says *"all 3 of its photographs are missing"*, with the
search offered as the first thing on the screen; the palette opens there too,
and both jobs are marked as working with nothing on screen. A catalog that
opens with *some* missing says so in the status line.

## Not built, and known

- **Nothing hashes in the background.** "Write down what each photograph is" is
  a job somebody runs. A library imported in place is unfindable until they do.
- **A moved library is found one folder at a time.** The search takes a folder;
  a library split across two disks needs two passes.
- **The relink cannot be undone**, other than by scanning the old location again
  or opening a copy of the catalog from before it.
- **Backups are not offered on the welcome screen**, so a catalog too broken to
  open cannot be restored from inside the window. The files are in the backups
  folder beside it, named for when they were taken.

## If you change this

| Changing… | …is guarded by |
|---|---|
| how a moved photograph is recognised | `photographs_that_moved_are_found_by_their_contents`, `two_copies_of_one_photograph_are_left_alone`, `a_photograph_with_no_hash_cannot_be_found_and_is_counted` |
| where a volume is measured from | `finding_one_folder_does_not_move_what_is_still_missing`, and `import.md`'s rooting tests |
| stopping a long job | `a_search_can_be_stopped`, `a_stop_between_files_keeps_and_lists_what_arrived` |
| what a copy writes down | `a_copy_writes_down_what_each_photograph_is` |
| the backups list or where copies go | nothing automatic. By hand: the panel lists what is in the folder, "Copy it now" adds one, "Open a copy" opens a new catalog beside and leaves the old one alone |
| the welcome screen for a moved library | nothing automatic. By hand: a catalog whose files have all moved must name the number and offer the search |
