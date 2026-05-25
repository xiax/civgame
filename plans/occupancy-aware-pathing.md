# Occupancy-Aware Adjacent Work Routing

## Summary
Fix the crowding loop for wells and other adjacent-interaction targets by making collision recovery choose a valid free stand tile around the original work target, not around the already occupied route tile. This applies to all `task_interacts_from_adjacent` tasks.

## Key Changes
- Add a shared helper for adjacent stand-tile selection that checks:
  - passable stand tile
  - valid Z reach to the work tile
  - not occupied in `SpatialIndex`
  - not already claimed in the current movement tick
  - deterministic nearest/tie-break ranking
- Update `movement_system` arrival collision handling:
  - for adjacent-interaction tasks, retarget around `ai.dest_tile` and update `ai.target_tile` + `ai.target_z`
  - clear stale `PathFollow` state so the next tick plans to the new stand tile
  - if every adjacent stand tile is occupied, release back to idle/retry instead of entering `Working` on an invalid tile
- Keep non-adjacent move-order collision nudging behavior unchanged.
- Update `src/simulation/CLAUDE.md` with the new movement/routing rule.

## Tests
- Add a regression test with a wet well, one worker occupying the nearest well-adjacent tile, and a thirsty worker approaching from that side; assert the thirsty worker retargets to another free tile around the well and eventually drinks.
- Add a movement-level regression for a generic adjacent task, confirming a collision retarget is chosen around `dest_tile`, not around the blocked route tile.
- Run `cargo test --bin civgame` after the change.

## Assumptions
- Chosen scope: all adjacent-interaction tasks, not just drinking.
- No new crates.
- When all stand tiles are occupied, waiting/retry is preferred over overlapping or pretending work can begin.
