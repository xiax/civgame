# Goal/Dispatcher Contract Cleanup

## Summary

Fix the recurring “goal selected but no task dispatched” class by making goal scorers prove executable opportunity, not just motivation. Start with the known risky goals, add shared readiness helpers used by both scorers and dispatchers, and add contract tests so this does not keep reappearing under new goals.

## Key Changes

- Add a small goal contract layer in `src/simulation`, with per-goal readiness helpers that return either `Ready { target/context }` or `Blocked(reason)`.
- Keep the implementation incremental: do not rewrite the whole HTN system. For now, use shared helper functions from both `goal_update_system`/scorers and the relevant dispatcher systems.
- Treat a goal as valid only if one of these is true:
  - it has a concrete executable target/opportunity now;
  - it has an intentional fallback task such as `Explore`, in-place `Sleep`, or route-to-care-site;
  - it is explicitly a parked/waiting state and documents that behavior.

## Goals To Fix First

- `TameAnimal`: require a local tech-eligible untamed candidate within the same radius the dispatcher uses.
- `Socialize`: require a reachable nearby partner or add an explicit “wander/seek partner” fallback; recommended default is require a nearby partner.
- `ProvideCare`: require an injured same-faction patient within healer scan range, unless the healer has an explicit route-to-care-site behavior.
- `Play`: require one concrete play option: nearby child/partner, toy, held/nearby entertainment resource, or seed/plantable play action.
- `FarmWork`: split readiness by actual executable farm phase: prepare, plant, harvest. Private plot work must not score unless the matching action can dispatch without a job claim.
- `Craft`: require a live actionable `CraftOrder` path: satisfied order to craft, material delivery target, or harvest/gather target for a needed input.
- `PersonalBuild`: require owned blueprint plus an executable material path: held material, accessible storage material, or known gather target.
- `Drink`: require routable water/storage, or add a clear fallback such as seeking toward home/water memory. Recommended default is to gate on a reachable drink source.

## Implementation Details

- Centralize duplicated constants such as tame radius, social partner radius, healer scan radius, and drink scan radii so scorer and dispatcher cannot drift.
- Replace broad `GoalContext` booleans like “has tameable animal” or “faction has injured member” with executable variants such as “has local tame target” or “has local care patient.”
- Where dispatchers currently silently `continue` on no target, record a structured blocked reason in debug/dev builds so future idling can be diagnosed by goal and reason.
- Document intentional no-task states in `src/simulation/CLAUDE.md`, especially `SeekCare` waiting at care sites and any future parked behavior.

## Test Plan

- Add contract tests for each high-risk goal:
  - scorer does not select the goal when only broad motivation exists but no executable opportunity exists;
  - scorer does select the goal when a minimal executable opportunity exists;
  - dispatcher creates a task for that selected goal within the same fixture.
- Add regression tests for the known failures:
  - far tameable animal does not trigger `TameAnimal`;
  - injured faction member outside healer scan range does not trigger `ProvideCare`;
  - social/play needs without nearby partner/options do not create idle selected goals;
  - private unprepared farm plot does not select `FarmWork` unless prepare can actually dispatch.
- Run `cargo test --bin civgame`.

## Progress

Shipped (structural fix — ends the *permanent* idle loop):
- `goal_contract.rs`: centralized scan radii, `BlockedReason`, `record_no_task_backstop`
  (throttled synthetic `MethodHistory` failure), `blocked()` dev-log helper,
  `goal_contract_backstop_system`.
- Backstop wired: inline at the no-task `continue` of the 4 single-dispatcher
  goals (`TameAnimal`/`ProvideCare`/`Socialize`/`Play`); generic post-ParallelB
  system for the multi-dispatcher goals (`Craft`/`Build`/`Farm`).
- `has_tameable_animal` gate is now radius-local (`TAME_SEARCH_RADIUS`), not a
  global "exists anywhere" check — the worst broad-gate offender.
- CLAUDE.md: generalized invariant + backstop + SeekCare parked-state doc.
- Tests: `goal_contract::tests` (throttle + chronic-threshold accumulation);
  full suite 958 passing.

Shipped (per-scorer executable gates — the deferred follow-up):
- `GoalScoringContext` gained `has_local_care_patient` / `has_social_partner`
  (replacing the broad `faction_has_injured`). Computed in
  `goal_update_system`'s `Scored` build site: `has_local_care_patient` from
  radius-filtered `CareNeed` opportunities + injured-agent positions within
  `HEAL_SCAN_RADIUS`; `has_social_partner` from a `SOCIAL_PARTNER_RADIUS`
  box sweep over a **fresh per-tile `Person`-count grid** built same-tick
  from live `Transform`s (not `SpatialIndex` — a cold index would flip a
  tick-1 `Socialize` agent off its goal). Bounded behind the social
  urgency threshold. The maintenance + EarnIncome build sites stub the
  gates (those paths never read them).
- `ProvideCareScorer` / `SocialScorer` decline when their gate is false.
- `Play` has **no** scorer gate by design: its solo fallbacks (stone-throw
  / seed-plant / toy) make a cheap `has_play_option` false-negative-prone
  and a faithful one a 5-way scan that duplicates `htn_play_dispatch_system`
  (drift risk). Instead the dispatcher's primary "no play option" exit —
  previously a silent `continue` — now records the `goal_contract::blocked`
  backstop, so every `Play` no-task exit is backstopped.
- Farm: the `households_with_seasonal_work` snapshot's Spring `plantable`
  branch now also requires the household (or parent village) to hold
  `grain_seed` — a seedless household no longer selects `Farm` for
  plant-only work (unprepared/Prepare tiles still count).
- `Craft` / `Build` need no new gate: `should_craft` already gates
  `CraftDemandScorer` on recipe-input availability, and `PersonalBuildScorer`
  must NOT gate on stored materials — a player-commissioned blueprint with
  no materials is built via the dispatcher's gather path; gating it would
  strand the player's order. Both goals are multi-dispatcher-backstopped.

Tests: `goal_scorers::tests::social_scorer_requires_partner` + updated
`provide_care_only_fires_*`.

## Assumptions

- No new crates.
- The first pass should prefer gating scorers over adding long-distance expedition behavior.
- Existing fallback goals like `Survive` exploration and in-place `Sleep` remain valid.
- This plan targets AI contract correctness, not broader behavior tuning or UI changes.
