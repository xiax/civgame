# Generic Planting Fix

## Summary

- The plant catalog is already generic: `PlantKind::Grain` and `PlantKind::BerryBush` both have seed resources in [plants.rs](/Users/xiao1/civgame/src/simulation/plants.rs:105); `Tree` is not plantable because it has no seed resource.
- The bug is in the task chain above that catalog:
  - `Task::Planter` / `Task::PlayPlant` carry only a tile, so [production.rs](/Users/xiao1/civgame/src/simulation/production.rs:150) guesses which held seed to plant by scanning `PlantKind::ALL`.
  - Farm dispatch can select berry seeds, but seasonal farm posting/scoring/harvest progress are still partly grain-only in [jobs.rs](/Users/xiao1/civgame/src/simulation/jobs.rs:2956), [goals.rs](/Users/xiao1/civgame/src/simulation/goals.rs:680), and [gather.rs](/Users/xiao1/civgame/src/simulation/gather.rs:809).
  - Plant target tiles are not reserved, so multiple workers can withdraw seeds, walk to the same chosen tile, perform the planting action, and only the first one actually spawns a plant.

## Key Changes

- Make planting intent explicit.
  - Change `Task::Planter` and `Task::PlayPlant` to carry `{ tile, seed_resource }`.
  - Add `PlantKind::from_seed_resource(ResourceId) -> Option<PlantKind>`.
  - Update farm and play HTN methods so the selected seed resource is threaded into the planting task instead of rediscovered later.
  - Update the planter executor to use the typed task tile and seed resource, consume exactly that seed only after a successful spawn, and record a failure instead of silently completing when the seed is missing or the tile is occupied.

- Add planting tile reservations.
  - Introduce a `PlantingReservations` resource keyed by tile, storing worker entity and seed resource.
  - Farm planting and play planting destination scans must skip reserved tiles.
  - Reserve the target tile when the withdraw chain is dispatched; release it on successful planting, failed routing to the plant tile, missing seed, occupied target, task cancellation, or a small GC pass that removes reservations whose worker no longer has the matching planting task queued/current.
  - This directly fixes the “workers do the planting action but no plant appears” race.

- Make farm crop lifecycle match the generic seed selection.
  - Treat any `PlantKind` with `seed_resource().is_some()` as a farm-plantable crop for posting/scoring purposes; currently that means grain and berry bushes.
  - Replace `grain_seed_stock` / `household_has_grain_seed` gates with plantable-seed totals over `PlantKind::ALL`.
  - Replace Autumn `mature_grain` posting counts with mature farm-crop counts inside the Agricultural plot.
  - Change `gather.rs` FieldWork harvest progress credit from grain-only to any farm-crop kind harvested inside the claimed Farm posting area.
  - Keep crop-specific yield behavior in `PlantKind`: grain remains Farming/nutrient/plow-oriented; berry bushes keep fruit/berry-seed yields and regrowth behavior.

- Fix storage handoff edge cases found while checking the full path.
  - Let farm-private seed withdrawal target the actual source faction selected by `FarmScope` so household storage works; use an explicit `source_faction_id` on `WithdrawMaterial` or an equivalent typed-task field.
  - Do not promote a `WithdrawMaterial -> Planter` chain if the worker failed to acquire the intended seed and is not already carrying it.
  - Preserve existing storage reservations for stockpile/build/craft flows.

- Update docs/comments.
  - Update task comments, farming notes in `src/simulation/CLAUDE.md`, and the root farming summary so they describe generic plantable crops rather than implying wheat-only planting.

## Test Plan

- Direct planter tests:
  - Plant grain from `Task::Planter { seed_resource: grain_seed }`; assert grain plant, seed consumed, `Cultivated` inserted, and existing tilled/plowed grain behavior remains.
  - Plant berry from `Task::Planter { seed_resource: berry_seed }`; assert `BerryBush`, berry seed consumed, `Cultivated` inserted.
  - Give a worker both grain and berry seeds, issue a berry planting task, and assert it plants berry rather than the first seed in `PlantKind::ALL`.

- Play planting tests:
  - Extend the existing grain play-chain test with a berry-seed version.
  - Assert `PlayPlant` uses its typed seed resource and does not insert `Cultivated`.

- Race/reservation tests:
  - Two workers, two seeds, two plantable tiles: dispatch in the same tick and assert they reserve different tiles.
  - Two workers, one plantable tile: assert only one reserves it and the other does not commit to a doomed planting action.
  - Empty the reserved storage stack before withdrawal completion and assert the trailing planter task is cancelled instead of doing a fake planting action.

- Farm lifecycle tests:
  - Spring Plant posting with only berry seeds available creates plant work, dispatches berry planting, and spawns `BerryBush`.
  - Mature berry bushes inside an Agricultural plot create Autumn Harvest work and credit `FieldWork { phase: Harvest }` when harvested.
  - Private household farm planting can withdraw from household storage and falls back to parent village storage when household storage lacks seeds.

- Verification commands:
  - Run focused tests for planter/play/farm posting paths.
  - Run `cargo test --bin civgame` before considering the fix complete.

## Assumptions

- The intended behavior is that every resource mapped by `PlantKind::seed_resource()` can be deliberately planted; currently that is grain and berry seeds.
- Trees remain non-plantable until a tree seed/sapling resource is added to the catalog and mapped in `PlantKind`.
- No new crates are needed.
