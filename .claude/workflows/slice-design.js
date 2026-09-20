export const meta = {
  name: 'slice-design',
  description: 'Design a slice that needs new architecture: one strong proposer reads the brief and the code, a challenger tries to break the proposal, the proposer answers once. Read-only.',
  whenToUse: 'Before building a slice that needs new Rust capability. The author writes target/scratch/<slice>-brief.md first. args: {slice}',
  phases: [
    { title: 'Propose', detail: 'one proposer, strong model, reads the brief and the named code', model: 'opus' },
    { title: 'Challenge', detail: 'one challenger told to break it', model: 'sonnet' },
    { title: 'Answer', detail: 'the proposer concedes or defends each attack, once', model: 'opus' },
  ],
}

const slice = args.slice
const brief = `target/scratch/${slice.toLowerCase()}-brief.md`
const FENCE = `This is READ-ONLY design work in the repository root: never edit, create or delete a file, never run cargo or the application. You may read source with sed -n / grep -n, but read with purpose: the brief names the files and lines that matter, and whole-file dumps of main.rs (7 000 lines) or library.rs are forbidden. NEVER state what a function does from its name or from seeing it called — read its body or say you have not.`

const DECISION = {
  type: 'object',
  properties: {
    letter: { type: 'string', description: 'the brief\'s letter, A to K' },
    options: { type: 'array', items: { type: 'object', properties: {
      name: { type: 'string' }, gains: { type: 'string' }, costs: { type: 'string' } }, required: ['name', 'gains', 'costs'] } },
    recommend: { type: 'string', description: 'which option, in one sentence' },
    because: { type: 'string', description: 'the reason, grounded in code you READ: cite file:line' },
    mechanism: { type: 'string', description: 'exactly how it works: which thread, which lock and for how long, which data crosses' },
  },
  required: ['letter', 'options', 'recommend', 'because', 'mechanism'],
}
const PROPOSAL = {
  type: 'object',
  properties: {
    summary: { type: 'string', description: 'the whole design in six sentences a maintainer could build from' },
    decisions: { type: 'array', items: DECISION },
    unknowns: { type: 'array', items: { type: 'string' }, description: 'things that can only be settled by measuring, and how to measure each' },
    slices: { type: 'array', items: { type: 'object', properties: {
      title: { type: 'string' }, leavesWorking: { type: 'string' }, touches: { type: 'string' }, verifiedBy: { type: 'string' } },
      required: ['title', 'leavesWorking', 'touches', 'verifiedBy'] } },
    read: { type: 'array', items: { type: 'string' }, description: 'every file:line-range you actually read' },
  },
  required: ['summary', 'decisions', 'unknowns', 'slices', 'read'],
}
const ATTACKS = {
  type: 'object',
  properties: { attacks: { type: 'array', items: { type: 'object', properties: {
    against: { type: 'string', description: 'decision letter(s)' },
    scenario: { type: 'string', description: 'a concrete sequence of events, with threads and timing, that breaks it' },
    consequence: { type: 'string', description: 'what the person at the window sees or loses' },
    evidence: { type: 'string', description: 'file:line you READ that makes the scenario possible; or "not read" ' },
    severity: { type: 'string', enum: ['data-loss', 'hang-or-budget', 'wrong-picture', 'waste', 'nit'] },
  }, required: ['against', 'scenario', 'consequence', 'evidence', 'severity'] } } },
  required: ['attacks'],
}
const ANSWERS = {
  type: 'object',
  properties: { answers: { type: 'array', items: { type: 'object', properties: {
    attack: { type: 'integer', description: 'index into the attacks list, from 0' },
    verdict: { type: 'string', enum: ['concede', 'defend', 'needs-measuring'] },
    why: { type: 'string', description: 'cite file:line you read' },
    change: { type: 'string', description: 'if conceded: the exact change to the design. Otherwise empty.' },
  }, required: ['attack', 'verdict', 'why', 'change'] } },
    revisedSummary: { type: 'string', description: 'the six-sentence design again, with the concessions folded in' } },
  required: ['answers', 'revisedSummary'],
}

phase('Propose')
const proposal = await agent(`${FENCE}

Read the brief first, all of it: cat ${brief}

You are the architect for this slice. Propose ONE design, deciding every lettered question A to K in the brief, each with the options you weighed and a recommendation grounded in code you have read. Prefer the design with the fewest moving parts that meets "what done looks like": no new dependency, no schema change, no new lock on the keypress path unless you can show you need one. Where the honest answer is "this can only be settled by measuring", say so in unknowns and say how to measure it — do not guess a number. The most important decisions are C (catalog access) and D (what is outstanding): for each, give the mechanism precisely enough that a second engineer could find the race in it if there is one. Budget: at most 25 file reads.`,
  { label: 'architect', phase: 'Propose', model: 'opus', effort: 'high', schema: PROPOSAL })
if (!proposal) return { slice, error: 'the proposer returned nothing' }
log(`proposal: ${proposal.decisions.length} decisions, ${proposal.slices.length} slices, ${proposal.unknowns.length} unknowns`)

phase('Challenge')
const challenge = await agent(`${FENCE}

Read the brief: cat ${brief}

An architect has proposed the design below for it. Your job is to BREAK it. Find concrete sequences of events — with threads, locks and timing — in which it loses data, hangs or blows the 100 ms keypress budget, shows the wrong picture, or wastes unbounded work. Attack the mechanisms, not the prose. Particular hunting grounds: (1) the library mutex being held while something slow happens, including things the proposal did not mention holding it (read what the render loop locks per frame); (2) SQLite: WAL with one connection versus two, a write from another thread while the keypress thread is mid-transaction, SQLITE_BUSY, the integrity check and backup that Catalog::open/close do; (3) the staleness window — an edit saved between "this is outstanding" and "this preview is recorded", and whether a stale record can ever be SHOWN; (4) the queue under scrolling — starvation, unbounded growth, an item in flight that is no longer wanted, duplicates; (5) a second wgpu device starving the interactive one, and memory at 4 x 300 MB beside a 24 MP edit session; (6) quit while a worker is mid-write to a JPEG or mid-record; (7) RAW files on an unplugged drive — a failure that repeats forever on every launch; (8) the oracle in the shell's tests (brief fact 9). For each attack give the file:line you READ that makes it possible; an attack resting on code you did not read must say "not read". At most 8 attacks, the most consequential first; do not pad with nits. Budget: at most 20 file reads.

THE PROPOSAL:
${JSON.stringify(proposal, null, 1)}`,
  { label: 'challenger', phase: 'Challenge', model: 'sonnet', effort: 'high', schema: ATTACKS })
const attacks = (challenge && challenge.attacks) || []
log(`challenger: ${attacks.length} attack(s) — ${attacks.map(a => a.severity).join(', ')}`)

phase('Answer')
const answer = attacks.length ? await agent(`${FENCE}

Read the brief: cat ${brief}

You proposed the design below. A challenger has attacked it. For EACH attack, read the code it cites (and whatever else you need) and either CONCEDE — and state the exact change to the design — or DEFEND, citing the code that makes the scenario impossible, or say it NEEDS MEASURING and how. Do not defend out of pride: a conceded attack with a precise fix is worth more than a defence. Then restate the design in six sentences with every concession folded in. Budget: at most 20 file reads.

YOUR PROPOSAL:
${JSON.stringify(proposal, null, 1)}

THE ATTACKS (index from 0):
${JSON.stringify(attacks, null, 1)}`,
  { label: 'architect answers', phase: 'Answer', model: 'opus', effort: 'high', schema: ANSWERS }) : null

return { slice, proposal, attacks, answer }
