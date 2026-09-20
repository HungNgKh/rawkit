export const meta = {
  name: 'slice-review-scoped',
  description: 'Read-only review of UI slice S2 from prepared, scoped input files: a key/mode truth-table lens on sonnet (findings adversarially verified) and a rules lens on haiku',
  whenToUse: 'After building a UI slice whose risk is key handling. The author prepares target/scratch/<slice>-keys.txt and <slice>-rules.txt first. args: {slice}',
  phases: [
    { title: 'Review', detail: 'keys (sonnet) + rules (haiku), one input file each' },
    { title: 'Verify', detail: 'refute each keys finding, max 3', model: 'sonnet' },
  ],
}

const FINDINGS = {
  type: 'object',
  properties: { findings: { type: 'array', items: { type: 'object',
    properties: {
      where: { type: 'string', description: 'section letter and the exact line of text the finding is about' },
      claim: { type: 'string', description: 'one sentence: what goes wrong' },
      scenario: { type: 'string', description: 'state + key pressed -> what happens -> what should' },
    }, required: ['where', 'claim', 'scenario'] } } },
  required: ['findings'],
}
const VERDICT = { type: 'object', properties: { refuted: { type: 'boolean' }, why: { type: 'string' } }, required: ['refuted', 'why'] }

const slice = (args && args.slice) || 'S2'
const low = slice.toLowerCase()
const FENCE = `HARD LIMITS. This is read-only: never edit, create or delete a file, never run cargo. Your ENTIRE input is one prepared file; read it with exactly one command and do not open the repository's source files, do not run git diff, do not explore. The previous run of this review spent five times its budget re-reading whole files; the file you are given already contains every line that matters. An empty findings list is a good answer. No praise, no style opinions, no restating the code.`

const keysPrompt = `${FENCE}

Run: cat target/scratch/${low}-keys.txt

It is the key handling of a photo editor's panel (JavaScript, sections B-D) and the matching guard in its Rust shell (E-F), after a change that makes "a tool in hand own the keyboard": while cropping (mode crop), using the spot tool (mode spot), or while a local adjustment waits to be placed (placing === true), keys that judge a photograph or move to another one must NOT act; they must produce a message via tell("refused", ...). Each tool keeps its own exits. Escape must go up exactly ONE level per press.

Build the truth table in your head: for each state in {loupe, grid, survey, crop, spot, placing, a text input focused, a slider focused by pointer, a slider focused by Tab} and each bound key, what happens. Report ONLY cells that are wrong. Look specifically for:
1. A key that still reaches act("pick"|"reject"|"rate"|"colour"|"next"|"previous"|"undo"|"mark"|"paste_edit"|"take_out"|...) with a tool in hand. Check the digit and colour-label branches, the Ctrl/Cmd chord branch (which returns before heldBack is consulted), and keys whose event.key is not what WAITS lists (e.g. with Shift held: "P" vs "p", "+" , "{" for shift-[ , "|" for shift-backslash; the handler lower-cases in some places and not others).
2. A tool that cannot be put down: an exit key that WAITS holds back and the tool's keys list does not return, or that the SHELL refuses (section E waits_for / section F guard) although the page sends it — e.g. what does Escape send in spot mode, in placing, in crop, and does waits_for let that action through for that tool? What does Enter send in spot mode?
3. Page and shell disagreeing: a key the page lets through for a tool whose action the shell then refuses, or the reverse.
4. Escape doing two things in one press, or a rung that can never be reached.
5. Regression against section A: a key bound before that is now unbound or does something else, other than the intended ones (bare s and a now only explain themselves; backspace/delete in spot mode removes the selected spot; copy/paste settings moved to Ctrl/Cmd+Shift+C/V).
6. The pointerup blur (section D): a case where it takes focus from something it should not, or fails to return the keyboard.
At most 6 findings, most consequential first. For each give the exact state and key.`

const rulesPrompt = `${FENCE}

Run: cat target/scratch/${low}-rules.txt

These are the ADDED lines of an uncommitted change (Rust, an HTML/JS page, and a Markdown decision record). Check them against this repository's written rules, and nothing else:
1. British spelling in prose: comments, docs and user-visible sentences. Flag American spellings (color, gray, center, behavior, canceled, labeled, -ize/-yze where -ise/-yse is meant, etc.). EXEMPT: identifiers, CSS properties and values, HTML attributes, JavaScript/DOM API names, serde names, quoted key names.
2. Stubs: TODO, FIXME, todo!, unimplemented!, "not implemented", placeholder text.
3. A Rust string literal handed to a user (inside format!, Told::from, anyhow!, say(...)) in a .rs file that NAMES A KEYBOARD KEY (Enter, Esc, Escape, a letter key, Ctrl+...). The project rule is that the Rust shell never names a key; only the page does. Comments and doc comments are exempt, test code is exempt.
4. The decision record (docs/decisions/interface.md lines) naming a test function or a Rust/JS identifier in backticks that does not appear anywhere else in these added lines AND looks newly introduced by this change. You may run AT MOST ONE extra command to check names: grep -rn "<name>" crates/rawkit-shell/src crates/rawkit-shell/ui
For each finding quote the exact added line.`

const out = await pipeline(
  [ { key: 'keys', prompt: keysPrompt, model: 'sonnet', effort: 'medium', verify: true },
    { key: 'rules', prompt: rulesPrompt, model: 'haiku', effort: 'low', verify: false } ],
  d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', model: d.model, effort: d.effort, schema: FINDINGS }),
  (review, d) => {
    const found = ((review && review.findings) || []).map(f => ({ ...f, lens: d.key }))
    if (!d.verify) return found.map(f => ({ ...f, verdict: 'unverified (mechanical; author checks)' }))
    if (found.length > 3) log(`keys: ${found.length - 3} finding(s) past the first 3 come back UNVERIFIED`)
    return parallel(found.slice(0, 3).map((f, i) => () =>
      agent(`${FENCE}

Run: cat target/scratch/${low}-keys.txt

A reviewer of that code claims:
  WHERE: ${f.where}
  CLAIM: ${f.claim}
  SCENARIO: ${f.scenario}

Try to REFUTE it by tracing the scenario through the code in the file, statement by statement, in the order the handler executes them (note every early return). The claim is refuted if the code does the right thing on that exact path, or if the state described cannot occur. If the file does not contain enough to decide, you may read AT MOST ONE named function from crates/rawkit-shell with a single sed -n or grep -n -A command, and must say which. If still undecided, refuted=false. In "why", give the trace.`,
        { label: `verify:keys#${i + 1}`, phase: 'Verify', model: 'sonnet', effort: 'medium', schema: VERDICT })
        .then(v => ({ ...f, verdict: !v ? 'verifier died' : v.refuted ? 'REFUTED' : 'CONFIRMED', why: v && v.why }))
    )).then(done => done.filter(Boolean).concat(found.slice(3).map(f => ({ ...f, verdict: 'unverified (over cap)' }))))
  }
)
const all = out.filter(Boolean).flat()
log(`${all.length} finding(s): ${all.filter(f => f.verdict === 'CONFIRMED').length} confirmed, ${all.filter(f => f.verdict === 'REFUTED').length} refuted`)
return { slice, findings: all.filter(f => f.verdict !== 'REFUTED'), refuted: all.filter(f => f.verdict === 'REFUTED').map(f => ({ claim: f.claim, why: f.why })) }
