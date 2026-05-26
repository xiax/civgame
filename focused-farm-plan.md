**Focused Farming AI Plan**

**Summary**
- Root cause: farming posts valid work for every state-owned agricultural plot, allocates seed budget in unsorted plot-map order, and job claiming only weakly penalizes travel distance. The AI is not maliciously foolish, just missing a foreman.
- Implement the selected behavior: focused fields. The village should finish the nearest useful plot before expanding, only opening overflow plots when the active plot’s live farm postings are saturated.

**Key Changes**
- Add farm plot classification helpers in `src/simulation/farm.rs`:
  - Count per plot: unprepared tiles, plantable tiles, mature crops, planted/started tiles, center distance from faction home.
  - Provide deterministic focus ranking: started/plantable plots first, then nearest raw plots, then stable rect coordinates.
  - Extract the existing FieldWork worker-cap formula so posting creation and claim limits use the same saturation rule.

- Update `chief_job_posting_system` farming branch in `src/simulation/jobs.rs`:
  - Sort state-owned Agricultural plots before seed allocation or posting emission. Never rely on `AHashMap` iteration order.
  - In Spring, emit `Plant` before expanding preparation elsewhere; if a focused plot has plantable tiles and seeds, do not open far-plot Prepare work.
  - Emit Spring `Prepare` only for the focused plot, plus overflow plots only when the focused plot’s relevant open postings are already saturated.
  - Keep Summer caretaker behavior assigned-plot-only.
  - Keep Autumn harvest broad enough to avoid crop loss, but sorted and distance-aware so nearby harvests are claimed first.

- Update job claiming:
  - Increase the farm-specific distance penalty so equal-priority FieldWork postings strongly prefer nearby fields.
  - Keep profession and skill useful, but not enough to make a worker cross the map while equivalent nearby field work exists.

- Update farm assignment matching:
  - Make `chief_farm_plot_assignment_system` deterministic and distance-aware by choosing closest farmer/plot pairs instead of depending on query/free-list order.

- Update docs:
  - Revise the farming/posting section in `/Users/xiao1/civgame/AGENTS.md` to describe focused field selection, sorted seed allocation, and overflow behavior.

**Test Plan**
- Add system tests covering:
  - Three Spring plots: initial postings target the nearest/started plot, not all plots.
  - Limited seed stock: seed targets are allocated to the focused/nearest plantable plot before any farther plot.
  - Overflow: a second plot opens only once the active plot’s live FieldWork posting is saturated.
  - Equal farm postings: `job_claim_system` chooses the nearer field.
  - Existing regressions still hold: Winter posts no farm work, Spring posts Prepare when needed, Plant beats Prepare ties, private household gardens still nominate Farm, Autumn harvest postings still appear.

- Run:
  - `cargo test --bin civgame farm`
  - `cargo test --bin civgame spring_chief_posts_prepare_field_jobs`
  - `cargo test --bin civgame`

**Assumptions**
- No new crates.
- No UI changes for this pass.
- Focused communal farming should optimize village throughput over strict per-farmer plot ownership; private household/kitchen-garden farming remains independent.
