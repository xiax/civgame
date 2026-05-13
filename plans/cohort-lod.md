# Cohort LOD

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 1.
**Depends on:** Behavioural-richness refactor (Phases A–F) shipped first.

## Trigger

Pick this up when in-game population exceeds ~500 active members and frame time becomes the limiting factor for further growth, OR when the player asks for visibly larger civilisations than the current cap supports.

## Scope

Make `LodLevel::Aggregate` actually reduce per-agent work via cohort batching. Today `Aggregate` agents run the full per-tick pipeline (`PersonAI`, goals, HTN, movement) — survey confirmed there is no batching, only distance-culling. Goal: an order-of-magnitude pop increase at the same frame budget while visible (camera-focused) agents still behave individually.

## Current state (from survey)

- `src/simulation/lod.rs:9-14` defines `enum LodLevel { Full, Aggregate, Dormant }`.
- ~25 systems gate on `LodLevel::Dormant` (skip-only); `Aggregate` does not short-circuit any tick.
- No cohort records anywhere; every member is an entity.

## Files to touch

- `src/simulation/lod.rs` — add `CohortKey { faction_id, settlement_id, profession, age_band, wealth_band }` + `CohortState { population, avg_hunger, avg_sleep, food_consumption_rate, birth_rate, death_rate, skill_means, disposition_distribution }`.
- `src/simulation/faction.rs` — `FactionData` gains `cohorts: AHashMap<CohortKey, CohortState>`.
- New `src/simulation/cohort.rs` — `cohort_tick_system` (Economy, daily): per-cohort consumption / production / births / deaths / migration pressure; drains from + contributes to `FactionStorage`.
- ~25 systems currently `Dormant`-gated — extend each to also short-circuit on `Aggregate` and route through cohort tick. List via `grep -rn "LodLevel::Dormant" src/`.
- `src/simulation/reproduction.rs` — births/deaths into/out of cohorts when parent agents are Aggregate.
- New `src/simulation/cohort_sample.rs` — `sample_from_cohort(rng, &CohortState) -> AgentSpawnTemplate` for promotion.

## Open questions a real plan must resolve

- **Storage model.** Cohorts as resource map entries (cheap, no ECS overhead) vs entities with `CohortMarker` (queryable). Recommend resource map.
- **Promotion triggers.** Camera approach, named-NPC, player command target, raid participation, social event, faction chief change. Pick the trigger set.
- **Demotion triggers.** `Full` agent collapses to cohort when it leaves camera range AND is not named/story-important AND has no active player command AND is not chief/captain.
- **Named-agent pinning.** Story-important / named agents must be pinned to `Full` and never collapsed. Need a `Pinned` marker component.
- **Identity continuity on re-promotion.** Same named "person" returns vs fresh sample? Recommend: pin named, sample others.
- **Skill / disposition synthesis on promotion.** Sample Disposition from cohort distribution, skills from `skill_means ± stddev`, hunger from `avg_hunger`. Define exact sampler.
- **Player faction policy.** Probably all player-faction members stay `Full` regardless of distance — player will inspect them.
- **Economy interaction.** Cohort consumption hits the same `FactionStorage` totals real agents read, so survival overrides see accurate food state.
- **Combat interaction.** Raids on cohort-only settlements — explode cohorts to entities for the combat duration, or resolve combat at cohort level?

## Acceptance criteria

- 2000 agents at the same frame time as 500 today.
- Visible (camera-focused) agents still behave individually.
- Cohort populations consume/produce/reproduce; faction storage stays balanced.
- Promotion test: a cohort member becomes a plausible agent with skills, disposition, hunger consistent with the cohort's distribution.
- Pinned named agents survive cohort collapse and re-appear identically when camera returns.
- Calibration test: spawn 100 agents, force half to Aggregate, run 1 game-year, verify total food consumption matches the all-Full baseline within 5%.
