export const meta = {
  name: 'slice-ground',
  description: 'Read-only: classify every eprintln! in rawkit-shell as user-causable or developer noise, so the status line knows what to carry',
  whenToUse: 'Before building a UI rework slice that reroutes messages. args: {slice: "S1"}',
  phases: [{ title: 'Shell map', detail: 'one haiku agent over ~42 eprintln! sites', model: 'haiku' }],
}

const SITES = {
  type: 'object',
  properties: {
    sites: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          message: { type: 'string', description: 'the literal text printed, shortened to 80 chars' },
          kind: { type: 'string', enum: ['user_causable_failure', 'startup_diagnostic', 'timing_or_debug', 'unreachable_or_internal'] },
          trigger: { type: 'string', description: 'what a person does in the window (or on launch) that reaches this line; "none" if nothing' },
          already_reaches_page: { type: 'boolean', description: 'true if the same failure is ALSO returned as Err to the page or passed to notice()' },
        },
        required: ['file', 'line', 'message', 'kind', 'trigger', 'already_reaches_page'],
      },
    },
  },
  required: ['sites'],
}

phase('Shell map')
const result = await agent(
  `READ-ONLY task in the repository root. Do not edit, create or delete any file. Do not run cargo.

Run: grep -n "eprintln!" crates/rawkit-shell/src/main.rs crates/rawkit-shell/src/library.rs

For EACH hit (expect about 42), read roughly 15 lines either side with sed -n, and classify it:
- user_causable_failure: something a person using the window can trigger and would want to know about (a profile file that will not parse, a catalog write that failed, an export that failed, a preview that could not be read, a file that has gone missing). 
- startup_diagnostic: informational lines printed once at launch (gpu, surface, route, display, image, decode timing).
- timing_or_debug: per-frame or per-action timings, "haste", "frames", histogram timings and the like.
- unreachable_or_internal: a bug-guard nobody using the window can cause.

For each, say in "trigger" what the person did to get there, and set already_reaches_page=true only if you can SEE in the surrounding code that the same failure is also returned as an Err from a #[tauri::command] or handed to notice(...). If unsure, false.

Do not read outside those two files unless a classification truly needs it. Return every site; do not summarise or skip.`,
  { label: 'eprintln sites', phase: 'Shell map', model: 'haiku', effort: 'low', schema: SITES }
)

const sites = (result && result.sites) || []
const wanted = sites.filter(s => s.kind === 'user_causable_failure')
log(`${sites.length} sites classified; ${wanted.length} user-causable, ${wanted.filter(s => !s.already_reaches_page).length} of those never reach the page`)
return { slice: (args && args.slice) || 'S1', total: sites.length, user_causable: wanted, other_counts: {
  startup_diagnostic: sites.filter(s => s.kind === 'startup_diagnostic').length,
  timing_or_debug: sites.filter(s => s.kind === 'timing_or_debug').length,
  unreachable_or_internal: sites.filter(s => s.kind === 'unreachable_or_internal').length,
} }
