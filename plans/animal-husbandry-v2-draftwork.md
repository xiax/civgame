# Animal Husbandry v2 — Draftwork (Plow + Modular Cart)

Revised plan. Supersedes the prior draft after exploration corrected several premises and user clarifications locked design choices.

## Context

v1 shipped taming, pens/stables, feed troughs, and `HitchingPost` (inert marker). The data shells `AnimalWorkClaim { worker, use_kind, expires_tick }` and `AnimalUse::{Plow, Cart, Pack, Mount, Companion, Guard}` exist (`src/simulation/animals.rs:160-181`) but have **zero writers** outside the expiry sweep — Pack/Plow/Cart/Guard are all stubs (`AnimalUse::Pack` never inserted either; nomad pack hauling is live via the separate `PackAnimalInventory` path). `DomesticAnimal.training: u8` is initialised to 0 and never incremented. `HitchingPost` is an empty marker with only an `on_add`/`on_remove` map hook — no logic, no reservation, no fields (`src/simulation/husbandry.rs:73-76`, `construction.rs:6838-6844`).

Tech gates are in place: `ARD_PLOW` (Chalcolithic, id 26), `OX_CART` (Chalcolithic, id 25), `HORSE_TAMING` (Bronze, prereqs `ARD_PLOW`) (`src/simulation/technology.rs:25-39`).

Goal: turn domesticated cattle/horses into a real economic input. Plowing boosts grain yield on Agricultural plots; carts haul bulk cargo farther/faster than human hand-carry; both are gated on tech, training, and equipment availability.

## Corrections to the prior draft

- **`PlotIndex.ag_tile_yield` does not exist.** `PlotIndex` (`src/simulation/land.rs:224-240`) has `by_id`, `by_tile`, and `ag_tiles: AHashSet<(i32, i32)>` for road-protection only. Yield is computed per-harvest from `TileKind::soil_fertility_mult` (static) × base yield × claim pressure (`tile.rs:156-167`, `plants.rs:150`). The plowed-year bonus must live on `Plot` and be applied at harvest.
- **`HitchingPost` is an empty marker.** Needs real fields (`faction_id`, `tile`, `reserved_by`, optional cart/plow slot) before it can be wired.
- **`AnimalWorkClaim` has no early-release path.** Only cleanup is TTL-based (`animals.rs:366-384`). v2 must remove the claim explicitly on task completion/cancellation, not rely on TTL — otherwise the same ox is unavailable for 60+ ticks after each job.
- **`DomesticAnimal.training` has no writer.** Needs an actual progress system; can't gate work on a field nothing updates.
- **`Calendar::current_year` doesn't exist.** Use `Seasons::years_elapsed() -> u32` (`src/world/seasons.rs:118`) cast to `u16`.
- **Nomadic pack inventory rolls up via `PackAnimalInventory`, not `AnimalUse::Pack`** (`src/simulation/faction.rs:3485-3573`). Cart-cargo rollup mirrors that pass, not the unused `Pack` claim.
- **`JobKind::PlowPlot` is one option among several.** Chief should evaluate per-village: depends on farmer knowledge + equipment + plot size + plow-vs-plant time budget.

## Locked design decisions

1. **Yield model:** `Plot.plowed_year: Option<u16>`. Plowing stamps the year. Planting inside a plot whose `plowed_year == Some(current_year)` attaches a `Tilled` marker to spawned `Plant` entities. Harvest applies a `1.4×` grain-yield multiplier when `Tilled` is present. Tilled lasts only for that year's crop; re-plow next year.
2. **Cart is a composable multi-part entity.** Parent `Cart` + child part entities (frame + 2–4 wheels). Each part carries its own material, size, and quality fields; effective capacity / speed / durability are derived from the combination. v2 ships **two cart sizes** (Handcart = 2-wheel light, OxCart = 2-wheel heavy; 4-wheel Wagon deferred to v2.1). Parts are craftable independently and assembled at a workbench.
3. **Plow routing is adaptive, evaluated by the chief per plot:**
   - **Inline plow+plant** (extend `JobProgress::Planting` with a tillage phase, or post `Plow` then `Farm` sequentially — see Open question) when: farmer knows `ARD_PLOW`, plot is small enough to plow+plant within the planting season, and a hitched ox is available.
   - **Separate `JobKind::Plow` posting** when: plot is large, or farmer lacks `ARD_PLOW` knowledge (needs a specialist), or chief wants to parallelise plowing and seed-fetch.
   - **Skip plowing** (plant unplowed, no yield bonus) when: faction lacks `ARD_PLOW` tech, no trained draft animal, or no plow implement in storage.

## Architecture

### A. Plot tillage state

```rust
// src/simulation/land.rs Plot
pub struct Plot {
    // existing fields ...
    pub plowed_year: Option<u16>,
}

// src/simulation/plants.rs
#[derive(Component)] pub struct Tilled;
pub const PLOW_YIELD_MULT: f32 = 1.4;
```

No explicit reset system needed: planting-dispatch gates on `plot.plowed_year == Some(current_year)`. Older stamps are functionally None.

### B. Implements (plow + cart parts)

**Resources** — `assets/data/resources/core.ron`:
- `ard_plow` — material, ~5 kg, sprite key `resource_ard_plow`.
- `cart_frame_small`, `cart_frame_medium` — ~10–25 kg.
- `cart_wheel_wood`, `cart_wheel_ironrim` — ~3–6 kg.

`core_ids.rs` auto-picks alphabetic IDs; add accessor helpers only.

**Crafting** (`src/simulation/crafting.rs`):
- `Ard Plow`: 4 wood + 1 tools, tech-gated `ARD_PLOW`, Workbench.
- `Wheel (wood)`: 3 wood + 1 tools, Workbench.
- `Wheel (iron-rimmed)`: 3 wood + 1 tools + 1 iron, gated on `IRONWORKING`.
- `Cart Frame (small)`: 5 wood + 1 tools.
- `Cart Frame (medium)`: 10 wood + 2 tools + 2 skin, gated `OX_CART`.
- `Assemble Cart`: 1 frame + 2 wheels, gated `OX_CART`. Recipe spawns a `Cart` entity by consuming the parts.

**Cart entity** — new module `src/simulation/cart.rs`:

```rust
#[derive(Component, Clone, Debug)]
pub struct Cart {
    pub size: CartSize,             // Handcart | OxCart (Wagon = v2.1)
    pub frame_quality: f32,         // 0.5..=1.5
    pub wheel_quality: f32,         // avg across child wheels
    pub durability: u16,
    pub owner_faction: u32,
    pub hitched_to: Option<Entity>, // animal currently pulling
    pub parked_at: Option<Entity>,  // HitchingPost when idle
}

#[derive(Component, Clone, Debug)]
pub struct CartInventory {
    pub items: [(ResourceId, u32); CART_SLOTS],   // 8 slots
    pub capacity_g: u32,                           // derived at spawn
}

#[derive(Component)] pub struct CartPart { pub parent: Entity, pub role: PartRole }
pub enum PartRole { Frame, Wheel(u8) }
pub enum CartSize { Handcart, OxCart }
```

`derive_cart_stats(frame, wheels) -> (capacity_g, speed_mult, durability)`:
- Capacity = `size.base_capacity_g() * frame_material_mult * (1.0 - wheel_drag_penalty)`.
  - Handcart base 50 kg; OxCart base 200 kg.
  - Hardwood frame ×1.2, softwood ×1.0.
  - Iron-rimmed wheels: no drag penalty; wooden wheels: −10% effective capacity.
- Speed mult: 0.9× human-walking for Handcart, 0.7× animal-walking for OxCart.
- Durability: ticks proportional to material; decremented per haul-tile traversed; below threshold triggers repair task (repair task itself is v2.1).

Cart sprite via `entity_sprites` reactive attach; sprite scales/changes with `CartSize` and material.

### C. HitchingPost made functional

```rust
// src/simulation/husbandry.rs
pub struct HitchingPost {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub parked_cart: Option<Entity>,
    pub parked_plow: Option<Entity>,
    pub reserved_by: Option<Entity>,
}
```

Helper `nearest_post_with_implement(faction, kind, from)` lets dispatchers find a tile holding the equipment they need. Chief postings prefer assembled+parked over crafting from scratch.

### D. Animal training

`DomesticAnimal.training: u8`, driven by a new `animal_training_progress_system` (Sequential, `TICKS_PER_DAY / 4` cadence) — mirrors `assign_preferred_home_system` rhythm:

- Animal must be `Tamed` and have `preferred_home` assigned (penned or stabled).
- Increment: +1 every 6 sim-hours while housed.
- Threshold: `training >= 80` → eligible as draft animal. Below 80, can still be `Pack`/`Mount` but not `Plow`/`Cart`.
- Species gating: only `Cattle` or `Horse` for `Plow`/`Cart`; pigs/dogs filtered at HTN precondition.

### E. New tasks

```rust
// src/simulation/typed_task.rs
Task::Hitch { animal: Entity, implement: Entity, post: Entity },
Task::Plow { plot: Entity, animal: Entity, plow: Entity },
Task::CartHaul { source_tile: (i32, i32), dest_tile: (i32, i32),
                 animal: Entity, cart: Entity, resource_id: ResourceId, qty: u32 },
Task::UnhitchAndPark { animal: Entity, implement: Entity, post: Entity },
```

`task_interacts_from_adjacent` arms: `Hitch`/`UnhitchAndPark` at post tile; `Plow` walks the plot rect tile-by-tile (re-routing inside the rect); `CartHaul` two-leg movement (source → dest).

Executors in `src/simulation/production.rs`:
- **`plow_task_system`** — walks next un-tilled tile in `plot.rect`; accumulates `PLOW_TICKS_PER_TILE = 8` ticks per tile; at end of last tile stamps `plot.plowed_year = Some(current_year)`, awards Farming XP, calls explicit early-release helper to remove `AnimalWorkClaim` + clear post `reserved_by`, then `aq.finish_task`. `MF_UNINTERRUPTIBLE` prevents mid-field hunger drops; safety check `aq.cancel_chain` on plow/animal despawn.
- **`cart_haul_task_system`** — two-phase. **Load phase** atomically transfers up to `cart.capacity_g` of `resource_id` from source into `CartInventory` (mirrors `buy_material_task_system` atomic pattern, `production.rs:630-781`, exclusive system). **Travel phase** walks animal+cart to dest. **Unload phase** atomically deposits into destination's storage. Releases claim, sets `Cart.hitched_to = None`, both park at nearest `HitchingPost`.
- **`hitch_task_system`** — 10-tick animation; inserts `AnimalWorkClaim { worker, use_kind, expires_tick: now + LONG_TTL }`, sets `Cart.hitched_to = Some(animal)` or attaches plow as an implement marker on the animal.
- **`unhitch_and_park_system`** — exclusive system; atomically removes claim, clears `hitched_to`, sets `parked_at = Some(post)`, sets `HitchingPost.parked_cart`/`parked_plow`.

### F. HTN methods

```rust
// src/simulation/htn.rs
AbstractTask::PlowPlot { plot_id }
AbstractTask::CartHaulMaterial { resource_id, source, dest, qty }
AbstractTask::HitchDraftAnimal { animal, implement }
```

All methods set `MF_UNINTERRUPTIBLE`:
- `PlowPlotMethod::expand` → `[Hitch{ox,plow,post}, Plow{plot,ox,plow}, UnhitchAndPark]`.
- `CartHaulMethod::expand` → `[Hitch{animal,cart,post}, CartHaul{...}, UnhitchAndPark]`.

Preconditions: faction tech, idle trained animal of right species (`training >= 80`), assembled implement (parked or in storage), reachable hitching post.

Dispatchers (ParallelB, after `goal_dispatch_system`):
- `htn_plow_plot_dispatch_system` — picks worker for open `JobKind::Plow` (or inline tillage phase on `Farm`).
- `htn_cart_haul_dispatch_system` — invoked from `AcquireGood` / haul-from-storage when distance × qty exceeds threshold (cart amortises only above hitch-overhead). Wages via `JobKind::Haul` reward.

### G. Adaptive chief routing

`chief_job_posting_system` (`src/simulation/jobs.rs`) calls `decide_plow_route(faction, plot, farmer, calendar)`:

```rust
fn decide_plow_route(faction, plot, farmer, calendar) -> PlowRoute {
    if !faction.techs.has(ARD_PLOW) { return PlowRoute::Skip; }
    let plow_available = storage_has_or_parked(faction, "ard_plow");
    let trained_animals = count_trained_draft(faction);
    if !plow_available || trained_animals == 0 { return PlowRoute::Skip; }

    let plot_tiles = plot.rect.area();
    let plow_time = plot_tiles * PLOW_TICKS_PER_TILE;
    let plant_time = plot_tiles * PLANT_TICKS_PER_TILE;
    let season_budget = TICKS_PER_SEASON; // spring

    if farmer.knows(ARD_PLOW) && plow_time + plant_time < (season_budget as f32 * 0.7) as u32 {
        PlowRoute::InlineFarmPosting    // farmer plows then plants
    } else {
        PlowRoute::SeparatePlowPosting  // specialist plows; farmer plants
    }
}
```

Standalone `JobKind::Plow` posting carries `JobProgress::Plowing { plot_id, plowed_tiles, target_tiles, assigned_worker }`. Reward 12.0 base, scaled by plot size.

Inline routing: either (a) extend `JobProgress::Planting` with a tillage phase, or (b) post a Plow job and have planting dispatch block on `plot.plowed_year == current_year`. See Open question — leaning toward (b) for minimum enum churn.

### H. Faction storage rollup for carts (settled)

Fourth pass in `compute_faction_storage_system` (`src/simulation/faction.rs:3485-3573`), mirroring nomadic pack pass but for settled factions:

```rust
for (cart, inv) in carts.iter() {
    let Some(faction) = registry.factions.get_mut(&cart.owner_faction) else { continue; };
    if !matches!(faction.caps.storage, StorageBackendKind::TileBased | StorageBackendKind::Hybrid) { continue; }
    // Count cart only when parked-near-storage OR in-transit on a Haul task.
    // Atomic on-arrival deposit (mirrors buy_material_task_system) avoids double-count windows.
    if !cart_is_at_or_near_storage(cart, &storage_tile_map) { continue; }
    for (rid, qty) in inv.iter() { *faction.storage.totals.entry(rid).or_insert(0) += qty; }
}
```

Cart-haul unload uses the exclusive-system pattern so the visibility transition happens within a single tick.

### I. Stables intent fix

The prior plan flagged a possible bug: `husbandry_intent_emitter_system` may only propose stables when horses are present. Add a unit test: settled faction + one tamed horse → assert a Stable blueprint is queued within one Economy tick. Fix the emitter only if the test fails.

## Files to modify

- `assets/data/resources/core.ron` — add `ard_plow`, `cart_frame_small`, `cart_frame_medium`, `cart_wheel_wood`, `cart_wheel_ironrim`.
- `src/economy/resource_catalog/core_ids.rs` — accessor functions.
- `src/simulation/crafting.rs` — recipes (Ard Plow, Wheel ×2, Cart Frame ×2, Assemble Cart).
- `src/simulation/cart.rs` (NEW) — `Cart`, `CartInventory`, `CartPart`; `derive_cart_stats`; assembly system.
- `src/simulation/animals.rs` — `animal_training_progress_system`; `release_animal_claim(commands, animal)` helper.
- `src/simulation/husbandry.rs` — extend `HitchingPost` with real fields + `nearest_post_with_implement`.
- `src/simulation/construction.rs:6838` — update `HitchingPost` spawn to populate new fields.
- `src/simulation/land.rs` — `Plot.plowed_year: Option<u16>`.
- `src/simulation/plants.rs` — `Tilled` marker; planting attaches `Tilled` when `plot.plowed_year == Some(current_year)`; harvest yield × `PLOW_YIELD_MULT`.
- `src/simulation/typed_task.rs` — new `Task` variants + `TaskKind` discriminants + `task_interacts_from_adjacent` arms.
- `src/simulation/production.rs` — `hitch_task_system`, `plow_task_system`, `cart_haul_task_system`, `unhitch_and_park_system` (exclusive).
- `src/simulation/htn.rs` — abstract tasks + methods + dispatchers; register in `register_builtin_methods`.
- `src/simulation/jobs.rs` — `JobKind::Plow`; `JobProgress::Plowing`; `decide_plow_route`; chief posting integration.
- `src/simulation/faction.rs` — fourth-pass cart rollup in `compute_faction_storage_system`.
- `src/rendering/entity_sprites.rs` + `src/rendering/sprite_library.rs` — Cart sprite (positioned relative to hitched animal) + part sprites.
- `src/simulation/mod.rs` — register new systems under appropriate `SimulationSet`.
- `src/simulation/CLAUDE.md` — update (training, hitching post, cart, plow, decide_plow_route).
- Top-level `CLAUDE.md` — update if any cross-cutting convention changes.

## Verification

**Unit tests** (`src/simulation/test_fixture.rs` patterns):
- `plow_task_stamps_plowed_year_after_full_field` — 4×4 plot, hitched ox + plow, dispatch `Task::Plow`, advance enough ticks; assert `plot.plowed_year == Some(current_year)` and `AnimalWorkClaim` removed.
- `plant_in_plowed_plot_carries_tilled_marker` — plow → plant → assert spawned `Plant` has `Tilled`.
- `harvest_tilled_grain_yields_1_4x` — fixed seed; tilled vs untilled identical plots; ratio ≈ 1.4.
- `cart_haul_atomic_transfer_settles_at_destination` — cart loaded, hauled; intermediate-tick failure injection asserts no duplication or loss (capital-style invariant).
- `hitch_releases_on_task_complete` — claim removed before TTL.
- `cart_capacity_scales_with_parts` — Handcart vs OxCart matched materials → OxCart > 3× Handcart.
- `iron_rim_wheel_outperforms_wood_wheel` — same frame, wheels swapped → iron-rim capacity higher.
- `untrained_animal_cannot_be_claimed_for_plow` — `training < 80` → HTN precondition false.
- `chief_inline_vs_separate_plow_routing` — small plot + trained farmer → inline; large plot → separate posting.
- `stable_intent_emitted_when_horse_owned` — confirms or refutes v1 bug.

**End-to-end** (`cargo run`, Bronze era, Settled, faction with cattle + farms):
1. After `ARD_PLOW` researched and plot has assigned farmer, chief posts `Plow` (or inline) job.
2. Activity log shows ox claimed (`AnimalWorkClaim::Plow`), walked to plot, walks rows; on completion plot marked plowed.
3. Plant the plot; observe higher grain yield at harvest vs control faction without plow.
4. Craft cart parts → assemble → park at hitching post.
5. Trigger a large `JobKind::Haul` (e.g., 200+ stone); confirm cart chosen over hand-carry, capacity honoured, atomic deposit at destination.

## Out of scope (v2.1+)

- 4-wheel Wagon size.
- Cart repair task (durability decrements but no repair flow yet).
- Composting + manure (separate plan).
- Player-direct hitch/unhitch UI command.
- Animal fatigue / rest on plow days.
- Plow blade quality tiers (bronze vs iron ard).

## Open question

Extending `JobProgress::Planting` with a tillage phase grows a pattern-matched enum. Cleaner alternative: keep `Planting` purely about seeding; chief posts a `Plow` job first, and the planting dispatcher blocks until `plot.plowed_year == current_year`. Mechanically equivalent, less enum churn. Default to the **two-posting** approach unless inline reward bonuses materially change worker behaviour.

## Progress

### v2.0 — shipped (2026-05-20)

Plow half end-to-end via the standard `JobBoard` + `JobClaim` + `record_progress_filtered` pipeline. Cart half deferred.

- `Plot.plowed_year: Option<u16>` added to `Plot` (`src/simulation/land.rs:178`). Threaded through all four constructors: `carve_plots_system` (land.rs), `seed_starting_farms_system` (farm.rs), and three test_fixture sites.
- `Tilled` marker component (`src/simulation/draftwork.rs`).
- `PLOW_YIELD_MULT_NUMER / DENOM = 7 / 5` (= 1.4×) applied to `grain_yield_for_nutrients(...)` base in `gather_system`'s Grain branch when the harvested Plant entity carries `Tilled`. `GatherRoutingResources` gained a `tilled_q` field to stay under the 16-param ceiling.
- `production_system`'s Planter branch inserts `Tilled` on freshly-spawned Grain plants when their tile sits inside a plot with `plowed_year == Some(calendar.year)`. Gated to Grain only (other crops have no plow bonus).
- `Task::Plow { plot_entity, animal }` variant + `as_plow()` accessor + `task_kind_for` arm.
- `TaskKind::Plow = 53` + `task_kind_label` ("Plowing") + `task_requires_free_hands = 1` + `task_interacts_from_adjacent` + `task_is_labor` arms.
- `plow_task_system` (Sequential, before `gather_system`): lump-sum work model (`plot.area() * PLOW_WORK_TICKS_PER_TILE`), stamps `plot.plowed_year`, releases `AnimalWorkClaim` via `release_animal_work_claim`, awards 8 Farming XP, defence-in-depth cancel on despawned plot.
- `chief_plow_dispatch_system` (Economy, daily Spring): per faction with `ARD_PLOW` Aware, matches one un-plowed plot + one trained Cattle/Horse + one idle Farmer; routes worker via `assign_task_with_routing`, dispatches `Task::Plow`, inserts `AnimalWorkClaim { use_kind: Plow, expires_tick: now + 2 days }`.
- `animal_training_progress_system` (Sequential, `TICKS_PER_DAY/4` cadence): `+1` training per pass while `preferred_home.is_some()`. Cattle/Horse reach threshold 80 in ~80 days. `TRAINING_THRESHOLD_DRAFT = 80` constant.
- Module registered in `simulation/mod.rs`; systems wired into Sequential / Economy / ParallelB.
- `src/simulation/CLAUDE.md` updated with the new "Animal husbandry v2 — draftwork (`draftwork.rs`)" section.
- **`ard_plow` resource** (`assets/data/resources/core.ron` + `core_ids::ard_plow`): tool class, `two_hand`, 5 kg, trade value 60, reuses `item_hammer` sprite.
- **"Ard Plow" craft recipe** (recipe id 14, `crafting.rs`): Wood×3 + Tools×1 → 1 `ard_plow`, 70 work ticks, `ARD_PLOW`-gated, Workbench-bound.
- **`JobKind::Plow` + `JobProgress::Plow`** in `jobs.rs`. Posting goes through standard chief posting pipeline: dedupe per pass, reachability check, target_tiles == plot.area, assigned_worker from `FarmPlotAssignments`, animal stamped at first dispatch. `JobKind::to_goal()` maps to `AgentGoal::Farm` (Plow piggybacks on the Farm goal so workers already on Farm pick up Plow claims). `posting_target_workers(Plow) = 1`. `chief_wage_for(Plow)` mirrors Farm. `posting_target_tile` returns plot centre. `same_target` matches by `plot_id`.
- **Chief posting branch** (`jobs.rs`, ~lines 3117-3210) gates on `ARD_PLOW` tech + `ard_plow` storage stock > 0 + Spring + un-plowed plot + reachable rect.
- **`job_claim_system` assigned-worker gate** — only the posting's `assigned_worker` (when set) can claim.
- **`htn_plow_dispatch_system`** (ParallelB) — walks each `JobClaim::Plow` holder, picks next tile via `tile_at_index(area.min, area.max, posting.plowed_tiles)`, picks animal via `pick_idle_draft_animal` (prefers un-claimed trained Cattle/Horse) OR falls back to `None` (human-drawn) when no ox is available. Routes worker, dispatches `Task::Plow { plot_entity, animal: Option<Entity> }`. When the dispatcher newly commits to an ox, stamps `posting.animal = Some(ox)` + inserts `AnimalWorkClaim` with TTL backstop. Human-drawn dispatches leave the posting's animal as `None` so subsequent passes can upgrade if an ox becomes free.
- **Human-drawn plowing fallback.** `Task::Plow.animal: Option<Entity>` — `Some(e)` for ox-drawn, `None` for human-drawn. Per-tile cost is `plow_work_ticks(animal): 6 if ox else 12`. Same `Tilled` marker + same 1.4× harvest bonus regardless of mode; the difference is pure throughput. Historically accurate (the ard is a simple scratch plow that humans can pull, just slowly). The fallback also solves the "Farmer claims Plow posting but can never execute because no ox exists" stuck-claim concern — every posting is always executable, just slower without animals.
- **`plow_task_system`** (Sequential, before `gather`) — per-tile work model with per-mode cost: `plow_work_ticks(animal)` = 6 (ox) or 12 (human). Credits `posting.plowed_tiles += 1`, defence-in-depth on vanished plot. On final tile: stamps `plot.plowed_year`, fires `JobCompletedEvent` via `record_progress_filtered`, releases AnimalWorkClaim (when animal is Some) + JobClaim, grants completion XP.
- **`goal_dispatch_system` preserve-arm** for `(AgentGoal::Farm, TaskKind::Plow)` keeps the chain alive across mid-job ticks.
- **`projects::compute_priority` + `WorkforceBudget::share`** arms for `JobKind::Plow` (rides the Farm slot).
- **`record_progress_filtered`** in `jobs.rs` now handles `JobProgress::Plow` for the completion accounting path so payout flows through the standard `JobEscrow` mechanism.
- **Unit tests** (`draftwork::tests`): `plow_yield_bonus_applies_seven_fifths_per_tier`, `plow_yield_bonus_is_monotonic_increasing`, `training_threshold_is_reachable`, `plow_total_work_is_bounded_for_default_plot`, `tile_at_index_walks_row_major`.
- **Fixture tests** (`test_fixture::smoke`): `spring_chief_posts_plow_job_when_tech_and_implement_and_unplowed_plot`, `no_plow_posting_when_ard_plow_implement_missing`, `no_plow_posting_when_plot_already_plowed_this_year`, `plow_executor_stamps_plowed_year_after_target_tiles` (drives the executor through 4 tiles via `register_system` + `run_system`, asserts plot stamped + posting despawned + claims released + queue Idle), `plow_executor_stamps_plowed_year_under_human_drawn_fallback` (same flow with `animal: None`, asserts the fallback path completes), `human_drawn_plowing_requires_higher_work_progress` (pins that animal-cost work_progress doesn't credit a human-drawn tile, but human-cost does).
- **`crafting::tests::craft_recipe_inputs_resolve_to_known_resources`** bumped to 15 recipes + asserts the Ard Plow shape.
- **887/887 tests pass.**

### v2.1 — cart half shipped (2026-05-20)

Cart half end-to-end via the standard `JobBoard` + `JobClaim` (`JobKind::Haul`) pipeline — mirrors how v2.0 shipped the plow. New module `src/simulation/cart.rs`.

- **Cart-part resources** (`assets/data/resources/core.ron` + `core_ids`): `cart_frame_small`, `cart_frame_medium`, `cart_wheel_wood`, `cart_wheel_ironrim` — `material` class, `two_hand` bulk, `draftwork`/`cart_part` tags.
- **Crafting recipes 15-18** (`crafting.rs`): Cart Wheel (Wood) `ANIMAL_HUSBANDRY`, Cart Wheel (Iron-Rimmed) `BRONZE_CASTING` (3 wood + 1 tools + 1 iron), Cart Frame (Small) `ANIMAL_HUSBANDRY`, Cart Frame (Medium) `OX_CART`. Recipe-count test bumped 15→19.
- **`Cart` + `CartInventory` + `CartSize` + `CartVisual`** components; `derive_cart_stats(frame, wheels) → (CartSize, capacity_g, durability)`. Handcart 50 kg / OxCart 200 kg base; wooden wheels −10% drag (`9/10`), iron-rimmed none. Composable parts are stored as frame/wheel `ResourceId` data on the `Cart` (not separate child entities — a simplification; `derive_cart_stats` still composes them).
- **`cart_assembly_system`** (Economy daily): one cart per qualifying settled faction (`ANIMAL_HUSBANDRY` + free `HitchingPost` + no cart). Prefers a pre-crafted `1 frame + 2 wheels` from storage, else raw-timber fallback (Handcart 15 wood + 2 tools / OxCart 30 wood + 4 tools). OxCart frame at `OX_CART`; iron wheels at `BRONZE_CASTING` + iron stock.
- **`htn_cart_haul_dispatch_system`** (ParallelB): routes `JobClaim::Haul` holders through a cart when the posting needs ≥ `CART_HAUL_MIN_REMAINING = 12` units. Resumes the worker's in-flight cart or hitches a fresh parked cart + trained Cattle/Horse (`AnimalWorkClaim { use_kind: Cart }`). Phase keyed on `CartInventory`: empty → route to storage tile (load); loaded → route to blueprint (deliver). Per-pass animal-claim dedupe set.
- **`cart_haul_task_system`** (Sequential): load phase fills the cart from `ai.dest_tile` storage capped at `min(blueprint need, capacity_units)`; deliver phase deposits into `bp.deposits`, credits `record_progress_filtered(JobKind::Haul, …)`, decrements durability, and on completion releases the animal claim + re-parks the cart + drops the `JobClaim`. Storage-empty / vanished-blueprint abort cleanly.
- **`cart_follow_system`** (Sequential after movement) snaps a hitched cart's `Transform` behind its hauler.
- **`HitchingPost`** extended with `parked_cart` / `reserved_by` (was an inert marker); `HitchingPost::new` constructor; construction spawn site updated.
- **Faction storage rollup** — fourth pass in `compute_faction_storage_system` folds every `CartInventory` into settled-faction `storage.totals` (conservation-correct across in-flight hauls).
- **`Task::CartHaul` + `TaskKind::CartHaul = 54`** + `as_cart_haul()` + `task_kind_for` / label / `task_is_labor` / `task_interacts_from_adjacent` arms; `(AgentGoal::Haul, CartHaul)` preserve-arm.
- **Cart sprite** — `entity_cart` (16×16 side-view cart) in `sprite_library.rs`; `entity_sprites::spawn_cart_sprites` reactive attach; registered in `rendering/mod.rs`.
- **`Hitch`/`UnhitchAndPark` folded into the `CartHaul` executor** (no separate animation tasks — same precedent as the v2.0 plow's direct ox claim/release).
- **Tests** — `cart::tests` (5 unit: capacity composition, iron-vs-wood wheels, size classification, capacity units, inventory roundtrip); `test_fixture::smoke::{cart_assembly_builds_cart_from_storage_timber, cart_haul_executor_loads_then_delivers_and_credits_posting}`. **905/905 tests pass.**
- `src/simulation/CLAUDE.md` updated ("Animal husbandry v2.1 — carts").

### v2.2 — close-out (2026-05-22)

Remaining genuinely-actionable items finished; the plan's draft scope is complete.

- **Stables-intent fix.** `husbandry_intent_emitter_system` (`husbandry.rs`) now splits the per-faction census by housing class: horses count against Stable capacity, cattle/pig/dog against Pen capacity (cats ignored — no housing structure exists). It emits a `Stable` when horses outgrow stables (Stable need wins ties — a horse with no stable is unhouseable) and a `Pen` when pen-species outgrow pens. Previously it emitted only `Pen`, so a horse-owning faction got a pen its horses could never list as `preferred_home`. Iteration is now deterministic (sorted faction ids).
- **Behavioural fixture tests** (`test_fixture::smoke`): `plant_in_plowed_plot_carries_tilled_marker` (drives `production_system` — Grain sown in a `plowed_year == current_year` plot gets `Tilled`, un-plowed plot does not), `harvest_tilled_grain_yields_1_4x` (drives `gather_system` — a `Tilled` Grain plant harvests for 7 vs the un-tilled 5 at identical nutrients), `husbandry_emitter_queues_stable_for_horse_owner` (one tamed horse → a `Stable` blueprint, never a `Pen`). **1025/1025 tests pass.**

### Resolved by design / superseded — not built

- **Adaptive plow routing (`decide_plow_route`).** The Open question defaulted to the **two-posting** approach (chief posts `Plow`; planting blocks on `plowed_year`), and that is what shipped in v2.0. Inline plow+plant was the rejected alternative — there is no remaining work, only the unbuilt rejected branch.
- **4-wheel Wagon + cart repair.** Superseded — `cart.rs` was deleted and the whole cargo-vehicle line moved to the Vehicle system (`vehicle.rs`), which ships the Four-Wheel Wagon and models per-cell durability. Cart repair is a vehicle-system concern, not draftwork.
- **Player-direct hitch/unhitch UI command.** Also moved to the Vehicle system — `PlayerCommand::VehicleOrder { kind: Hitch/Unhitch }` exists there. No draftwork-specific UI is needed.

### Known follow-up gotchas

- The `animal_work_claim_expiry_system`'s 60-tick TTL sweep covers stranded claims, but the plow executor already explicit-releases on every exit. The 2-day `expires_tick` is the failsafe.
- `plow_task_system` queries `&mut Plot` mutably; runs in Sequential, no scheduling conflict with the read-only `Query<&Plot>` added to `production_system` (Bevy serialises them, separate systems).
- Dispatcher gates on `Calendar.season == Spring`. A faction researching `ARD_PLOW` mid-Summer waits until next Spring before the chief schedules plowing.
- Plowing is currently free labor (no chief-funded escrow). When the cart half adds `JobKind::Plow` postings, funding will land alongside.
