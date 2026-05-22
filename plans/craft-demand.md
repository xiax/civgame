# Functional Craft Demand — SHIPPED

Autonomous craft posting now requires a concrete functional deficit, not a
population-scaled quota. Player craft requests (`JobSource::Player`) untouched.

## What landed

- **`jobs::compute_craft_demand` + `CraftDemandInputs`** — pure helper. Models
  only **Weapon** (unarmed Hunters + raid-party members, netted vs storage +
  in-flight crafts/orders) and **Ard Plow** (faction has `ARD_PLOW` + a
  state-owned Agricultural plot + no plow stored/in-flight). **Tools** appear
  only as *derived* demand — the Ard Plow recipe's ingredient. No standalone
  Tools/Cloth/Luxury/Shield/Armor/cart-part demand.
- **`FactionData.craft_demand`** — new netted per-output deficit map.
  `resource_demand_system` computes it (queries Profession/Equipment/Carrier,
  JobBoard, CraftOrders, PlotIndex); the six population-quota inserts are gone.
  `resource_demand` keeps only gatherable blueprint + food demand.
- **Chief Craft branch** reads `craft_demand` directly (was `demand − supply`).
  Blocked-input pull-posting now skips craftable inputs (Tools) — `craft_demand`
  owns them; a Stockpile job for Tools would only stall.
- **`pick_household_recipe` → `Option<RecipeId>`** — `Some(Woven Cloth)` only
  for a Belonging-tier head when the village can weave and no accessible Cloth
  exists; every other tier `None` (no Tools fallback).
- **Esteem contracts** — gated: poster must hold no Luxury good and the faction
  board must have no live Luxury craft contract (dedup).

## Tests

`jobs::craft_demand_tests` (4 pure unit tests) + `test_fixture` integration:
`chief_posts_no_craft_without_functional_consumer`,
`unarmed_hunter_drives_weapon_craft_demand`, updated
`funded_household_posts_paid_craft_contract_per_day` /
`household_picks_cloth_recipe_at_belonging_tier_when_loom_known`. Full suite
998 passing.

Side fix: Ard Plows are now auto-craftable (were never in the old quota).
Docs: `src/simulation/CLAUDE.md`. Full design rationale:
`~/.claude/plans/evaluate-the-users-xiao1-civgame-plans-c-curried-dewdrop.md`.

## Extended — Armor / Shield / Cloth + priority

The deferred goods that *do* have a hard functional consumer are now modelled
(`~/.claude/plans/evaluate-the-users-xiao1-civgame-plans-c-purrfect-rose.md`):

- **Armor / Shield** — `compute_craft_demand` gains `unarmored_combatants` /
  `unshielded_combatants` (the same Hunter + raid-party set as Weapon), netted
  vs storage + in-flight. Armor/Shield mitigate real combat damage
  (`combat.rs` reads `armor_stats`), so an unequipped combatant is a genuine
  deficit.
- **Cloth** — `unclothed_members` (bare TorsoArmor slot) drives demand when the
  faction is Aware of `LOOM_WEAVING`. The functional consumer is a flat
  `mood::CLOTHING_MOOD_PENALTY` (12) in `derive_mood_system` for a bare torso
  in a weaving faction — no clothing/temperature `Need`.
- **`jobs::craft_priority`** — the chief Craft branch now selects by
  `(priority, deficit)`: Weapon 4 > Tools 3 > Armor/Shield 2 > Ard Plow 1 >
  Cloth 0, so a hard combat gate always crafts before the large-N comfort good.
- The household Belonging→Cloth contract is untouched (Market-mode paid path);
  it self-dedups against chief cloth demand via the `in_flight` scan.

Still deferred: cart parts (no vehicle haul-throughput demand model), Luxury.
