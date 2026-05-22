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
