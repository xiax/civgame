# Realistic Tool Overhaul — Complete

## Progress (2026-05-22)

**Shipped — full plan, including HTN prepend (build green, 1049 tests):**

- HTN tool-acquisition dispatcher (`tools::htn_acquire_tool_dispatch_system`,
  ParallelB, after the spear dispatcher and before every work dispatcher).
  Goal-agnostic: maps `GatherWood→Axe`, `GatherStone→Pick`, `Farm→Sickle`,
  `Craft→recipe.tool_requirements[0]` (via `JobClaim`+`JobBoard` lookup);
  when the worker's `ToolKit` lacks the form and faction storage holds a
  satisfying tool, routes `[WithdrawTool → StowToolKit]`. Storage-stock
  short-circuit skips dry armouries; no satisfying Item anywhere ⇒ the
  dispatcher declines and the work proceeds degraded (gather/dig/fish) or
  stalls (craft) for chief posting to fill via
  `compute_faction_tool_deficits`.
- `goal_dispatch_system` preserve-arms: `WithdrawTool` (narrowed via
  `aq.current.as_withdraw_tool().is_some()` — shares `WithdrawMaterial`
  task kind with non-tool legs) and `StowToolKit` survive across
  goal-dispatch ticks.
- Behavioural test (`tool_dispatcher_prepends_withdraw_and_stow_for_gather_wood`):
  empty-kit `GatherWood` worker + faction-storage axe ⇒ dispatcher
  installs `Task::WithdrawTool` head + queued `StowToolKit`; full
  executor pass leaves the axe in the worker's kit.

**Shipped earlier — data model + craft integration:**
- `ItemMaterial` + `Bone`/`Copper`/`Bronze`; catalog resources `knife/axe/pick/
  hammer/sickle/awl/fishing_kit` + `bone`; `bone` added to butchery yield.
- `simulation::tools` module: `ToolForm`, `ToolUseKind`, `ToolTier`,
  `ToolRequirement`, `ToolKit` component, `work_speed_mult`, `capacity_for_era`
  (+4 unit tests).
- `CraftRecipe` extended with `tool_requirements` / `quality_floor` /
  `min_station_tier`; 25 tool recipes appended (indices 21-45, recipe count
  21→46); plow/cart recipes drop the `tools` ingredient for a tool requirement.
- `craft_order_system`: tool-requirement gate (graceful — a worker with **no**
  `ToolKit` is treated as satisfied, so the gate is dormant until ToolKits are
  attached/seeded), `work_speed_mult` per-worker advance, `quality_floor`
  applied at output. `faction_craft_order_system`: `min_station_tier` filter.

**Shipped — attach + seeding + gates + demand (build green, 1048 tests):**
- `ToolKit` attached at all 3 Person spawn sites (founder = `capacity_for_era`,
  newborn + fixture = default; fixture uses `fixture_full_toolkit()` so
  baseline tests aren't gated).
- `seed_starting_tools_system` (OnEnter, after farms): `starting_tool_loadout`
  per era/tech; settled = core tool per member kit + rest to storage, nomadic
  = round-robin into kits; sandbox skipped.
- Execution hard gates: `gather.rs` (Axe→fell/deadwood, Pick→mine/trickle,
  Sickle→grain, `work_speed_mult` shortens threshold), `dig.rs` (Pick for
  stone-like floor), `fishing.rs` (Fishing Kit). All graceful (no `ToolKit`
  → satisfied; empty kit → blocks).
- Tool-acquisition tasks: `Task::WithdrawTool` + `Task::StowToolKit`
  (`TaskKind::StowToolKit = 57`) + `stow_toolkit_task_system` executor; the
  `WithdrawTool` branch in `withdraw_material_task_system` picks the best-tier
  matching `Item` from storage into hands.
- Faction tool demand: `compute_faction_tool_deficits` → `compute_craft_demand`
  per-form deficits; chief posts via existing recipe loop (`craft_priority`
  rank 3). Generic `tools` kept only as Ard-Plow ingredient.
- Tests: `tools` unit tests (loadout scaling, deficit netting) + fixture
  executor tests (no-Sickle blocks grain, no-Axe leaves tree standing,
  StowToolKit eviction). CLAUDE.md (root + simulation) updated.

**Remaining:** none — plan complete.

## Context

Today every "tool" is a single generic `tools` commodity (`ResourceClass::Tool`, `core_ids::tools()`,
`assets/data/resources/core.ron`). It is crafted by two recipes ("Stone Tools", "Iron Tools"),
consumed as a flat ingredient by plow/wheel recipes, and read by gathering only as a binary
`EconomicAgent::has_tool()` that doubles tree-felling yield. There is no functional distinction
between an axe, a pick, and a sickle, and no hard gate.

Goal: realistic, separate, functional tools. A tool *form* (Knife/Axe/Pick/…) determines *what* it
can do; *material + quality* (bone → stone → fine/microlithic stone → copper → bronze) determines
*how well*. Gathering, mining, fishing, and crafting hard-gate on the right form; better tools work
faster. No durability/wear in this pass. Factions start each era with an era-appropriate kit.

## Key Model Changes

- New `simulation::tools` module:
  - `ToolForm`: `Knife, Axe, Pick, Hammer, Sickle, Awl, FishingKit`.
  - `ToolUseKind`: `Cut, Chop, Mine, Shape, Smith, HarvestCrop, Stitch, Fish`; `use_kind → ToolForm`.
  - `ToolTier` (ordered): `Bone < Stone < FineStone < Copper < Bronze`.
    `tool_tier(material, quality)` — `Stone` at `≥Fine` quality ⇒ `FineStone`.
  - `ToolRequirement { use_kind, min_tier }`; lookup helpers `best_tool_for` / satisfaction check.
  - `work_speed_mult(ToolTier) -> f32` (~`Bone 0.9 … Bronze 1.7`) — the only v1 tool benefit.
  - `ToolKit { items: Vec<Item>, capacity: u8 }` component (full-`Item` fidelity, separate from
    `Carrier` cargo hands — worker keeps both hands free for cargo). `capacity_for_era(era)` ⇒ 1
    (Paleo/Meso) or 3 (Neolithic+). No hauler/worker split needed.
- `ItemMaterial` (`src/economy/item.rs`): add `Bone` (~1.2/0.5), `Copper` (~2.2/1.5),
  `Bronze` (~2.8/1.5). `Iron`/`Steel` stay (legacy/future).
- Catalog: add `knife, axe, pick, hammer, sickle, awl, fishing_kit` (`ResourceClass::Tool`) and
  `bone` (`ResourceClass::Material`, butchery yield) to `core.ron` + `core_ids!`; sprite keys via
  `sprite_library.rs`. Keep generic `tools` resource for legacy compatibility.
- `CraftRecipe`: add `tool_requirements: Vec<ToolRequirement>`, `quality_floor: Option<ItemQuality>`,
  `min_station_tier: Option<WorkbenchTier>`. Default all three on the 21 existing recipes.
- Manufactured tool `Item`s already survive ground/storage round-trips (`GroundItem` merges only on
  full `Item` equality, `simulation/items.rs:98`); `ToolKit` likewise holds full `Item`s.

## Recipes And Tool Requirements

Append new recipes — never reorder/splice; `recipe_id` is a stable `u8` on `CraftOrder`. Keep the
two legacy `tools` recipes at their current indices.

- **Paleolithic** — Stone Knife/Axe/Pick/Hammer: loose stone + wood, no tool req. Bone Awl:
  bone + stone/skin, requires Knife, gated `BONE_TOOLS`.
- **Mesolithic** — Microlithic Knife/Axe/Pick: stone + wood/skin, `quality_floor: Fine`, gated
  `MICROLITHIC_TOOLS`. Bone Fishing Kit: bone + skin, requires Knife, gated `FISHING`.
- **Neolithic** — Polished Stone Axe/Hammer + Stone Sickle: `quality_floor: Fine`, requires
  Hammer/Knife, gated `CROP_CULTIVATION`/settlement tech.
- **Chalcolithic** — Copper Knife/Axe/Pick/Hammer/Sickle/Awl: copper + wood/skin, requires Hammer
  (≥`Stone`), `min_station_tier: Copper`, gated `COPPER_TOOLS`.
- **Bronze** — Bronze full set incl. Fishing Kit: copper + tin + wood/skin, requires Hammer
  (≥`Copper`), `min_station_tier: Bronze`, gated `BRONZE_TOOLS`.

Update existing recipes' `tool_requirements`: Spear/Bow/Preserved Meat/Fish → Knife; shields/carts/
plows/wheels/frames → Axe or Hammer; Leather Armor/Book → Awl/Knife. Boil Water + first stone-tool
recipes stay tool-free (bootstrap). Plow/wheel recipes drop `tools` *as an ingredient* and instead
carry a tool *requirement*. Output `Item` carries `material` + `display_name`; `quality` is
`max(skill quality, quality_floor)`. Station search filters `WorkbenchMap` by `min_station_tier`.

## Execution Hard Gates

- **Tree felling** (`gather.rs` + `plants.rs::harvest_*`): Axe to fell. No-Axe ⇒ low-yield deadwood,
  tree not despawned (`harvest_despawns(false)` → `false`).
- **Tile mining** (Stone/Wall/Ore in `gather_system`): Pick required. No-Pick ⇒ trickle: small
  loose-stone yield, tile not carved (bootstrap safety).
- **Dig** (`dig_system`): Pick required only when target tile is stone-like; soil stays
  hand-diggable (slower). Stone-like + no Pick ⇒ failed/no-target outcome.
- **Grain harvest** (mature Grain in `gather_system`): Sickle required. Berries + loose
  `GroundItem`s stay hand-gatherable.
- **Fishing** (`fish_task_system`): Fishing Kit required; no kit ⇒ failed outcome.
- **Craft** (`crafting.rs` work loop, `work_progress += workers.len()`): worker must hold all
  `tool_requirements`; executor re-checks each tick so stale plans cannot advance.
- Better tier ⇒ apply `work_speed_mult` to gather `work_progress` accrual and craft `advance`.
  v1 adds no yield bonus beyond existing tech/activity multipliers.

## HTN Dispatch & Tool Acquisition

- Before `Gather`/`Fish`/`Dig`/`WorkOnCraftOrder`, if `ToolKit` lacks the required form and storage
  holds one, prepend a tool withdraw.
- Extend `WithdrawMaterial` to accept an optional `ToolRequirement` and select the best matching
  full `Item` from storage (not a bare `ResourceId` count).
- New withdraw follow-up `StowToolKit` (sibling of `Equip`, in `production::finish_withdraw_material`):
  place the tool `Item` into `ToolKit` (respect `capacity`; evict lowest-tier tool back to
  storage/ground if full), then continue into the work task.
- No tool anywhere ⇒ failed/no-target outcome (`PlanHistory` bias, not hard exclude); let chief
  posting create the tool.

## Faction Tool Demand (replaces generic `tools` demand)

- New `compute_faction_tool_summary` system: scan member `ToolKit`s + faction-storage `GroundItem`s
  into a `by_form_tier` count.
- Chief craft posting: post missing required forms first, then upgrades for forms below the
  faction's best-known tier. Drop the implicit "Tools as plow ingredient" demand path.
- Market/Mixed household tool-buying targets specific forms instead of generic `tools`.

## Starting Era Seeding

- `seed_starting_tools_system` on `OnEnter(Playing)`, after `seed_starting_farms_system`, before
  `mark_warmup_complete_system`.
- Settled: full tool stacks as `GroundItem`s at the faction storage tile via
  `spawn_or_merge_ground_item_full`. Nomadic: one tool per member `ToolKit`. Market: basic personal
  tools into household storage/`ToolKit`s.
- Loadout scales with `member_count`, by era: Paleolithic stone knives/axes + ≥1 pick & hammer
  (+ bone awls if `BONE_TOOLS`); Mesolithic adds fishing kits + Fine/microlithic replacements;
  Neolithic adds sickles + polished work tools; Chalcolithic copper set + stone backups; Bronze
  bronze set + copper/stone backups. Set `ToolKit::capacity` via `capacity_for_era`.
- Sandbox (`seed_buildings == false`): skip or seed minimal one-of-each.

## Deferred (not in v1)

- Tool durability / wear.
- A craftable tool-belt/satchel *item* raising `ToolKit::capacity` (v1 ties capacity to era).
- Hauler vs. worker role split (`ToolKit`-separate-from-`Carrier` already removes the hands
  conflict).
- Construction tool requirements beyond recipes that consume/require tools.

## Tests And Docs

- Tests (`cargo test --bin civgame`): catalog/core IDs for 7 forms + `bone`; `tool_tier` ordering;
  `ToolKit` capacity & evict; recipe `tool_requirements`/`quality_floor`/`min_station_tier`;
  full-`Item` preservation through withdraw → `StowToolKit`; executors (no-Axe deadwood-only,
  no-Pick mining trickle, no-Sickle no grain, fishing needs kit, berries/loose stone still work);
  craft (required-tool recipe stalls, dispatcher prepends withdraw, chief posts missing/upgrade);
  start-seed for Paleolithic/Neolithic/Chalcolithic/Bronze, nomadic, Market.
- `cargo check` + `cargo run` (not `--sandbox`).
- Update `CLAUDE.md` files: root (tool model summary), `src/simulation/CLAUDE.md` /
  `src/economy/CLAUDE.md` (separate tools, `ToolKit` carriage, hard gates, era seeding, legacy
  status of generic `tools`).

## Assumptions

- Separate tools = separate functional forms; material + quality represent better versions.
- No tool wear/durability in this pass.
- Scope: crafting, gathering, fishing, mining, digging. Construction unchanged except recipes.
- No new crates. No new tech IDs (all gates already exist).
