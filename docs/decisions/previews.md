# Previews that build themselves

**Status:** accepted, part built · **Decided:** 2026-09-21 ·
**Code:** `rawkit-shell::building`, `rawkit-deliver::previews`,
`rawkit-catalog::previews::outstanding_in` ·
**Commits:** `b2f3b13` `b776ee9` `f36100c` `8db22c2` `1a931bc`, and visible-first in the one after

Open a catalog nobody has built previews for and the grid fills in while you
cull. Before this, that catalog was a black rectangle, and the cure was a
terminal command the window never mentioned.

## What it had to satisfy

- A keypress has **100 ms**, at twenty thousand photographs, and takes the
  library's lock. Anything else that takes that lock is inside the budget.
- SQLite, one writer. **No `busy_timeout` anywhere**, so a second connection
  that writes while the window saves an edit does not wait — it fails.
- The development machine draws its canvas on an integrated GPU that shares its
  memory with everything else. A photograph's previews are about two thirds of
  a second of decode, pyramid, render and encode, and ~300 MB while in flight.
- Nobody asked for this work. It runs under whatever they are doing, and the
  test of it is that they do not notice it except by its results.

## The decisions

| | Decision | In one line |
|---|---|---|
| P1 | One implementation, in `rawkit-deliver` | The terminal's `build` and the window's `one` are the same render; the shell cannot depend on a binary. |
| P2 | The builder has its own GPU device | The precedent is export, which already does exactly this in-process. Made lazily: a catalog with nothing outstanding never pays for it. |
| P3 | **The builder never opens the catalog** | It is handed a `Wanted` and hands back `Preview`s. The render loop records them. One connection, one writer, no `SQLITE_BUSY` to handle because nobody else asks. |
| P4 | The pump runs at the top of every frame, before the grid's early return | `tick` is a 16 ms GTK timer and runs with no input, so it can be relied on; the grid is where somebody is sitting while previews matter. |
| P5 | What is outstanding is asked **a page of 64 at a time** | The whole walk is ~270 ms at 20 000 under the keypress lock. A page is 1.4 ms and flat in the size of the library (scale gate, `page 64`). |
| P6 | Workers take **one photograph at a time from a queue** | A list fixed up front cannot be re-ordered by a scroll. The terminal keeps its fixed list; it has no scroll. |
| P7 | The queue owns "dispatched once" | What is waiting is one map by photograph, and the orders are lists of ids into it — so a photograph can be in both orders and still be one piece of work. An id leaves `in_flight` when its result *arrives* — recorded, failed or discarded alike — or the send fails, and for no other reason. |
| P15 | **What you are looking at, first** | The grid names the cells on screen it has nothing for, nearest the selection first, at most 64. They go to the front, and the list is *replaced* each time: what was on screen a scroll ago has no claim. Each also joins the back of the walk's order the first time it is seen, or a photograph only the screen had asked for would, after a scroll, be wanted by nothing that would ever offer it — and a run ends when nothing is waiting. |
| P16 | On-screen cells are read from the catalog, not looked up in the queue | The walk may not have reached them: scroll to the middle of twenty thousand and you are five seconds ahead of it. And where it has, what it read is older. Asked only when the list changes — 1.4 ms, about once a photograph — not every frame. Failed photographs are left out, or one that cannot be built would be put back at the front for as long as it stayed in view. |
| P8 | `sync_channel(PREVIEW_JOBS)` | A stalled pump makes the builder wait; results cannot pile up for one unlucky frame to record. |
| P9 | **One job**, and held while an edit is moving | Measured, below. Export uses two; somebody asked for an export. |
| P10 | The walk covers everything the source holds, not what the filter shows | A build that followed the view would finish on three picks and leave the shoot black. A change of source restarts the walk; what is built is skipped by the test that found it wanting. |
| P11 | Failures never stop a run and are said once | Counted, named at the end — the first by name, the rest by number — and not retried until somebody asks for a build by name. |
| P12 | Persist nothing | Quit mid-build and the next launch walks again; the catalog already knows what is current. |
| P13 | Stop and Build are commands with no key, and **the last one asked wins** | They are for the day the machine is needed for something else. Stop without a way back would be a dead end, so both exist. Each clears the other where it is asked, not in the pump — there, two inside one frame came out as Build whichever was last. Build says so, in words, on a machine with no second device. |
| P14 | The grid keys its cells by photograph and remembers misses | By position, a filter left slot 0 drawing the old slot 0; and four misses nearest the selection could starve every cell that did have a preview. What the pump records is dropped from both the misses and the cells: a photograph is rebuilt because its edit changed, so the thumbnail in hand is the old one. |

## Measured

On the AMD 780M, 3120×2022 canvas, 24 MP Sony files, debug build of the shell
with dependencies optimised.

| | builder idle | builder rendering | rendering, held while moving |
|---|---|---|---|
| Grid, per frame | 0.8–1.0 ms, worst 1.3–3.0 | 1.0–1.2 ms, worst 6–7 | — |
| Dragging Contrast in Develop (coarse) | 29–43 ms, worst 44–59 | 46–50 ms, worst 86–98, once 174 | first second 48 ms, then 27 ms, worst 45 |
| Ten photographs | — | 9.1–10.7 s | 14.2 s with a five-second drag in it |

The second device costs the grid nothing anyone would see. It costs a slider
drag about a third of its frame rate and doubles its worst frame, at **one**
job — which is why the number is one and why P9 has a second half. The hold
is between photographs: the one in flight finishes, so the first second of a
drag is still shared.

In the scale gate, at 20 000: a page of 64 is **1.4 ms**; recording 8 previews
0.2–0.7 ms, 24 previews 0.7–1.4 ms; the first write after a run of reads
**2–7 ms**, which is the connection going from reading to writing and is paid
once each time the builder wakes — not a per-row price.

With 64 cells on screen and the builder working, the grid measures 2.0 ms a
frame, worst 12 ms: the near list costs nothing that shows.

## Not built yet

- **An edit made while its photograph is queued or in flight** builds the old
  edit. Harmless — the hash declines it on lookup — and wasted. The cure is one
  door for every edit write (there are **six** `edits::save` call sites in the
  shell, not four) and a list of dirtied ids the pump drains.
- **Stopping inside a photograph.** Needs `one` to take a `keep_going` and
  check it at the stage boundaries it already times.
- **An unplugged card** fails every file on it, one cheap `exists()` each, and
  is reported as one sentence. Skipping a volume after ten in a row is not
  built.
- **Previews ignore a chosen camera profile.** `one` renders with the
  decoder's matrix; the window and an export use the `.dcp` the catalog
  records. A body with a profile gets thumbnails in a slightly different
  colour from its loupe, and the staleness key does not include the profile.
  Older than this work, and found by it.

## If you change this

| Changing… | …is guarded by |
|---|---|
| who records previews, or when `in_flight` is cleared | `a_result_of_any_kind_lets_the_photograph_be_asked_for_again`, `what_the_builder_finishes_reaches_the_catalog_and_is_not_asked_for_again` |
| what the queue admits | `a_photograph_asked_for_twice_is_built_once`, `forgetting_the_queue_lets_what_was_started_finish` |
| the order things are built in | `what_is_on_screen_is_built_before_what_is_not`, `what_was_on_screen_a_scroll_ago_has_no_claim`, `what_is_being_built_is_not_offered_again_by_being_on_screen` |
| who else holds a photograph the screen asked for | `a_photograph_only_the_screen_asked_for_is_still_built_after_a_scroll` — without it a run never ends |
| where an on-screen cell's edit is read from | `what_the_screen_read_a_moment_ago_replaces_what_the_walk_read`, `the_pump_asks_about_what_is_on_screen_before_the_walk_gets_there` |
| what a run says, and how often | `the_run_says_what_it_came_to_once_and_names_a_failure`, `stopping_forgets_what_is_waiting_and_counts_what_was_done` |
| what Stop and Build do to each other | `the_last_thing_asked_is_what_happens` |
| how the builder is held or woken | `a_held_builder_starts_nothing_and_wakes_when_let_go` — a lost wakeup is a build that stops for good |
| the rule for "stale" | `asking_a_page_at_a_time_gives_the_answer_asking_all_at_once_does` (catalog) |
| the page size, or anything under the pump's lock | the scale gate's `page 64` and `record` columns |
| `PREVIEW_JOBS`, or the hold | nothing automatic. Repeat the table above, on integrated graphics, before and after |
| the names of the three commands | `every_command_the_page_invokes_is_registered` |
