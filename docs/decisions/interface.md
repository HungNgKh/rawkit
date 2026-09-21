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
| **I11** | Library and Develop are a word the shell owns | `workspace`, beside `mode` and `tool`. The loupe is in both, so it cannot be derived. |
| **I12** | One registry of commands | Keys, buttons, tooltips and the shortcut sheet are read from `COMMANDS`; a test holds its `waits` to the shell's `waits_for`. |
| **I13** | The column shows one workspace's controls | Sections carry `library-only` / `develop-only`; the body says which. In the file in the order they display. |
| **I14** | The palette is the registry, searched | It runs what a key runs, through `run`, and says what is in the way *before* the command is chosen. |
| **I7** | Copying settings is a chord | `Ctrl+Shift+C` / `Ctrl+Shift+V`; bare `S` and `A` say where they went. |
| **I15** | Nothing open is a welcome screen | The canvas is hidden and the page covers the window: Open, New, one photograph, the recent catalogs. The synthetic mosaic is `--test-pattern`. |
| **I16** | Opening something else is a relaunch | One process, one catalog. A new process cannot show the last catalog's collections, target or undo stack, because it never had them. |
| **I17** | A bare launch reopens the last catalog; closing one stays closed | `recent.json` beside `window.json`. Closing is a relaunch with `--welcome`, or the relaunch would reopen what was just closed. |
| **I18** | Nothing a person can meet may fail the launch or end the render loop | A catalog that will not open is the welcome screen and a sentence. A photograph that cannot be read is a flat stand-in and a sentence. |
| **I19** | Every way out goes through the render loop | It owns the saver. Closing the window used to lose an edit made in the last 800 ms; now close, open and relaunch all flush first. |
| **I20** | What the shell says has a command of its own | `take_notice`. It rode on `snapshot`, which five places read and two of them said. |
| **I21** | New never replaces a catalog, and only `--new` makes one | The picker's "replace?" is a question about a file. Without `--new`, a path that is not there is refused, not created. |

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
  up** (`hands_free`), in `add_mask`, `select_mask`, `place_mask`, `arm_target`
  and both pickers. I5 held the *keys* back; every one of these has a button as
  well.
- **A press on the photograph is armed to mean one thing** (`only`). The two
  eyedroppers and the mixer's target exclude each other, and a placement
  excludes all three. The page used to do this, in one direction only: arming
  the white-balance picker put the range picker away and not the reverse, so
  both could be armed while the badge named one.

**The page asks, then draws what it is told.** `arm`, `pickWb`, `pickRange` and
`place` used to set their own variable and light their own button before asking,
so a refusal left a button lit for something never armed. They now call `ask`,
which invokes and then reads the shell back, on success *and* on refusal. A poll
that was already on its way when one of them ran describes the moment before;
`toolEpoch` makes it say nothing about tools.

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

## I11 — two workspaces

In the grid the develop sliders were dimmed to 45 % and still took clicks; in a
survey the header named one photograph while the histogram and the tone values
were another's. Both are a panel showing controls for a photograph that is not
the one on screen.

| | |
|---|---|
| One adaptive column that guesses | A loupe means "I am judging" or "I am editing", and the guess is exactly what produced the mode errors S2 and S3 fixed. |
| Lightroom's seven modules | The product has two jobs. Import and export are dialogs. |
| **Library and Develop** | What the owner's hands and every migrant already know: G and E, and D. |

`workspace` is shell state because the loupe is in both — the same photograph at
the same size is being *judged* in one and *changed* in the other — so it cannot
be derived from `mode`. A catalog opens in the Library; a RAW opened on its own
has no library to be in and stays in Develop.

**Picking a tool up is entering Develop**, from anywhere. `R` from the grid opens
the selected photograph with the crop up, as Lightroom does. That is safe because
the render loop re-asserts crop mode on every frame rather than on the keypress:
a session replaced under an open tool is a case it already handled. It is *not*
safe for the tools that work on the edit in the session — from a grid that is
still the **last** photograph's edit until this one has loaded — so `L`, `B` and
`W` from the grid only go to Develop and say to press again, and the shell
refuses them over a grid on its own account (`one_photograph`).

Escape's bottom rung is the loupe **from a grid or a survey only**. It used to
send `loupe` from anywhere, which now means the Library, and a key for putting
things down should not also change where you are.

## I12 — one registry

The handler had the bindings. A hand-typed line at the bottom of the column
listed two thirds of them and called survey "compare". The buttons knew nothing
of either. S2 added a fourth account — a list of key names that wait for a tool —
to be kept in step with the handler by hand.

`COMMANDS` is the one account: `{ id, keys, scope, waits, group, title }` and
either `act` (a cull action by the shell's name) or `run`. Everything goes
through `run(id)` — a key, a button marked `data-command`, the tool strip — so a
button cannot do what its key would have refused.

- **`scope`** is where a command means something. A Develop command pressed in
  the Library goes there first; a Library command pressed in Develop says so and
  does nothing.
- **`waits`** replaces S2's list of key names. `the_page_and_the_shell_agree_…`
  scrapes every entry and holds its `waits` to `CullAction::waits_for`, which
  closes the gap S2 recorded as "kept in step by hand". It fails on a Reject
  that does not wait — the bug S2 was for.
- **A chord belongs to a text box while one is being typed in.** The old handler
  took every Ctrl chord before it looked at focus, so Ctrl+Z in a rename box
  undid the *edit* and suppressed the box's own undo. A slider is not a text
  box, and the chords still work with one focused.
- **Enter opens a photograph from a grid or a survey, and is nothing otherwise.**
  It used to send `loupe` whatever was showing — harmless while the loupe was
  the only place to be, and with two workspaces a way of being thrown out of
  Develop by Enter.
- The Shift fallback is for bare keys only: Ctrl+Shift+S must not become Ctrl+S
  because nothing is on the first.
- A key bound twice throws at load. The second would simply never run, and
  nobody finds that by pressing it.
- Shift on a key that does not use it is still the key (Shift+X rejects, as it
  did), and is not part of a symbol, which is already what Shift made it.

Entries are written `{ id: "…", … act: "…" }` so the contract test can read them.
That is a constraint on how the registry is formatted, and the test says so when
it cannot find forty of them.

## I13 — one workspace's controls

Sections are in the file in the order they are shown: **White balance → Tone →
Presence → Curve → Colour mixer → Grading → Detail → Lens & geometry → Local
adjustments → Spot removal → Effects → Calibration → Profile**. White balance
first because everything after it is judged through it; it was eleventh.

- **Presence** takes clarity, texture and dehaze from Tone. **Lens & geometry**
  takes the lens corrections from Detail. Moved as markup; every id is unchanged,
  which is why none of the wiring had to hear about it.
- **Colour mixer** is one section with one of hue / saturation / luminance
  showing. All twenty-four sliders exist the whole time under the ids they
  always had. One target button aims whichever is showing, and the tab follows
  the shell if something else gets armed.
- **View** and **Keys** are gone: zoom is beside the clipping readout, the pan
  arrows went (the pointer pans), and the sheet is `?`.
- The Library's panel is the judgement as controls and what the catalog knows
  about the photograph. `taken` and `in_collections` are two seeks per keypress,
  measured at 5.7 µs and 2.5 µs and flat to twenty thousand.

`[hidden] { display: none !important }` — `label { display: flex }` outranked
the attribute, so every row the page had marked hidden was on screen.

## I14 — the palette

`Ctrl+K`. Every command by name, with its key beside it — so it is also where a
key is learnt — and the way to reach a command that has no key. That last part
is what lets there be commands not worth a key: *Zoom to 1:1*, *Level the
horizon*, *New collection from what is selected*.

It is inside the column, like the sheet, because nothing the page draws can be
over the photograph on Linux.

A row that will not simply happen says why, in the row: *waits for Crop*,
*Library only*, *opens Develop*. The reason comes from `obstacle(command)`, the
same function `run` acts on. The first draft asked the three questions a second
time for the palette; that is two accounts of one rule, which is what every
review of this rework has found a bug in.

Search is every typed word found somewhere in the command, ranked by where: the
start of the title, the start of a word, anywhere in the title, then the group,
the key and the id — so "shift k" finds what Shift+K does. With nothing typed the
list is the registry's own order.

The box has its own key handler and stops what it handles: a text box owns the
keyboard, and Escape must close the palette without also being a rung of the
ladder behind it.

## I15 — nothing open is a welcome screen

A bare launch used to show a synthetic pink mosaic with a live histogram, live
sliders and no words: every control worked, on a photograph that did not exist,
and nothing said how to reach one that did (F01, the designer's first
severity-4 finding). Now it shows what can be done: open a catalog, make one,
open one photograph, or pick from the catalogs this machine has opened.

On Linux the canvas is an X window *over* the page, so the page cannot be seen
while it is mapped. With nothing open it is unmapped (`canvas::hide`) and the
render loop draws nothing. Everything else is built as usual, over a stand-in
frame — the session, the state the commands ask for — because thirty commands
take that state and a window with none of it managed would turn each of them
into a way to crash. The page asks `entrance` once, and while it says
`welcome`, `run` lets through only registry entries marked `welcome: true`.

An open catalog with no photographs in it is the same screen with a different
headline and a first action of its own: add a folder
(`docs/decisions/import.md`).

The mosaic is `--test-pattern`. It is a developer's tool and is spelt like one.

## I16 — opening something else is a relaunch

**Weighed against:** switching in place. It would be seamless, and it is what
the plan first assumed.

A catalog is not one value in this shell. It is the library, the saver, the
session, the preview builder and its thread, the grid's textures, and some
thirty statics: the mode, the workspace, the tool in hand, the undo stack, the
notice slot, the export in progress, the builder's `OnceLock`. Switching in
place means finding and resetting every one, and **the failure is silent** — a
collection list, a target or a tally from the catalog before, drawn over the
catalog after. The plan's own acceptance test for this slice was "switch twice;
nothing from the first leaks into the second". A new process passes that by
construction, and keeps passing it as statics are added by people who have
never read this.

Lightroom Classic relaunches to switch catalogs, for the same reason.

It costs about a second, and a window that closes and reopens where it was
(`window.json` is written first). It is refused while an export is running: the
export is a thread of this process, and leaving would end it part-way through a
folder with nothing to say which files were written.

**Not done:** a drop on the canvas. On Linux the canvas is its own window and
takes no drops, so a drop lands only on the panel, or anywhere on the welcome
screen. A dropped folder is photographs to add, not something to open.

## I17 — what a bare launch opens

`start_for(arguments, last)` is a pure function and is tested as one. Anything
named wins; with nothing named the last catalog comes back, if it is still
there; `--welcome` overrides that, and is how "close" is spelt — without it the
relaunch finds the catalog just closed at the top of the recent list and opens
it again. Only the *last* catalog is ever reopened: falling back to the one
before would open something nobody left, without saying why.

A recent catalog that has gone stays on the list, struck through, with a
`forget` beside it. It may be on a drive that is not plugged in, and a list
that quietly dropped it would be one more thing to wonder about.

`RAWKIT_CONFIG_DIR` moves both files. For a portable install, and for every
test of the window — which would otherwise write scratch catalogs into the list
somebody's real launch reads, and reopen one for them the next morning.

## I18 — nothing a person can meet may fail the launch, or end the render loop

Tauri panics when its `setup` hook returns an error, and the render loop ends
when a frame does. Both were reachable by the ordinary world: a catalog on a
card that is not plugged in did the first, and walking onto one missing file in
the loupe did the second — the window froze on the frame before it.

So: a catalog that will not open is the welcome screen with the reason on it. A
photograph that cannot be read is `Loaded::stand_in` — flat, dark, and
deliberately nothing like a photograph — with the reason on the status line,
and the next photograph opens normally. In the navigation block of `tick`, the
header read, the preview lookup and the texture upload no longer use `?`.

A missing file says it is not there, in those words, before the decoder is
asked: what the decoder says is "io error: Input/output error".

While a stand-in is showing, nothing is written for that photograph
(`Saver::set_aside`). The session still holds its real edit and the sliders
still work, so somebody pushing exposure up to see whether anything is there
would have been editing a picture they could not see — and it would have been
saved.

## I19 — every way out goes through the render loop

`LEAVING` is a request the render loop acts on at the top of its next frame:
flush the saver, write the window's geometry, start the next process if there
is one, exit. The saver lives there, and an edit made in the last 800 ms is
still in its settle timer.

Closing the window is one of those ways out, which it was not before: the close
is held for a frame (`prevent_close`) and the loop exits the process. That is
only safe while the loop is alive — `TICKING` — because a window held open for
a loop that has died could never be closed. And a second close closes at once:
the loop can die between being found alive and being asked.

The first way out asked for is the one taken. A file picker answers on its own
thread whenever somebody gets round to it, and must not turn a window closed in
the meantime into one that reopens on a catalog. The export check is made
twice — by the command, and again by the loop where the answer is acted on.

## I20 — what the shell says has a command of its own

`NOTICE` is taken when read, so it must only be read by something that says it.
It used to be a field of `snapshot`, and five places on the page ask for a
snapshot: two said the notice, three took it and dropped it — one of them as
the page loads. Anything said during `setup` was gone before there was a status
line to put it on, which is exactly when "that catalog could not be opened" is
said. `take_notice` is asked for by `hear()` and by nothing else.

On the welcome screen the status line is under the welcome, so `tell` writes
its sentence in both places.

## I21 — New never replaces a catalog

The save dialog asks "replace?" about a *file*, and a person who answers yes to
that has not agreed to lose a library. An existing path is left alone, and the
sentence says to open it instead. Separately, SQLite opens-or-creates: a
mistyped catalog name on the command line used to leave an empty file as its
only reply. Now only `--new` — which only New passes — may create one.

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
| why a command would not be carried out | `obstacle` in the page is the only place that decides; `run` and the palette both read it |
| what a command does, which keys it has, or whether it waits | `page_contract::the_page_and_the_shell_agree_about_what_waits_for_a_tool`, and the registry throws at load on a key bound twice |
| the shape of a registry entry | the same test — it reads `{ id: `, `act: "…"`, `value: …`, `waits: …` as text |
| what the view carries per keypress | `the_view_says_where_a_photograph_is_kept`; the `holding 1` and `taken 1` columns of the scale gate |
| what is written while a stand-in is showing | `a_photograph_that_could_not_be_read_is_not_edited_by_accident` |
| what a launch opens | `what_is_named_is_what_opens`, `with_nothing_named_the_last_catalog_comes_back`, `a_catalog_that_was_closed_stays_closed`, `the_test_pattern_has_to_be_asked_for` |
| the recent list | `the_last_one_opened_comes_first_and_is_listed_once`, `only_so_many_are_kept`, `a_bare_launch_reopens_the_last_one_only_if_it_is_there`, `a_list_that_cannot_be_read_is_an_empty_one` |
| anything in `setup`, or in the navigation block of `tick` | nothing automatic. **No `?` on anything a missing file, a bad catalog or a corrupt preview can reach.** Check by hand with `target/scratch`-style catalogs whose files have been renamed |
| who reads the notice | `grep -n 'take_notice' panel.html` shows only `hear` |
| a new command that should work with nothing open | mark it `welcome: true` in the registry, or the welcome screen drops it |
| a new static that holds something about the open catalog | nothing to do — that is what I16 buys. Do not add a "reset" path |
| anything that writes a message in the page | there must be exactly one writer: `grep -n 'saidLine\.\|failedLine\.' panel.html` shows only `tell` and the dismiss handler |
