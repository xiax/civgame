# Fix Drink Stalls At Wells And Water

## Summary
- The new snapshot confirms two linked bugs: workers can hold `Task::Drink` while `State: Idle`, and wells are being routed through the generic adjacent-work logic even though a well shaft is not a normal standable work tile.
- Fix drinking as its own robust state machine: if a worker is already adjacent to the well/water, they should drink; if not, the task should cancel cleanly and re-route.

## Key Changes
- In [drink.rs](/Users/xiao1/civgame/src/simulation/drink.rs), add Drink orphan recovery like Sleep:
  - `Seeking`/`Routing`: let movement continue.
  - `Working`: keep current sip logic.
  - `Idle` next to a valid `DrinkSource`: promote back to `Working`.
  - `Idle` not next to a valid source, source gone, or wrong typed variant: release stand reservation and `cancel_chain`.
- Replace generic well/water stand selection with drink-specific stand selection:
  - Pick a reachable dry passable tile adjacent to the well shaft or water tile.
  - Do not derive reachability from the shaft’s `nearest_standable_z`; wells/water are impassable sources, not work floors.
  - Prefer the worker’s current tile when already adjacent and valid, so “standing next to the well” immediately works.
- In [movement.rs](/Users/xiao1/civgame/src/simulation/movement.rs), when adjacent-task collision recovery cannot find a new stand tile, clear the whole task/path state instead of leaving `Idle + Task::Drink + PathFollow::Following`.

## Tests
- Worker already adjacent to a charged well with `Idle + Task::Drink` recovers to `Working` and quenches thirst.
- Worker adjacent to a charged well at a different shaft Z still drinks.
- Contended well stand tiles do not leave `Idle + Task::Drink`.
- Bad nearby water/well candidates are skipped or canceled cleanly so home fallback can route.
- Run `cargo test --bin civgame thirsty_worker` plus new drink/well regression tests, then `cargo test --bin civgame`.

## Assumptions
- Well drinking should follow the existing documented behavior: chebyshev-adjacent to the shaft is enough; the worker does not need the shaft’s own Z to be standable.
- No save migration is needed. Update `src/simulation/CLAUDE.md` to document the drink-specific stand-tile contract.
