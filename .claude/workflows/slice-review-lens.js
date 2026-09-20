export const meta = {
  name: 'slice-review-lens',
  description: 'Read-only review of a UI slice from prepared, scoped input files: one slice-specific lens on sonnet (findings adversarially verified, max 3) and a rules lens on haiku',
  whenToUse: 'After building a UI slice. The author prepares target/scratch/<slice>-<lens>.txt and <slice>-rules.txt, and passes the lens brief. args: {slice, lens, about, lookFor}',
  phases: [
    { title: 'Review', detail: 'the slice lens (sonnet) + rules (haiku), one input file each' },
    { title: 'Verify', detail: 'refute each lens finding, max 3', model: 'sonnet' },
  ],
}

const FINDINGS = {
  type: 'object',
  properties: { findings: { type: 'array', items: { type: 'object',
    properties: {
      where: { type: 'string', description: 'section letter and the exact line of text the finding is about' },
      claim: { type: 'string', description: 'one sentence: what goes wrong' },
      scenario: { type: 'string', description: 'starting state + what the person does -> what happens -> what should' },
    }, required: ['where', 'claim', 'scenario'] } } },
  required: ['findings'],
}
const VERDICT = { type: 'object', properties: { refuted: { type: 'boolean' }, why: { type: 'string' } }, required: ['refuted', 'why'] }

const slice = args.slice
const low = slice.toLowerCase()
const lensFile = `target/scratch/${low}-${args.lens}.txt`
const FENCE = `HARD LIMITS. This is read-only: never edit, create or delete a file, never run cargo. Your ENTIRE input is one prepared file; read it with exactly one command and do not open the repository's source files, do not run git diff, do not explore. The file already contains every line that matters. An empty findings list is a good answer. No praise, no style opinions, no restating the code.`

const lensPrompt = `${FENCE}

Run: cat ${lensFile}

${args.about}

Report ONLY things that are wrong, each with a concrete scenario a person at the window could reproduce. Look specifically for:
${args.lookFor}
At most 6 findings, most consequential first.`

const rulesPrompt = `${FENCE}

Run: cat target/scratch/${low}-rules.txt

These are the ADDED lines of an uncommitted change (Rust, an HTML/JS page, and a Markdown decision record). Check them against this repository's written rules, and nothing else:
1. British spelling in prose: comments, docs and user-visible sentences. Flag American spellings (color, gray, center, behavior, canceled, labeled, -ize/-yze where -ise/-yse is meant, etc.). EXEMPT: identifiers, CSS properties and values, HTML attributes, JavaScript/DOM API names (scrollIntoView, behavior: "smooth" is an API value), serde names, quoted key names.
2. Stubs: TODO, FIXME, todo!, unimplemented!, "not implemented", placeholder text.
3. A Rust string literal handed to a user (inside format!, notice(...), Err(...), anyhow!) in a .rs file that NAMES A KEYBOARD KEY (Enter, Esc, Escape, a single letter key, Ctrl+...). The project rule is that the Rust shell never names a key; only the page does. Comments, doc comments and test code are exempt.
4. The decision record (docs/decisions/interface.md lines) naming a test function or identifier in backticks that looks newly introduced by this change but does not appear anywhere else in these added lines. You may run AT MOST ONE extra command to check such names: grep -rn "<name>" crates/rawkit-shell/src crates/rawkit-shell/ui
For each finding quote the exact added line.`

const out = await pipeline(
  [ { key: args.lens, prompt: lensPrompt, model: 'sonnet', effort: 'medium', verify: true },
    { key: 'rules', prompt: rulesPrompt, model: 'haiku', effort: 'low', verify: false } ],
  d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', model: d.model, effort: d.effort, schema: FINDINGS }),
  (review, d) => {
    const found = ((review && review.findings) || []).map(f => ({ ...f, lens: d.key }))
    if (!d.verify) return found.map(f => ({ ...f, verdict: 'unverified (mechanical; author checks)' }))
    if (found.length > 3) log(`${d.key}: ${found.length - 3} finding(s) past the first 3 come back UNVERIFIED`)
    return parallel(found.slice(0, 3).map((f, i) => () =>
      agent(`${FENCE}

Run: cat ${lensFile}

${args.about}

A reviewer of that code claims:
  WHERE: ${f.where}
  CLAIM: ${f.claim}
  SCENARIO: ${f.scenario}

Try to REFUTE it by tracing the scenario through the code in the file, statement by statement, in the order it executes (note every early return, every guard, and which side — page or shell — runs first). The claim is refuted if the code does the right thing on that exact path, or if the starting state cannot occur. 'The old code did the same' is NOT a refutation: a behaviour can be harmless before a change and wrong after it, because something it relies on now means something else. Judge whether what happens on that path is RIGHT for the person at the window, not whether this change introduced it. If the file does not contain enough to decide, you may read AT MOST THREE named functions from crates/rawkit-shell, each with a single sed -n or grep -n -A command, and must say which. NEVER conclude anything about what a function does from its name or from seeing it called: if your verdict depends on a called function's body and you have not read that body, read it (it counts as one of your three) or answer refuted=false AND say in 'why' that the verdict is UNDECIDED and which body you would need. A verifier once confirmed 'the workspace never changes' after seeing a call to pick_up_from_anywhere() and not opening it; its first line was enter(DEVELOP). If still undecided, refuted=false. In "why", give the trace.`,
        { label: `verify:${d.key}#${i + 1}`, phase: 'Verify', model: 'sonnet', effort: 'medium', schema: VERDICT })
        .then(v => ({ ...f, verdict: !v ? 'verifier died' : v.refuted ? 'REFUTED' : 'CONFIRMED', why: v && v.why }))
    )).then(done => done.filter(Boolean).concat(found.slice(3).map(f => ({ ...f, verdict: 'unverified (over cap)' }))))
  }
)
const all = out.filter(Boolean).flat()
log(`${all.length} finding(s): ${all.filter(f => f.verdict === 'CONFIRMED').length} confirmed, ${all.filter(f => f.verdict === 'REFUTED').length} refuted`)
return { slice, findings: all.filter(f => f.verdict !== 'REFUTED'), refuted: all.filter(f => f.verdict === 'REFUTED').map(f => ({ claim: f.claim, why: f.why })) }
