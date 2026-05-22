# Realistic Tool Overhaul

## Summary
Implement separate, functional tools with hard craft/gather requirements, no wear in v1, and era-appropriate starting tool kits. Tools will be distinct by purpose, while material/quality controls how good they are: stone/bone → microlithic/polished stone → copper → bronze.

Keep the legacy generic `tools` resource only for compatibility outside this scope; new crafting and gathering logic should use specific tools.

## Key Model Changes
- Add a `simulation::tools` module with:
  - `ToolForm`: `Knife`, `Axe`, `Pick`, `Hammer`, `Sickle`, `Awl`, `FishingKit`.
  - `ToolUseKind`: `Cut`, `Chop`, `Mine`, `Shape`, `Smith`, `HarvestCrop`, `Stitch`, `Fish`.
  - `ToolRequirement { use_kind, min_tier }`, plus helpers to find the best matching tool in inventory, hands, or equipment.
  - `ToolTier` derived from `ItemMaterial` and quality: stone/bone baseline, Fine stone for microlithic/polished tools, copper, bronze.
- Extend `ItemMaterial` with `Bone`, `Copper`, and `Bronze`.
- Add catalog resources for tool forms: `knife`, `axe`, `pick`, `hammer`, `sickle`, `awl`, `fishing_kit`; add `bone` as an animal-product material from butchery.
- Extend `CraftRecipe` with `tool_requirements` and station tier support, so bronze recipes can require a bronze workbench while primitive stone tools remain bootstrap-safe.
- Preserve full manufactured `Item` values when withdrawing tools from storage; do not collapse a Bronze Axe into a commodity `axe`.

## Recipes And Tool Requirements
- Paleolithic:
  - Stone Knife, Axe, Pick, Hammer: loose stone + wood, no tool requirement.
  - Bone Awl: bone + stone/skin, requires Knife, gated by `BONE_TOOLS`.
- Mesolithic:
  - Microlithic Knife/Axe/Pick: stone + wood/skin, Fine quality floor, gated by `MICROLITHIC_TOOLS`.
  - Bone Fishing Kit: bone + skin, requires Knife, gated by `FISHING`.
- Neolithic:
  - Polished Stone Axe/Hammer and Stone Sickle: stone + wood + skin, requires Hammer/Knife, gated by `CROP_CULTIVATION` or settlement-era tech as appropriate.
- Chalcolithic:
  - Copper Knife/Axe/Pick/Hammer/Sickle/Awl: copper + wood/skin, requires Hammer tier Stone+, copper workbench, gated by `COPPER_TOOLS`.
- Bronze Age:
  - Bronze Knife/Axe/Pick/Hammer/Sickle/Awl/Fishing Kit: copper + tin + wood/skin, requires Hammer tier Copper+, bronze workbench, gated by `BRONZE_TOOLS`.

Update existing recipes so normal crafts require realistic tools:
- Spear/Bow/Preserved Meat/Preserved Fish: Knife.
- Shields, carts, plows, wheels, frames: Axe or Hammer as appropriate.
- Leather Armor, Cloth-adjacent fine work, Book: Awl/Knife.
- Boil Water and first stone-tool recipes remain tool-free.

## Execution And AI Behavior
- Gathering hard gates:
  - Tree felling requires Axe; no-axe fallback becomes low-yield deadwood/branch collection and does not fell the tree.
  - Stone/Wall/Ore mining and `Dig` require Pick.
  - Mature Grain harvest requires Sickle; berries and loose GroundItems remain hand-gatherable.
  - Fishing requires Fishing Kit.
- Better tools reduce work ticks only; v1 does not add extra yield beyond current tech/activity multipliers.
- Craft workers must have required tools before `WorkOnCraftOrder` progresses. Executor double-checks the requirement so stale plans cannot advance.
- HTN dispatchers should prepend tool acquisition when needed:
  - If the worker lacks a required tool and storage has one, queue `WithdrawMaterial { tool, 1 }` before `Gather`, `Fish`, `Dig`, or `WorkOnCraftOrder`.
  - Extend withdraw follow-up routing so a tool withdraw can continue into those work tasks.
  - If no tool exists, record a failed/no-target outcome and let chief posting create the missing tool instead of thrashing.
- Replace generic tool demand:
  - Add a per-faction tool summary by form/tier from storage and member inventories.
  - Chief craft posting prioritizes missing required forms first, then upgrades below the faction’s best known tool tier.
  - Market/Mixed household tool-buying shifts from generic `tools` to useful specific forms.

## Starting Era Seeding
- Add `seed_starting_tools_system` on `OnEnter(Playing)` after population/household creation and before warmup completion.
- Settled factions receive full `GroundItem` tool stacks at their faction storage tile; nomadic factions receive tools distributed into member inventories.
- Market starts also seed basic personal tools into household inventories/storage so private workers are not blocked at tick 1.
- Starting loadout scales from `member_count`:
  - Paleolithic: stone knives/axes, at least one pick and hammer, bone awls if known.
  - Mesolithic: fishing kits and Fine stone/microlithic replacements.
  - Neolithic: sickles and polished stone work tools.
  - Chalcolithic: copper working set plus stone backups.
  - Bronze: bronze working set plus copper/stone backups.
- Sandbox keeps `seed_buildings = false`; tool seeding should also skip or use sandbox-specific minimal one-of-each behavior.

## Tests And Docs
- Add tests for catalog/core IDs, tool lookup, tier ordering, recipe requirements, and full-item withdraw preservation.
- Add executor tests: no axe cannot fell trees, axe can; no pick cannot mine; no sickle cannot harvest grain; fishing requires kit; primitive berries/loose stone still work.
- Add craft tests: required-tool recipes do not progress without tools, dispatchers withdraw tools when available, chiefs post missing/upgrade tool recipes.
- Add start-seed tests for Paleolithic, Neolithic, Chalcolithic, Bronze, nomadic, and Market starts.
- Run `cargo test --bin civgame` and `cargo check`.
- Update root `AGENTS.md` plus simulation/economy notes to document separate tools, starting loadouts, hard gates, and the legacy status of generic `tools`.

## Assumptions
- Separate tools means separate functional tools; material and quality represent better versions of each tool.
- No tool wear or durability in this pass.
- Scope is crafting, gathering, fishing, mining, and digging only; construction remains unchanged except where recipes consume or require tools.
- No new crates.
