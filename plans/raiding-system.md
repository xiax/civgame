# Realistic Raiding System

## Summary
Replace the current global `raid_target` shortcut with a stateful raid pipeline: factions only raid targets they have actually discovered, only when survival pressure outweighs safer options, and only with a limited party that can travel, steal physical goods, retreat, and create diplomatic consequences.

The first implementation should focus on food raids. Materials, tribute enforcement, and punitive raids can use the same framework later.

## Key Changes
- Replace `FactionData.raid_target: Option<u32>` with a richer `RaidState`/`RaidPlan` model:
  - `Idle`
  - `Considering`
  - `Mustering { plan_id }`
  - `Traveling`
  - `Raiding`
  - `Retreating`
  - `Recovering`
- Add faction-level raid intel:
  - known target faction or settlement id
  - last seen tile
  - last seen tick
  - estimated food surplus
  - estimated defender strength
  - confidence score
- Feed intel from existing simulation visibility, not player fog:
  - `vision_system` records foreign settlements, storage tiles, people, and visible food
  - gossip promotes intel through existing `AgentMemory` / `SharedKnowledge` patterns
  - target selection never reads hidden rival `FactionStorage` directly
- Replace “nearest rival with food” with a daily raid decision score:
  - own food per head critically low
  - average hunger high
  - no fresh known public food/prey/farm alternative
  - target intel fresh enough
  - target appears to have surplus
  - route is reachable
  - distance and defender risk are acceptable
  - faction is outside an initial grace period, e.g. first 3 game-days after founding
- Limit participation:
  - only assigned `RaidParticipant`s get `AgentGoal::Raid`
  - party size capped, e.g. `2..=max(2, adults / 4)`
  - prefer Hunters, high Combat skill, high martial disposition
  - exclude children, injured, exhausted, starving, chiefs unless culture/need strongly justifies it
- Change raid execution:
  - route to a known storage/market/loot tile, not blindly to `home_tile`
  - validate target on arrival; abort if loot is gone, stale, unreachable, or too dangerous
  - physically remove edible `GroundItem`s from target storage and put those exact resources into raider inventory/carrier
  - retreat once loot quota, danger, time, or casualty thresholds are reached
  - deposit stolen goods into attacker storage on return
- Change defense:
  - replace broad 30-tile alarm with LOS/sound/contact-based alert
  - `under_raid` becomes a timed `RaidAlert { attacker, last_seen_tile, confidence }`
  - defenders respond through existing distress/rescue systems; Hunters rally farther, civilians prioritize fleeing/shelter
- Add aftermath:
  - per-faction raid cooldown
  - relation/grievance memory between factions
  - repeated raids increase hostility and retaliation likelihood
  - failed or costly raids reduce future risk appetite for a while
- Align `world_sim_system` offscreen raids with the same spirit:
  - no raids into loaded/player-observed cells
  - only adjacent known/claimed rival cells
  - require starvation plus target surplus
  - add cooldown so offscreen cells do not churn food every minute

## Implementation Notes
- Reuse existing ECS shapes: systems plus data-only components/resources.
- Keep `AgentGoal::Raid` and `Task::Raid`, but drive them from `RaidParticipant`/`RaidPlan` rather than global faction war state.
- Move raid selection into a cadence-based Economy system after `compute_faction_storage_system` and knowledge promotion.
- Update `htn_combat_faction_dispatch_system` so raid destination comes from the participant’s active plan.
- Remove the goal-update branch that sends every non-hungry faction member to raid whenever `raid_target` is set.

## Test Plan
- Unknown target with real food does not become raid target.
- Day-0 factions with no food do not raid before scout/intel/grace gates pass.
- A faction with known forage/prey/farm options chooses survival gathering before raiding.
- A fresh scout sighting of a food-rich rival can produce a small raid party under real famine.
- Only assigned participants switch to `AgentGoal::Raid`.
- Raid steals actual stored edible resources, not synthetic fruit.
- Raid aborts when target loot is stale, unreachable, or gone.
- Defender alert only fires on sight/sound/contact, not arbitrary distance.
- Retreated raiders deposit stolen food at home.
- Run `cargo test --bin civgame`.

## Assumptions
- V1 is food raiding, not conquest or siege warfare.
- No new crates.
- Rendering fog stays player-facing only; AI uses simulation memory/intel.
- Root and `src/simulation/CLAUDE.md` docs should be updated when behavior changes.
