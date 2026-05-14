**Fix Era-Aware Settlement Seeding**

**Problem**
At OnEnter time, `FactionData.techs`, `FactionData.tech_adoption`, and `Settlement.peak_population` are all stale (empty / `Unknown` / 0) because their maintaining systems run in the Economy schedule, after OnEnter. As a result, `phase_for` (reads `faction.techs`, organic_settlement.rs:799) and `generate_candidates` (reads `community_adoption_bitset` via `tech_adoption`, construction.rs:2608) both treat every starting faction as Paleolithic / Camp phase regardless of `GameStartOptions.era`. Neo+ starts fall into the Paleo branch of `generate_candidates` and emit radial `Single(BuildSiteKind::Bed)` intents (construction.rs:2803) instead of walled houses.

**Approach**
Reuse the existing tech / adoption / peak-population systems by scheduling them inside the OnEnter chain *before* the survey + seed pass. No new priming code path; same source of truth as runtime.

**Key Changes**
- In `simulation/mod.rs:248` OnEnter chain, insert in order after `auto_found_default_settlements_system` and before `kickoff_initial_survey_system`:
  1. `sync_faction_techs_from_chief_system` (faction.rs:3553) — projects chief's `PersonKnowledge.aware` onto `FactionData.techs`.
  2. `derive_tech_adoption_system` (technology_adoption.rs:404) — fills `tech_adoption` from member-aggregate Learned counts.
  3. `settlement_peak_population_system` — ratchets `Settlement.peak_population` from current `member_count` so civic milestones see the real population.
- Verify each system is OnEnter-safe (idempotent, no per-tick assumption). If any of them queries `Time<Fixed>` or `Calendar` deltas, factor the body into a helper callable from both schedules.

**Out of Scope (intentionally dropped from the prior draft)**
- *Seed-vs-runtime civic gate split.* CLAUDE.md documents the population-threshold gate on Markets/Barracks/Monuments as a deliberate departure from grandfathered seeding. Tech priming alone fixes walls/roads/parcels; civic milestones stay population-gated.
- *L-shape composite shelter guard.* `seed_apply_intent` (construction.rs:3417) already rejects `CompositeHouse` at seed time, and `plan_composite_building` (construction.rs:3591) does emit interior `Bed` tiles. No evidence of a no-bed L-shape at seed. Revisit only if a runtime repro surfaces.
- *Single(Bed) shelter-pressure guard.* The stray `Single(Bed)` emission is a symptom of empty `faction.techs` routing Neo+ starts into the Paleo branch. Tech priming eliminates it without code changes to the intent emitter.

**Test Plan**
- New tests in the seed-suite (alongside `walled_house_tile_plan`):
  - Neolithic and Bronze starts: assert `current_era(faction.techs) == options.era` immediately after the OnEnter chain completes.
  - Neolithic+ start: assert at least one walled house (perimeter Wall + Door + interior Bed) is stamped per settlement.
  - Neolithic+ start: assert zero `Single(BuildSiteKind::Bed)` intents are applied at seed (no radial paleo beds).
  - Settled Neo+ faction post-OnEnter: `SettlementBrain` has non-Camp phase, road tiles, and Residential parcels with `frontage_edge.is_some()`.
- Keep existing layout tests green: `cargo test --bin civgame walled_house_tile_plan`.
- Full suite: `cargo test --bin civgame` plus `cargo check`.

**Assumptions**
- `GameStartOptions.era` defines starting civilization capability — chief-aware tech, community-adopted tech, and seeded peak population — not merely "chief has heard of it."
- Runtime adoption decay/growth remains untouched; reusing the existing systems guarantees no divergence between OnEnter priming and per-tick maintenance.
