# Player-Directed Physical Nomad Migration Plan

## Summary (shipped subset)
- `nomad_autopilot` field distinguishes AI vs player nomads; AI runs Survey → PendingCommit → commit → Walking FSM, players run free-form Pack → Move → Pitch.
- HUD "Plan Migration" + `ui/migration_panel.rs` shows status / intent picker / scout dispatch / candidate sites.
- Six `MigrationIntent` variants (FreeRoute / FollowWater / FollowHerds / SeekWinterShelter / SeekSummerPasture / AvoidDanger) with weight vectors fed into `pick_migration_target`.
- `PlayerCommand::SendScout { direction, range }` → `ScoutAssignment { kind: PlayerManual }`; `manual_scout_completion_system` validates and folds into `FactionData.candidate_sites` (ring cap 16).
- `CampSiteCandidate { anchor, score, reasons, validation }` with `CandidateReason` enum.
- Observable packing pipeline: `PackingDuty` + `Task::UnpitchStructure` + `unpitch_structure_task_system` (`UNPITCH_WORK_TICKS = 40`); refund drops as `GroundItem`s.
- `Deployable.packed_bundles: Vec<(ResourceId, u32)>` lets yurts drop multiple haul-sized goods instead of one 80kg item.
- `PackAnimalInventory` per-`Tamed`; capacity by species; folded into `faction.storage.totals` for nomads.
- Pitch conserves shelter count (debits one `bedroll` / `packed_yurt` per spawned shelter from member/pack-animal inventories).

## Deferred
- **Route phase (waypoints).** `SetMigrationRoute { waypoints }`, `StartMigration` / `HaltMigration` / `ResumeMigration`, `MakeTemporaryCamp`. Today's pipeline is Pack → Move → Pitch directly (no waypoint chain, no temporary camps). `CampOperation::Traveling` not modelled — sim relies on per-agent `MigrationTarget` + `Task::WalkTo`.
- **`CampOperation` enum** (`Idle / Scouting / Packing / Traveling / TemporaryCamp / Pitching`). Today's `MigrationPhase` covers Idle / Surveying / PendingCommit / Walking only; no explicit "Packing in progress" or "Pitching in progress" sub-phases.
- **Full cargo manifest pipeline.** `CampCargoManifest { required, loaded, abandoned, deployed }` is a scaffold today; `LoadCampCargo` / `UnloadCampCargo` / `PitchStructureAt` `TaskKind` variants exist (45–47) but only `UnpitchStructure` has an executor. Members scavenge dropped GroundItems instead of running a coordinated load step.
- **Travel pace modifiers.** Today's column moves at each agent's individual walk speed; no children/elders/injury/burden composite pace; no straggler logic.
- **`Plan Migration` HUD button.** Migration panel exists but is launched from a different entry point than the originally-specified "Plan Migration" button. Adjust if a dedicated button is wanted.

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
