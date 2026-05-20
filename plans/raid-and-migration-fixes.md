**Raid And Migration Realism Pass**

**Summary**
- Replace one-tick “desperation flips” with sustained-pressure decisions for raids, nomad migration, and settled collapse.
- Keep the current ECS shape: faction-level systems decide intent, goal systems dispatch members, executors perform movement/steal/pack/pitch.
- Preserve player control: player nomads remain manual; this pass only changes autonomous AI behavior and defensive reactions.

**Public API / State Changes**
- Extend `FactionData` with lightweight raid lifecycle fields:
  - `raid_started_tick: u32`
  - `last_raid_end_tick: u32`
  - `raid_stolen_food: u32`
- Keep `raid_target: Option<u32>` and `under_raid: bool` for compatibility with existing goal/HTN systems.
- Add raid constants in `src/simulation/raid.rs`:
  - `RAID_TRIGGER_DAYS = 2`
  - `RAID_COOLDOWN_DAYS = 10`
  - `RAID_MIN_AVG_HUNGER = 140.0`
  - `RAID_CANCEL_FOOD_PER_MEMBER = 3.0`
  - `RAID_RIVAL_RESERVE_PER_MEMBER = 5.0`
  - `RAID_MAX_PARTY_FRAC = 0.35`
  - `RAID_MAX_PARTY_ABS = 8`
  - `RAID_STEAL_COOLDOWN_TICKS = TICKS_PER_DAY / 8`
- Add nomad constants in `src/simulation/nomad.rs`:
  - `NOMAD_TRIGGER_FOOD_DAYS = 3`
  - `NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER = 1`
  - `NOMAD_MIN_ACCEPTABLE_SITE_SCORE = 15.0`
  - `NOMAD_NO_CANDIDATE_RETRY_DAYS = 2`
- Add collapse constants in `src/simulation/sedentary_collapse.rs`:
  - `COLLAPSE_FOOD_DAYS = 3`
  - `COLLAPSE_TRIGGER_SEASONS = 2`
  - `COLLAPSE_MIN_BED_COVERAGE = 0.5`

**Raid Changes**
- In `faction_decision_system`, require sustained crisis before setting `raid_target`:
  - Food crisis if `food_total < member_count * 1.0` for at least `RAID_TRIGGER_DAYS`.
  - Hunger crisis if average hunger is at least `RAID_MIN_AVG_HUNGER`.
  - Do not raid during cooldown: `now < last_raid_end_tick + RAID_COOLDOWN_DAYS * TICKS_PER_DAY`.
- Filter raid targets:
  - Exclude `SOLO`, self, child household factions, parent factions, same-root factions, subordinates/overlords, and factions without enough surplus.
  - Target surplus must be `target_food > target_members * RAID_RIVAL_RESERVE_PER_MEMBER`.
  - Prefer nearest valid rival, but reject targets beyond a fixed travel cap of `500` Chebyshev tiles.
- Limit participation in `goal_update_system`:
  - Only members with hunger `< HUNGER_RAID_CEILING`, not drafted, not chief unless party is undersized, and deterministic hash rank within party cap choose `AgentGoal::Raid`.
  - Party cap is `min(RAID_MAX_PARTY_ABS, ceil(member_count * RAID_MAX_PARTY_FRAC))`.
  - Everyone else stays on survival/food gathering.
- Fix raid execution:
  - Treat arrival as `chebyshev(current_tile, enemy_home) <= 1`, not `ai.target_tile == enemy_home`.
  - Let each raider steal at most once per `RAID_STEAL_COOLDOWN_TICKS`.
  - Do not steal if target food would fall below `target_members * RAID_RIVAL_RESERVE_PER_MEMBER`.
  - Increment `raid_stolen_food` on successful steal.
- End raids when:
  - Raider faction has recovered to `member_count * RAID_CANCEL_FOOD_PER_MEMBER`, including carried edible food for the raiding party.
  - Target has no surplus.
  - Raid has lasted more than `2 * TICKS_PER_DAY`.
  - On end, clear `raid_target`, reset `raid_stolen_food`, set `last_raid_end_tick = now`, and force raiders to reevaluate.

**Nomad Migration Changes**
- Change the migration trigger in `nomad_migration_system`:
  - Use actual faction storage/carry inventory first: `food_total < member_count * NOMAD_TRIGGER_FOOD_DAYS`.
  - Require weak local opportunity as a second condition: known local food score `< member_count * NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER`.
  - Use the capability value `migration_period_min_days` for cooldown, converted to ticks, instead of hard-coded `TICKS_PER_SEASON`.
- Improve survey completion:
  - If `pick_migration_candidates` returns no candidate with score `>= NOMAD_MIN_ACCEPTABLE_SITE_SCORE`, do not write `pending_migration`.
  - Return to `Idle`, set `last_phase_change_tick = now`, and delay retry by setting `last_migration_tick = now - cooldown + NOMAD_NO_CANDIDATE_RETRY_DAYS * TICKS_PER_DAY`.
  - Remove blind `fallback_direction` for AI autopilot commits; keep it only for debug/manual tooling if still needed.
- Candidate scoring:
  - Include wild herds only when represented in faction knowledge or a scout report; do not read every global herd as an omniscient option.
  - Stop using `MemoryKind::Prey` as predator danger. Until predator memory exists, danger should be `0` except for explicit hostile-faction scout reports.
  - Reject ocean/impassable candidates before scoring, not only during commit validation.
- Migration commit:
  - Keep `migration_target_ready` as the final passability/connectivity gate.
  - If validation fails, clear `pending_migration`, return to `Idle`, and apply the short retry delay above.

**Settled Collapse Changes**
- Make collapse require combined failure, not any single weak signal:
  - `food_deficit = food_total < member_count * COLLAPSE_FOOD_DAYS`
  - `pop_crash = member_count < SEDENTARY_COLLAPSE_MIN_MEMBERS`
  - `shelter_loss = usable_beds < ceil(member_count * COLLAPSE_MIN_BED_COVERAGE)`
  - `failing = food_deficit && (pop_crash || shelter_loss)`
- Increase duration:
  - Set `COLLAPSE_TRIGGER_TICKS = TICKS_PER_SEASON * COLLAPSE_TRIGGER_SEASONS`.
  - With current seasons, collapse requires 10 consecutive in-game days of combined failure.
- Count shelter more robustly:
  - Prefer settlement-owned beds for the faction when a settlement exists.
  - Fall back to current radius-based `BedMap` scan only if no settlement entity exists.
  - Use a radius based on settlement footprint or at least `32`, not `OLD_CAMP_RADIUS = 12`.
- On settled → nomadic switch in `handle_switch_archetype`:
  - Set `camp_state = Pitched`.
  - Set `migration_phase = Idle`.
  - Clear `pending_migration`.
  - Set `last_migration_tick = current_tick` so the newly collapsed camp cannot immediately migrate again.
  - Keep `nomad_autopilot` unchanged for AI factions and false for player factions.

**Test Plan**
- Raid tests:
  - No raid at food `0` with average hunger below `140`.
  - No raid until crisis lasts `2` days.
  - No target selection for household/same-root/overlord/subordinate factions.
  - Party size never exceeds `min(8, ceil(0.35 * members))`.
  - Adjacent raider can steal once, cannot drain target below reserve, and observes steal cooldown.
  - Raid ends and enters cooldown when carried/stored food recovers.
- Nomad tests:
  - Food-rich nomadic faction does not enter `Surveying` even with weak local knowledge.
  - Food-poor faction with enough known nearby food does not migrate.
  - Food-poor faction with no acceptable candidate returns to `Idle` without `pending_migration`.
  - Unknown global herd does not create a candidate; scouted/known herd does.
  - Cooldown uses `migration_period_min_days` metadata.
- Collapse tests:
  - Food deficit alone does not collapse.
  - Shelter loss alone does not collapse.
  - Food deficit plus shelter loss for one season does not collapse.
  - Food deficit plus shelter loss for two seasons emits `SwitchArchetype`.
  - Post-collapse faction has `camp_state = Pitched`, `migration_phase = Idle`, no `pending_migration`, and fresh `last_migration_tick`.
- Regression command:
  - Run `cargo test --bin civgame`.
  - If broad tests are slow, first run focused tests for `raid`, `nomad`, and `sedentary_collapse`, then full binary tests.

**Assumptions**
- Raids should be rare emergency actions, not normal food logistics.
- Nomads should move because the current camp is failing and a better destination is known, not merely because local knowledge is sparse.
- Settled collapse should represent sustained institutional failure, not a temporary storage dip or a few missing beds.
- No new crates or save-file migration are required; new `FactionData` fields can default to `0`.
