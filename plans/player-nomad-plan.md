# Player-Directed Physical Nomad Migration Plan

## Summary
- Turn migration into an active expedition loop: prepare, scout, choose route, travel by waypoints, manage risk, and pitch only after reaching a viable discovered site.
- Keep physical packing/pitching: workers dismantle, load, carry, travel, unload, and rebuild.
- Add `nomad_autopilot` so AI can use the same systems automatically, while player nomads stay manual unless a future toggle enables automation.

## Core Gameplay Loop
- **Migration Mode**
  - Add a HUD button for nomadic player factions: `Plan Migration`.
  - Opening it shows a migration panel with camp readiness, cargo burden, scouts, known water/forage/herd pins, danger pins, and suggested seasonal directions.
  - The player chooses a migration intent, not just a final pitch tile:
    - `Follow Water`
    - `Follow Herds`
    - `Seek Winter Shelter`
    - `Seek Summer Pasture`
    - `Avoid Danger`
    - `Free Route`
  - Intent affects scouting recommendations and route scoring, but the player can override.

- **Scouting phase**
  - Add player commands for `SendMigrationScout { direction }` and `RecallScout`.
  - Scouts physically travel outward, reveal terrain, and report candidate camp sites, water, forage, herds, predators, and blocked routes.
  - Candidate sites appear as map pins with reason summaries like `fresh water`, `good pasture`, `wolf risk`, `poor shelter`, `long carry`.
  - Players can send multiple scouts before committing, making migration an exploration choice instead of a single click.

- **Route phase**
  - Add `SetMigrationRoute { waypoints }` instead of only `PitchCamp`.
  - The route is a chain of waypoints through known or partially known terrain.
  - Waypoints can be edited while the band is packed or traveling.
  - Add commands:
    - `BeginPackingForMigration`
    - `SetMigrationRoute`
    - `StartMigration`
    - `HaltMigration`
    - `ResumeMigration`
    - `MakeTemporaryCamp`
    - `PitchPermanentCamp`
  - The final pitch site must be reached and validated before full camp deployment.

- **Travel phase**
  - The band moves as a migration column toward the next waypoint.
  - Members and pack animals physically carry cargo.
  - Travel pace depends on children/elders, hunger, injuries, terrain, cargo burden, and pack animals.
  - During travel, the player can halt, forage locally, adjust route, wait for stragglers, or send scouts ahead.
  - Temporary camps provide rest and basic sleep but do not count as a full `CampState::Pitched`.

- **Pitching phase**
  - `PitchPermanentCamp` starts physical deployment jobs at the current reachable site.
  - The camp becomes functional after hearth plus minimum bedroll coverage.
  - Tents/yurts and extra structures deploy as follow-up labor.

## System Changes
- **State and types**
  - Add `FactionData.nomad_autopilot: bool`.
  - Add `CampOperation::{Idle, Scouting, Packing, Traveling, TemporaryCamp, Pitching}`.
  - Add `MigrationRoute { waypoints, current_index, intent, created_tick }`.
  - Add `CampSiteCandidate { anchor, score, reasons, validation }`.
  - Add `MigrationScoutAssignment { direction, range, started_tick, report }`.
  - Add cargo manifest types tracking required, loaded, carried, abandoned, and deployed goods.

- **Physical packing**
  - `BeginPackingForMigration` creates jobs; it does not despawn structures.
  - Workers execute:
    - `UnpitchStructure`
    - `LoadCampCargo`
    - `UnloadCampCargo`
    - `PitchStructure`
  - Bedrolls pack directly.
  - Tents become `packed_tent`.
  - Yurts become multiple carryable `yurt_bundle` goods rather than one impossible 80kg item.
  - No deployable despawns unless cargo is successfully created or explicitly abandoned.

- **Camp and market lifecycle**
  - `CampState::Pitched` means a real camp exists.
  - `CampState::Packed` means the core camp is dismantled and loaded.
  - Add `CampState::Traveling` or represent travel through `CampOperation::Traveling`.
  - Camp market/treasury exists but is inactive while traveling.
  - Update `FactionData.home_tile` and `Camp.home_tile` only when a permanent camp is successfully pitched.

## Realism Rules
- Player and AI migration scoring should prefer historically plausible movement:
  - fresh water
  - seasonal forage and pasture
  - herd movement
  - shelter from winter/summer extremes
  - known safe routes
  - distance from predators/enemies
  - sanitation/depletion pressure at the old camp
  - carrying burden and straggler risk
- The band should not know perfect destinations without scouting or memory.
- A nearby mediocre known site should often beat a far perfect unknown site.
- Herds attract migration as pasture/taming/hunting opportunity only when the band has the knowledge and tools to exploit them.

## Test Plan
- Player `Plan Migration` does not trigger AI migration when `nomad_autopilot == false`.
- Scouts physically travel and produce candidate site reports.
- Route waypoints can be assigned before pitching and can be changed during travel.
- Packing jobs preserve all cargo; full inventories block progress instead of deleting goods.
- Migration travel moves members and pack animals with carried cargo.
- Temporary camp allows rest without updating permanent `home_tile`.
- Permanent pitch updates `FactionData.home_tile`, `Camp.home_tile`, recent camp history, and activity log only after minimum shelter is deployed.
- AI autopilot uses the same scouting, packing, routing, travel, and pitching pipeline as players.

## Assumptions
- Player migration should feel like leading an expedition, not placing a teleport destination.
- Manual player control is default; autopilot is for AI factions and optional future player automation.
- Historical realism wins over convenience: uncertainty, scouting, carrying limits, seasonal pressure, and travel fatigue are intentional gameplay constraints.
