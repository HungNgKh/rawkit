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
| **I5** | A tool in hand owns the keyboard | In crop, spot, or while placing an adjustment, keys that judge or move wait — and say so. Held in the page *and* the shell. |
| **I6** | A pointer gives the keyboard back; a keyboard keeps it | A slider blurs after a drag, not after Tab. Escape is one rung per press. |
| **I8** | What is showing and what is in hand are two words | `mode` is the view; `tool` is what a press on the photograph would do. Both come from the shell. |
| **I9** | One tool at a time, in both directions | Picking up crop or the spot tool puts the rest down; while one is in hand, nothing else is picked up — by key *or* by button. |
| **I10** | The tool's exits are always on screen | A row in the footer: what is in hand, how to leave it, and buttons that do it. |
| **I7** | Copying settings is a chord | `Ctrl+Shift+C` / `Ctrl+Shift+V`; bare `S` and `A` say where they went. |

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

## I5 — a tool in hand owns the keyboard

Lightroom's X flips a crop's orientation. Here it rejected the photograph and
moved to the next one: a judgement made and the evidence removed in one
keypress, by somebody who thought they were cropping. Nothing guarded it — not
the page, not `CullAction::Pick`.

| What a cull key does while a tool is up | |
|---|---|
| Carries on as before | The mode error above. |
| Is silently ignored | Reads as a broken key. |
| Puts the tool down, then acts | Two things for one key, and the crop is lost without having been asked about. |
| **Waits, and says what is in the way and how to leave** | The only one that teaches the exit. |

**Which states count:** crop, the spot tool, an adjustment waiting to be placed —
the three with something *half-made* in them. Not the eyedroppers or the mixer's
target, which are armed the same way but are only what the next click will mean;
carrying one to the next photograph is a reasonable thing to want.

**Which keys wait:** everything that can change which photograph is on screen or
what is recorded about it. The whole rule rather than "navigation and
judgements", because a rule with exceptions has to be remembered and this one
only has to be read off the screen. `CullAction::waits_for` has no wildcard, so
a new action has to be put on one side or the other.

Held in two places on purpose. The page checks first, because it knows the keys
and can name the exits (*"Crop is in hand — Enter keeps it, Esc cancels"*). The
shell checks again in the `cull` handler and names none (I4), because the page
is a string nobody compiles and a rule that protects a judgement should not
depend on one. The page holds back a **list of keys that wait**, not everything
but a list of exceptions: a keyboard has a hundred keys that do nothing here, and
a tool that complained about Shift on its way to a chord would be noise.

Backspace means *remove the selected thing from where I am*: the selected mark
in the spot tool, otherwise the frame from the collection being viewed.

## I6 — who keeps the keyboard

A focused control owns its keys — without that, arrows move a slider *and* the
selection. But a slider keeps focus after the drag that moved it, so after any
adjustment made with the mouse the arrows nudged Exposure instead of changing
photograph, P and X did nothing, and there was no focus style to say why.

Blur on `pointerup`, **and only the slider the pointer went down on** — not
whichever has focus, or releasing a drag on the photograph would take the
keyboard from a slider somebody had tabbed to. Somebody who tabbed to a slider is
driving it with the arrows on purpose. `:focus-visible` draws the ring for the
same split: the engine already knows which of the two just happened.

Escape is a ladder, one rung a press: a focused control → whatever the pointer is
armed to mean → the tool in hand → the view. It used to blur a text box *and*
fall through, so Escape out of a rename also cancelled the crop behind it; and
the range picker was not on the ladder at all.

## I7 — copying settings is a chord

Bare `A` pasted the copied settings onto every selected photograph: the most
expensive key on the board was also the easiest to hit. Now `Ctrl+Shift+C` and
`Ctrl+Shift+V`, as Lightroom has them. `S` and `A` do nothing except say where
they went, for as long as hands remember them — a key that silently stopped
working is a bug report.

## I8 — two words

The badge read LOUPE while a gradient was live on the photograph, because it
showed the *view* and a gradient is not a view. It was also the variable: four
places read the badge's text back to decide what Enter, Escape and Backspace do,
so changing what it says would have changed what they do.

`mode` stays the view (`loupe · grid · survey · crop · spot`). `tool` is one word
for what a press on the photograph would do — `crop · spot · placing · wb · range
· target · mask`, or nothing — decided in the shell by `tool_name_of`, most
specific first: a mode owns the photograph; then whatever the next press is armed
to mean; then the adjustment whose handles a press would grab. A grid or a survey
has no tool whatever was left armed on the way in. The page holds both as
variables and the badge only displays them.

The page used to keep its own note of which pickers it had armed, and the shell
never said otherwise. Now the snapshot carries `armed`, and the page draws from
it (`followArmed` — draws only, invokes nothing, or the two would chase each
other).

## I9 — one tool at a time

Every tool answers the same press on the photograph, and only one can have it.
The selected adjustment went on drawing its outline and handles through the spot
tool; an armed eyedropper stayed armed under a crop.

- **Picking up crop or the spot tool puts everything else down**
  (`put_down_the_rest`): the selected adjustment, both eyedroppers, the mixer's
  target. Put down rather than ignored while the other tool is up, because
  "still armed, but not now" is a state nobody can see.
- **While crop, the spot tool or a placement is in hand, nothing else is picked
  up** (`hands_free`), in `add_mask`, `select_mask`, `arm_target` and both
  pickers. I5 held the *keys* back; every one of these has a button as well.

## I10 — the exits, always

| Where a tool says how to leave it | |
|---|---|
| Over the photograph, beside the tool | Not possible: HTML cannot be over the canvas on Linux and the GPU draws no text. |
| In the tool's own section | Scrolls away, and crop had no section. |
| **A row in the footer** | The one part of the column that cannot scroll. Becomes the tool-options bar under the canvas when the window gains four sides. |

It carries the name, the keys, and buttons for them — crop gets **Done · Cancel ·
Reset**, which it never had. `CropReset` puts the rectangle back to the whole
frame *inside* the tool and commits nothing, so Cancel still undoes it.

The tool strip under the histogram is a second way to press the same keys: it
calls what they call and waits for what they wait for. It is lit from `tool`,
never from having been clicked. Picking a tool up opens its section and brings it
to the top — once, when the tool changes, not on every poll.

## If you change this

| If you touch… | …this will tell you |
|---|---|
| an action name, a command name, a field of `CullView` | `page_contract::*` |
| what an action says, or whether it says it twice | `every_change_says_what_it_did_and_says_it_once`, `an_undo_can_name_a_frame_the_filter_had_hidden`, `a_label_that_takes_the_frame_out_of_view_still_names_it` |
| what undo is given, or how | `the_stack_being_full_does_not_hide_a_new_entry`; the `takes_back` half of `every_change_says…` |
| the notice slot | `a_failure_is_not_talked_over` |
| what counts as a failure | `a_refusal_and_a_failure_are_told_apart_by_cause` |
| which actions a tool holds back, or adding a `CullAction` | `a_tool_in_hand_keeps_the_keyboard`, and `waits_for` will not compile until the new action is placed |
| which *keys* a tool holds back | nothing automatic — `WAITS` and `inHand()` in the page are checked by hand in the window. Keep them in step with `waits_for`. |
| the order of precedence between tools | `the_badge_names_what_a_press_would_do` |
| a new tool, or a new way to pick one up | add it to `tool_name_of`, to `TOOLS` in the page, and put `hands_free()?` at the top of the command that arms it |
| anything that writes a message in the page | there must be exactly one writer: `grep -n 'saidLine\.\|failedLine\.' panel.html` shows only `tell` and the dismiss handler |
