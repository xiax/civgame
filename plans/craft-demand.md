# Functional Craft Demand

## Summary
Fix auto-crafting so factions stop inventing population-scaled demand for crafted goods. All autonomous craft posting paths will require a concrete functional deficit; explicit player craft requests stay unchanged.

## Key Changes
- Replace the crafted-good quota in `resource_demand_system` with an internal craft-demand helper used by auto-posters.
- Chief crafting will post only for functional gaps:
  - `Weapon`: hunters or active raiders lacking a weapon.
  - `Tools`: a small core reserve plus craft/architecture workers lacking tools.
  - `Ard Plow`: agricultural faction with `ARD_PLOW`, plots, and no plow.
  - No automatic chief `Luxury`, `Cloth`, `Armor`, `Shield`, or cart-part crafting until a real consumer/deficit exists.
- Count existing supply broadly: faction storage, member inventory, carried items, equipped items, live craft postings, and live `CraftOrder`s.
- Tighten private auto-contracts:
  - Household contracts no longer fall back to Tools by default.
  - Belonging contracts can post Cloth only when the household/faction lacks accessible Cloth.
  - Esteem/luxury contracts post only when the poster/faction lacks the output and no duplicate output contract is already live.
- Leave player-driven tablet/book and direct craft orders untouched.

## Interfaces / Types
- Add an internal helper, likely in `src/simulation/jobs.rs`, for auto craft demand:
  - input: faction id/data, recipe/output, live member/equipment/carrying state, board/orders.
  - output: remaining deficit for that recipe output.
- Change `pick_household_recipe(...)` in `src/simulation/faction.rs` to return `Option<RecipeId>` and require a live output deficit.
- Update `src/simulation/CLAUDE.md` to document functional craft demand replacing population quotas.

## Test Plan
- Add tests that population alone creates no chief Craft posting.
- Add tests that an unarmed Hunter creates Weapon demand, while inventory/equipped/storage weapons satisfy it.
- Add tests that Tools are capped to the small functional reserve/worker gap.
- Add tests that chief does not auto-post Luxury/Armor/Shield/Cloth without a real consumer.
- Add tests that household/esteem auto-contracts do not duplicate outputs already available or already posted.
- Run `cargo test --bin civgame` after implementation.

## Assumptions
- “All auto craft” includes chief, household, and esteem-driven autonomous contracts.
- “Functional” means inventory/equipment/system-use deficits, not raw population quotas.
- Explicit player craft requests remain exempt from these gates.
