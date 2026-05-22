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

Deferred (idle-churn polish — backstop already guarantees correctness):
- Per-scorer executable gates needing `GoalScoringContext` fields:
  `ProvideCare` (radius-filter `CareNeed` opportunities), `Socialize`/`Play`
  (`has_social_partner` / `has_play_option`), `Craft`
  (`faction_has_craft_order_path`), `Build` (`has_build_material_path`),
  `Farm` (seed-availability tightening of the seasonal-work snapshot).
  Each needs the `GoalScoringContext` struct + its 3 build sites in
  `goal_update_system` + the scorer body. Without them an agent may pick a
  dead-end goal for up to ~60 ticks before the backstop releases it; with the
  backstop it never loops permanently.

## Assumptions

- No new crates.
- The first pass should prefer gating scorers over adding long-distance expedition behavior.
- Existing fallback goals like `Survive` exploration and in-place `Sleep` remain valid.
- This plan targets AI contract correctness, not broader behavior tuning or UI changes.
