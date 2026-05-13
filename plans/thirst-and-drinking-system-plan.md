# Thirst, Drinking, And Sanitation

## Summary
Add thirst for people and animals, with water quality, stored water, boiling, waste/contamination, and nonlethal sickness. Thirst becomes a physiological survival pressure like hunger, but it will not directly damage health or kill.

## Key Changes
- Add `thirst: f32` to human `Needs` and animal `AnimalNeeds`; include it in distress, mood, Maslow physiological gating, inspector/hover/debug UI, and need ticking.
- Add water resources: `clean_water`, `raw_water`, and `waste` to the resource catalog and `core_ids`; one water unit equals one drink, weighs `1000g`, and uses existing inventory/storage systems.
- Add water classification helpers:
  - `River` = clean freshwater unless contaminated.
  - inland/lake `Water` and `Marsh` = raw/dirty water.
  - ocean `Water` = salt water, not drinkable or collectible.
  - contamination downgrades nearby water to contaminated/raw.
- Add `SanitationMap`, `WastePile`, `Latrine`, and `Sickness`:
  - waste and rotted corpses add local contamination.
  - contamination decays over time and affects nearby water.
  - latrines contain waste better, but pollute water if placed too close.
  - sickness is nonlethal: it raises discomfort/fatigue pressure and slows recovery/work, but never damages `Health`.

## Human Behavior
- Keep `AgentGoal::Survive`; add thirst branches before hunger in `goal_update_system` and the scored goal pipeline.
- Add typed tasks and task kinds for `Drink`, `CollectWater`, and `BoilWater`.
- Add HTN methods under survival:
  - drink clean water from inventory/hands.
  - drink from adjacent clean river water.
  - withdraw clean water from faction/member-pool storage, then drink.
  - collect clean water from rivers for storage.
  - collect raw water from dirty freshwater, then boil at a hearth/campfire into clean water.
  - emergency raw/contaminated drinking only when no clean/boil route exists and thirst is severe; this may apply nonlethal sickness.
- Add chief water stockpiling: factions maintain a clean-water reserve target based on member count; nomadic member-pool storage and pack-animal rollups count as stored water.

## Animal Behavior
- Add `AnimalState::Drinking`; thirsty animals seek nearest reachable non-salt water and drink adjacent to it.
- Animal priority order: flee/attack stays urgent, severe thirst can interrupt wandering, grazing, reproduction, and normal hunting/chasing.
- Tamed animals use the same natural-water seeking as wild animals; stored-water/trough behavior is deferred unless a later structure-specific pass adds troughs.
- Drinking raw/contaminated water can apply the same nonlethal sickness component to animals.

## Test Plan
- Unit tests: water classification, contamination downgrade, ocean salt rejection, contamination decay, latrine containment, thirst utility monotonicity, `Needs::worst`/`avg_distress`.
- Human behavior tests: thirsty person drinks held clean water, withdraws stored water, drinks from river, collects/boils raw water, ignores ocean water, and does not take health damage at max thirst.
- Animal tests: thirst rises, animals route to water, drink to reduce thirst, avoid salt water, and can become nonlethally sick from contaminated water.
- Economy/storage tests: clean/raw water count in faction storage and nomadic member-pool/pack-animal storage.
- Run `cargo test --bin civgame` and `cargo check`.

## Assumptions
- No dehydration or sickness health damage in this pass.
- Boiling requires a reachable campfire/hearth and consumes raw water; fuel use starts with no extra wood cost unless balance testing says otherwise.
- Existing `Water` tiles need `Globe`-based classification because `TileKind::Water` currently represents both lakes and oceans.
- Update subsystem docs in `src/simulation/CLAUDE.md`, `src/world/CLAUDE.md`, and resource/economy notes after implementation.
