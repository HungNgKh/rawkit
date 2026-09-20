export const meta = {
  name: 'slice-review',
  description: 'Read-only review of an uncommitted UI slice: rules and regression on haiku, per-keypress budget on sonnet, budget findings adversarially verified',
  whenToUse: 'After building a UI rework slice, before committing. args: {slice, summary, files}',
  phases: [
    { title: 'Review', detail: 'rules + regression (haiku), budget (sonnet)' },
    { title: 'Verify', detail: 'try to refute each budget finding (sonnet, max 3)', model: 'sonnet' },
  ],
}

const FINDINGS = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          claim: { type: 'string', description: 'one sentence: what is wrong' },
          evidence: { type: 'string', description: 'the exact text at that line, or the command output, that shows it' },
        },
        required: ['file', 'line', 'claim', 'evidence'],
      },
    },
  },
  required: ['findings'],
}
const VERDICT = {
  type: 'object',
  properties: { refuted: { type: 'boolean' }, why: { type: 'string' } },
  required: ['refuted', 'why'],
}

const slice = (args && args.slice) || 'S?'
const summary = (args && args.summary) || ''
const COMMON = `READ-ONLY review in the repository root of the UNCOMMITTED change for UI slice ${slice}. Do not edit, create or delete any file. Do not run cargo (the author is building in the same target directory). 
See the change with: git diff ; and the two new files with: cat crates/rawkit-shell/src/page_contract.rs docs/decisions/interface.md
What the slice does: ${summary}
Report ONLY problems you can point at with a file and line. No praise, no style opinions, no restating the diff. An empty findings list is a good answer. Do not read outside the diff and the named files unless a finding truly needs it.`

const DIMENSIONS = [
  {
    key: 'rules', model: 'haiku', effort: 'low', verify: false,
    prompt: `${COMMON}

YOUR LENS — the repository's written rules, in AGENTS.md (read its "Working rules"/conventions sections). Check ONLY the added lines (those starting with + in the diff, and the two new files) for:
1. American spelling where British is required (color, gray, center, behavior, canceled, labeled, normalize, -ize where the repo uses -ise, etc.). NOTE: identifiers, CSS properties (color:, text-align: center), HTML attributes, serde names and quoted external names are exempt — only prose in comments, docs and user-visible sentences counts.
2. Stubs: TODO, FIXME, unimplemented!, todo!, "not implemented", placeholder text.
3. Comments that say WHAT the next line does instead of WHY (only flag clear cases).
4. A test that assumes the host filesystem (hard-coded /home, /tmp paths, reading files outside the crate other than via include_str!).
5. A new dependency added anywhere other than the root Cargo.toml.
6. docs/decisions/interface.md claiming something the diff does not do (a test name that does not exist in the diff or the tree — check with grep -rn "fn <name>" crates/ ; a function name that does not exist).`,
  },
  {
    key: 'regression', model: 'haiku', effort: 'low', verify: false,
    prompt: `${COMMON}

YOUR LENS — did the page lose anything. Work from: git diff -- crates/rawkit-shell/ui/panel.html
1. Every key bound BEFORE is still bound AFTER. Compare: git show HEAD:crates/rawkit-shell/ui/panel.html | grep -n 'case "' ; against: grep -n 'case "' crates/rawkit-shell/ui/panel.html . Report any key that vanished or changed action.
2. Any remaining reference to the removed elements or names: grep -n 'getElementById("log")\\|getElementById("export")\\|\\blog\\.textContent\\|\\blog\\.className\\|#log\\b\\|#export\\b\\|exportLine' crates/rawkit-shell/ui/panel.html . exportLine is expected to remain but must now point at the element with id "job"; confirm an element with id="job" exists in the markup. Every getElementById("x") ADDED by the diff must have a matching id="x" in the markup — check each.
3. Any call to say(...) or hint(...) or tell(...) that appears textually BEFORE the line where that function is defined with const, at module top level (not inside a function body or callback) — that would be a temporal-dead-zone crash at load. Give the line of the call and the line of the definition.
4. In crates/rawkit-shell/src: any remaining caller that still treats the notice as a plain String, or still does .map_err(|e| e.to_string()) inside the fn cull handler (its error type is now Told).`,
  },
  {
    key: 'budget', model: 'sonnet', effort: 'medium', verify: true,
    prompt: `${COMMON}

YOUR LENS — cost per keypress and correctness of the message flow. The app has a hard 100 ms budget per keypress at 20 000 photographs, and Library::act() in crates/rawkit-shell/src/library.rs runs on every key. Read the diff to library.rs carefully, and the surrounding act()/view()/name_of()/collection_name() code as needed.
1. Anything the diff adds to the per-keypress path that is not O(1) or a single indexed lookup: e.g. name_of() calls position_of() — check what Sequence::position_of costs (crates/rawkit-shell/src/sequence.rs); an extra catalog query per key (the Colour arm now calls cull::judgement a second time); a linear scan of collections per key. For each, say whether it runs on EVERY key or only on the rare action, because only the former matters. Be specific about which.
2. A sentence that can be FALSE: said before the write happens and the write then does nothing or is refused (e.g. "Picked X" is set before judge(); what if the frame was already picked? what does TargetToggle say if take_out_of returns 0?), wrong count, wrong name because the name was read after the frame left the sequence (name_of falls back to "a photograph not showing").
3. said leaking across actions or being lost: act() clears it at the top and takes it at the end — find any early return (the ? operator, return Err) or any path where a helper's say() overwrites the arm's, or where the oracle block could return before the take.
4. In main.rs: tell()/NOTICE — a deadlock (lock held while calling something that locks NOTICE again), or failure() being called while the library lock is held in a way that could block the render loop.
Report at most 6 findings, the most consequential first.`,
  },
]

const reviewed = await pipeline(
  DIMENSIONS,
  d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', model: d.model, effort: d.effort, schema: FINDINGS }),
  (review, d) => {
    const found = (review && review.findings) || []
    if (!d.verify) return found.map(f => ({ ...f, lens: d.key, verdict: 'unverified (mechanical; author checks)' }))
    const taken = found.slice(0, 3)
    if (found.length > taken.length) log(`budget: ${found.length - taken.length} finding(s) beyond the first 3 are returned UNVERIFIED`)
    return parallel(taken.map(f => () =>
      agent(`READ-ONLY in the repository root. Do not edit files or run cargo. A reviewer of the uncommitted diff (git diff) claims:

  ${f.file}:${f.line} — ${f.claim}
  Evidence given: ${f.evidence}

Try to REFUTE it by reading the actual code (the diff, and the functions it calls — e.g. crates/rawkit-shell/src/sequence.rs for position_of, crates/rawkit-shell/src/library.rs for judge/act). A claim about cost is refuted if the code is O(1)/an indexed lookup, or only runs on a rare deliberate action rather than every keypress. A claim about a false sentence is refuted only if you can show the sentence is true on that path. If you cannot decide from the code, refuted=false. Say in "why" exactly what you read that settles it.`,
        { label: `verify:${f.file.split('/').pop()}:${f.line}`, phase: 'Verify', model: 'sonnet', effort: 'medium', schema: VERDICT })
        .then(v => ({ ...f, lens: d.key, verdict: !v ? 'verifier died' : v.refuted ? 'REFUTED' : 'CONFIRMED', why: v && v.why }))
    )).then(done => done.filter(Boolean).concat(found.slice(3).map(f => ({ ...f, lens: d.key, verdict: 'unverified (over cap)' }))))
  }
)

const all = reviewed.filter(Boolean).flat()
log(`${all.length} finding(s): ${all.filter(f => f.verdict === 'CONFIRMED').length} confirmed, ${all.filter(f => f.verdict === 'REFUTED').length} refuted`)
return { slice, findings: all.filter(f => f.verdict !== 'REFUTED'), refuted: all.filter(f => f.verdict === 'REFUTED').map(f => ({ at: `${f.file}:${f.line}`, claim: f.claim, why: f.why })) }
