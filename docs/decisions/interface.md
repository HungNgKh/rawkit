# Interface

**Status:** in progress · **Started:** 2026-09-20 ·
**Code:** `crates/rawkit-shell/ui/panel.html`, `rawkit-shell::{main, library, page_contract}`

The interface is being restructured in slices. The plan, the evaluation it came
from and the eight product decisions behind it (two workspaces, which Lightroom
keys stay, one *selected* concept, photographic slider units) are in the design
vault, not here. This file records what each slice decided **in the code** and
what a contributor would otherwise undo.

## Constraints every slice lives under

- **HTML cannot be drawn over the photograph on Linux.** The canvas is its own X
  window on top of the page. Chrome lives around the canvas; anything over the
  photograph is drawn by the GPU.
- **The GPU overlay draws rectangles, not text.** Words stay in HTML.
- **The whole view is re-sent after every action, inside a 100 ms budget.**
  Anything added to it is a cost per keypress.
- **The page is a string to the compiler.** See I2.

## The decisions

| | Decision | In one line |
|---|---|---|
| **I1** | One status line, one way in | Everything said to the user goes through `tell(level, text)` and lands in a footer that cannot scroll away. |
| **I2** | The page is held to the shell's types by a test | `page_contract` scrapes `panel.html` and fails on an action, command or view field the shell does not have. |
| **I3** | Three levels, told apart by cause, in the shell | *info* fades · *refused* stays until the next thing is said · *failed* gets its own line until dismissed. |
| **I4** | The shell says what happened; the page says which key | `CullView::said` is a sentence with no key names in it. |

## I1 — one status line

Every refusal used to be written to `#log`, a line inside the collapsible *View*
section, second from the bottom of a column six screens long. Nine places wrote
to it, two with debug strings (`edit 42`, `view changed`), which taught the eye
to ignore it. Six failures never reached the page at all — they were `eprintln!`
to a terminal the user may not have. One of them was **"this edit could not be
saved"**.

| Where messages go | |
|---|---|
| A toast over the photograph | Not possible: HTML cannot be over the canvas on Linux, and the GPU draws no text. |
| The header, beside the filename | Already carries eight things in 11 px. |
| **A footer under the column** | Always on screen whatever is scrolled to; becomes the full-width status bar when the window gains four sides. |

`tell` is the only writer. `say(e)` classifies a rejected command and `hint(text)`
is for "how to use the tool you just picked up"; both call it. The standing
facts — how many are selected, where K goes, a running export — sit on a second
row because they are state, not events.

## I2 — the page contract

The panel names an action as `"pick"`, a command as `"apply_preset"`, a field as
`view.in_target`. Rename any of them in Rust and everything still compiles and
every other test passes; the window fails the first time the key is pressed.
That happened: the page said `quick_toggle` for an afternoon after the action
became `target_toggle`.

`page_contract.rs` reads the page as text. It is a scrape, not a parse, and
strict about it: an `act(` whose first argument holds no string literal **fails**
rather than being skipped — a name the test cannot see is a name it cannot check.
Verified to bite on all three vocabularies.

## I3 — refused is not failed

A refusal is the shell declining something it understood: Backspace in the whole
library, a name already taken. A failure is the shell not managing something it
agreed to do. The first should be gone at the next keypress; the second must
survive it — *"could not be saved"* replaced a second later by *"Picked"* is the
old problem again.

The page cannot tell them apart from a string, so the shell does:
`Told::from_error` walks the `anyhow` chain and calls it *failed* only if it
finds `CatalogError::{Sqlite, Io, Corrupt}` or an `io::Error`. Everything else —
including a bare string from a command handler — is a refusal.

The render loop has no command to return through, so it uses one notice slot the
page polls four times a second. Two messages inside one interval collide, and
**the heavier one is kept**: a failure is never replaced by the white-balance
readout that followed it. `NOTICE` is a **leaf lock** — `tell` takes nothing else
while holding it — which is what makes it safe to call `failure` from under the
library's lock or the session's.

An export that finishes with failures says which file and why on the failed
line. It used to say *"1 failed"* in the window and the reason to the terminal.

## I4 — sentences from the shell, keys from the page

`act` returns a view carrying `said`: *"Rejected DSC00881.ARW"*, *"Added 2 to
Portfolio; 3 already in it"*, *"Undid the judgement on …"*, *"Nothing to undo"*.
Only the view an action returns carries it — a view that was merely asked for
describes state, so it is `None` there and nothing is said twice. Moving between
photographs says nothing: the photograph changing is the feedback.

The sentence never names a key. The page appends `UNDO_HINT` (*"· Z undoes
it"*), which is the one line to change when undo moves to `Ctrl+Z` — and appends
it only when the view says `takes_back`: that **this action** recorded something.
`undoable` is the wrong question. Pressing P on a frame that is already a pick
records nothing, and a hint gated on "the stack is not empty" would promise to
reverse whatever was put there earlier. `takes_back` is a counter compared
across the action, not the stack's length, which does not change when a full
stack takes a new entry and drops its oldest — and every push goes through
`remember`, so there is one place to count.

A frame is **named before it is judged**. Under a filter, the judgement is what
takes it out of the view, and it cannot be named from there afterwards.

Auto-advance is **not** shown as a setting yet. It was planned for this slice,
but a label for a setting that cannot be changed is a lie; it arrives with the
slice that gives settings somewhere to live.

## If you change this

| If you touch… | …this will tell you |
|---|---|
| an action name, a command name, a field of `CullView` | `page_contract::*` |
| what an action says, or whether it says it twice | `every_change_says_what_it_did_and_says_it_once`, `an_undo_can_name_a_frame_the_filter_had_hidden`, `a_label_that_takes_the_frame_out_of_view_still_names_it` |
| what undo is given, or how | `the_stack_being_full_does_not_hide_a_new_entry`; the `takes_back` half of `every_change_says…` |
| the notice slot | `a_failure_is_not_talked_over` |
| what counts as a failure | `a_refusal_and_a_failure_are_told_apart_by_cause` |
| anything that writes a message in the page | there must be exactly one writer: `grep -n 'saidLine\.\|failedLine\.' panel.html` shows only `tell` and the dismiss handler |
