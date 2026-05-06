# Simulation (`src/simulation/`)

Agent AI, factions, knowledge, hunting, and the typed-task pipeline. See the root `CLAUDE.md` for the cross-cutting `SimulationSet` ordering and global conventions.

## Behavioural test fixture (`test_fixture.rs`)

Headless `App` harness for asserting AI behaviour without rendering / UI / globe generation. Build a `TestSim::new(seed)`, scaffold terrain with `flat_world(radius, z, kind)`, spawn agents via `spawn_person(faction, tile, |b| b.hunger(...).add_inventory(...))`, optionally place storage tiles + ground items, then drive the schedule with `tick()` / `tick_n(n)`.

- Time is deterministic via `TimeUpdateStrategy::ManualDuration` — every `app.update()` advances `Time` by exactly one fixed-tick (1/20 s).
- Game state stays in `SpawnSelect` so `OnEnter(Playing)` systems (which spawn 200 people on the real globe) never fire; FixedUpdate sim systems run regardless of state.
- A camera entity is spawned at world origin so `update_lod_levels_system` doesn't drop every test agent to `Dormant`.
- Test helper: `test_fixture::person_task(&app, entity) -> Task` reads `ActionQueue.current`. Use it instead of any defunct `PersonAI.task` field.

## Agent AI (Goals → Plans / HTN → Tasks)

- **Goals (`goals.rs`):** High-level objectives (`Survive`, `GatherWood`, `GatherStone`, `GatherFood`, `Haul`, `Defend`, `Farm`, `Build`, `Socialize`, `Sleep`, ...) driven by Needs + Faction state.
- **Plan registry (`plan/`):** Multi-step sequences scored by `dot(state, plan.state_weights) + plan.bias` plus manual bonuses (persistence, ally influence, dist-weighted memory penalty). `PlanScoringMethod::Weighted` is normal; `Random` is ε-greedy. Module split: `plan/mod.rs` (types, `ActivePlan`/`PlanHistory`/`KnownPlans`, `resolve_target`, `plan_execution_system`), `plan/registry.rs` (step+plan tables), `plan/state.rs` (`build_state_vec` + `count_visible_*`).
- **HTN registry (`htn.rs`):** Per-goal decomposition path. Methods carry `precondition` / `utility` / `expand(ctx) -> Vec<Task>`; the dispatcher argmaxes over applicable methods and routes the head into the typed-task channel, with the tail prefetched on `ActionQueue`'s queue ring. Authoritative for `Sleep`, `Eat`, `AcquireFood` (under `Survive`), `AcquireGood` (under `Haul` / `GatherWood` / `GatherStone`), and `StockpileFood` (under `GatherFood`). Plan registry handles everything else.
- **`PlanId` / `StepId` (`plan/mod.rs`):** Newtype wrappers around `u16`/`u8` with a named `pub const` per registered plan/step (e.g. `PlanId::HUNT_FOOD`, `StepId::CHOP_FOREST`). Bare `PlanId(n)` / `StepId(n)` constructors are public for migration / serialisation only — every code path that writes a plan or step ID should use the named const so typos surface at compile time. `PersonAI.last_plan_id` is `u16` because the sentinel `UNEMPLOYED = u16::MAX` predates the typed wrappers; comparisons round-trip via `.raw()`.
- **PlanFlags (`PlanDef.flags`):** `PF_EXPLORE`, `PF_SCAVENGE`, `PF_TARGETS_FOOD/WOOD/STONE`, `PF_DROP_FOOD_ON_TIMEOUT`, `PF_UNINTERRUPTIBLE`. Candidate filter reads flags, not plan-id matches. `PF_UNINTERRUPTIBLE` plans (e.g. BuildBlueprint, the craft pipeline, HaulFromStorageAndBuild) survive a goal flip — they end only via completion, timeout, target invalidation, precondition fail, or external preempt.
- **Tasks (`tasks.rs`):** The agent's *current* action — `TaskKind` enum (Gather, Construct, WithdrawMaterial, Hunt, Butcher, Equip, ...). Transient; managed by `plan_execution_system` and the HTN dispatchers. `Idle` between tasks.
- **Professions (`person.rs::Profession`):** `None | Farmer | Hunter`. Persistent role. Farmer auto-assigned by `faction_profession_system` when food < 100. Hunter is chief-driven for every faction via `faction_hunter_assignment_system` (Economy, every `TICKS_PER_DAY/4`): target headcount = `max(1, adults*0.20)` × martial × prey-density, capped at adults/2; demotes do full teardown. Plans gate on `PlanDef.requires_profession`.
- **Skills (`skills.rs`):** `[u8; 8]` — Farming, Mining, Building, Trading, Combat, Crafting, Social, Medicine. Default 5; `gain_xp()` saturating.
- **Bucketing:** Agents sliced across 20 fixed `BucketSlot`s to spread CPU.
- **LOD:** `Detail / Aggregate / Dormant`. Dormant entities skip sim entirely. Focus-aware (see Game lifecycle below).
- **Memory & gossip (`memory.rs`):** Known locations + agent sightings, `u8` freshness decay, shared via `plan::plan_gossip_system`. `MemoryKind` is a 3-variant `Copy` enum: `AnyEdible` (aggregate "any food" for AcquireFood / StockpileFood / Forage), `Resource(ResourceId)` (any specific catalog resource — Wood / Stone / GrainSeed / BerrySeed today, any new tagged resource for free), `Prey` (animal entity). Helper constructors `MemoryKind::wood() / stone() / grain_seed() / berry_seed()` resolve `core_ids::*` to keep call sites terse. Adding a new gather-goal resource = new construction of `MemoryKind::Resource(id)` at the dispatcher / writer; no enum touch.
- **Needs:** 6 needs (hunger, sleep, shelter, safety, social, reproduction), `[0,255]`, decay over time, feed goal selection.

## Plan-design rules of thumb

- **Plan scoring is *viability*, not motivation.** Inside a need-driven goal the triggering need is constant across candidates, so weighting it again is circular noise. Plan weights answer "would this succeed and produce value?" via `SI_HAS_*` (inventory), `SI_VIS_*` (visibility), `SI_MEM_*` (memory), `SI_SKILL_*`, `SI_STORAGE_*`. Need-slots 0–5 are populated for inspector readout but not currently weighted (`#[allow(dead_code)]`). Bias is for "this is the cheap-and-immediate right answer."
- **Source vs good visibility (`STATE_DIM=42`):** Sources (slots 35–37: `SI_VIS_PLANT_FOOD`, `SI_VIS_TREE`, `SI_VIS_STONE_TILE`) feed Forage/Gather. Goods (slots 38–40: ground wood/stone/food) feed Scavenge. Never collapse the two — gather/farm gate on source-vis OR memory; `PF_SCAVENGE` plans gate on the matching ground slot; `PF_EXPLORE` plans invert (need both source-vis AND ground-vis to be zero).
- **Farming is `Farm`-goal only.** Plans `FarmFood`, `PlantFromStorage`, `PlantBerryFromStorage` never appear under Survive/GatherFood. Seed↔plant mapping is centralised in `PlantKind::seed_good()` + `PlantKind::ALL` (`plants.rs`); the Planter executor walks `PlantKind::ALL` and consumes one seed from carrier-or-inventory via `consume_one_good` (`production.rs`). Adding a new seed/plant pair = new `Good` variant + new `PlantKind` variant + arm in `seed_good()` + arm in `Good::is_seed()`. `StepPreconditions::requires_good` checks inventory + carrier so harvested seeds (which land in hands) satisfy plant-step gating.
- **Faction storage stocks in state vector (slots 29–32, 34, 41):** `SI_STORAGE_FOOD/WOOD/STONE/GRAIN_SEED/BERRY_SEED`, normalised against `STORAGE_SATURATE=20`. Refreshed by `compute_faction_storage_system` (Economy). Lets withdraw/haul plans score on actual stock; lets producers self-throttle when storage is full.

## ActionQueue and typed Task variants (`typed_task.rs`)

The agent's typed task lives on its own `ActionQueue` component — `aq.current: Task` is the canonical "task running now," `Task::Idle` is default, every Person spawn site bundles `ActionQueue::idle()`. Behind `current` sits a fixed-capacity `queued: [Task; 4]` prefetch ring (private fields; access via `enqueue` / `pop_next` / `peek_next` / `queued_len` / `queued_is_empty` / `clear_queued`).

- **Producers** (`plan_execution_system`, the HTN dispatchers, the player-order handlers in `ui/orders.rs`, the `ReadItem` handler in `teaching.rs`) route through `aq.dispatch(task)` rather than writing `current` directly. `dispatch` enqueues then promotes the head into `current` if `current == Idle`.
- **Consumers** (every executor exit path in `gather.rs`, `dig.rs`, `corpse.rs`, `construction.rs`, `items.rs`, `production.rs`, `teaching.rs`, plus the `MilitaryMove` arrival in `military.rs`) call `aq.advance()` instead of writing `current = Task::Idle`. `advance` pops the head of the queue, or sets `current = Idle` when empty.
- **External preempts** (`apply_muster_hunters_system`, `faction_hunter_assignment_system` demote, `goal_dispatch_system` stale reset) call `aq.cancel()`, dropping both `current` and the prefetched queue so chained follow-ups can't outlive the plan/method that produced them.
- Per-tick "pin" sites (the lecture / teach pin writes in `teaching.rs`) deliberately stay as direct `aq.current = X` writes — they're idempotent re-assertions, not fresh dispatches; routing them through `dispatch` would pile duplicates onto the queue every tick.

Current variants:

- `Idle` — between tasks.
- `WalkTo { tile, z, why: WalkReason }` — pure movement; `WalkReason` tags the dispatch context (currently `MilitaryMove` and reserved `Gather`).
- `WithdrawGood { filter: WithdrawGoodFilter }` — `Specific(ResourceId)` or `AnyEntertainment`.
- `WithdrawMaterial { resource_id, qty }` — withdraw N of a specific catalog resource from storage.
- `WithdrawFood { tile }` — pull one edible off the named faction-storage tile.
- `Equip { slot: EquipmentSlot, resource_id: ResourceId }`.
- `Construct { blueprint: Entity }` — covers Construct + ConstructBed (the `task_id` discriminant survives for reward scaling).
- `Gather { tile }`, `Dig { tile }`, `Scavenge { target: Entity }`.
- `Read { tech }`, `Teach { tech }`, `HoldLecture { tech }`, `AttendLecture { tech }` — all in `teaching.rs`.
- `PickUpCorpse { corpse: Entity }` — first leg of hunt → carry → butcher.
- `Sleep { bed: Option<Entity> }` — bookkeeping only (the executor is a state transition, not a per-tick task system).
- `Eat` — parameterless, in-place. Executor (`production::eat_task_system`) inspects inventory + hands for the smallest-cover-then-largest selection.
- `HaulToBlueprint { blueprint }` — carry hand contents to the named `Blueprint` and drop into deposit slots. Trailing leg of `[WithdrawMaterial, HaulToBlueprint]`.
- `DepositToFactionStorage { resource_id }` — carry hand contents to the nearest faction storage tile and drop. Trailing leg of gather/scavenge chains.
- `Explore { kind: MemoryKind }` — walk to a random reachable tile near faction home, hoping to record a matching sighting.

**Rules of the road:**

- Systems that mutate the typed task **must** include `&mut ActionQueue` in their query alongside `&mut PersonAI`. In `plan_execution_system` the component lives in `OptionalQuery` as `Option<&mut ActionQueue>` (the 15-tuple ceiling on `AgentQuery` is full); the executor `expect`s it, since every spawn bundle adds one.
- Executor reads its parameters from the typed variant (with optional fallback to a legacy field where one survives, e.g. `Construct` falls back to `target_entity` for the HaulMaterials path).
- Every executor exit path (success, abort, timeout, precondition fail) must `aq.advance()` (or `aq.cancel()` for chain-drop) to prevent stale params leaking to the next task.
- External teardowns must clear both `aq` and the legacy `task_id`.
- Inconsistent-state guard: `military_task_system`, `withdraw_good_task_system`, and `withdraw_material_task_system` fall back to `Idle` if `task_id == X` but `aq.current` is the wrong variant — defence in depth against a forgetful dispatcher.

## HTN domain (`htn.rs`)

Five abstract tasks are dispatcher-live: `Sleep`, `Eat`, `AcquireFood` (Survive case), `AcquireGood { resource_id }` (Haul + GatherWood/Stone), `StockpileFood` (chief-driven GatherFood).

### Core types

- **`AbstractTask` / `AbstractTaskKind`** — the high-level goal a method decomposes. `Sleep`, `Eat`, `AcquireFood`, and `StockpileFood` are parameterless; `AcquireGood { resource_id }` carries the target catalog `ResourceId` so one method body serves every material. AcquireFood ends in `Eat` (cover personal hunger); StockpileFood ends in `DepositToFactionStorage` (chief-driven storage-fill). AcquireFood gates on a hunger condition; StockpileFood gates on a `JobClaim::Stockpile{food}` posting.
- **`Method` trait** — `precondition` / `utility` / `expand(ctx) -> Vec<Task>` / `flags` / `tech_gate` / `profession_gate` / `name`. Methods read only the `PlannerCtx` fields they care about; the ctx is a borrowed snapshot built per-decision, not a long-lived component. New ctx fields land on demand; Sleep-only sites leave them at zero / `None`.
- **`MethodRegistry`** — `Resource`, `AHashMap<AbstractTaskKind, Vec<Box<dyn Method>>>`. Built once in `SimulationPlugin::build` via `register_builtin_methods`. Read-only after init so dispatch systems can borrow immutably in parallel.
- **`MethodFlags`** is a plain `u8` bitmask (no `bitflags` dep). `MF_UNINTERRUPTIBLE` mirrors `PF_UNINTERRUPTIBLE`.
- **`MethodId`** is a `u16` newtype with one `pub const` per registered method (mirrors `PlanId`). Returned by `Method::id()`. Stable identity for `MethodHistory` keying and the `PersonAI.active_method` per-agent stamp.
- **`MethodHistory`** is a per-agent ring buffer (`METHOD_HISTORY_LEN=2`, `METHOD_HISTORY_TTL_TICKS=100`) of `(MethodId, MethodOutcome, tick)` entries. Mirrors `PlanHistory` shape exactly. Spawned at every Person spawn site (`person.rs`, `reproduction.rs`, `test_fixture.rs`).
- **Failure-biased scoring (Phase 6b):** the multi-method dispatchers (`htn_acquire_food_dispatch_system`, `htn_acquire_good_dispatch_system`, `htn_stockpile_food_dispatch_system`) score candidates via `score_method_with_history(method, abstract_task, ctx, history, now) = utility - failures * METHOD_FAILURE_PENALTY` (penalty `0.5` per recent failure within TTL). Routing failures and missing-target failures push `MethodOutcome::FailedRouting` / `FailedTarget` into the agent's `MethodHistory` so the next decision tick biases away from the same method. Sleep and Eat dispatchers stay on bare `utility()` — single-method registries with nothing to bias.
- **Success recording (Phase 6b-ii):** each multi-task HTN dispatcher stamps `PersonAI.active_method = Some(method.id())` after a successful `aq.dispatch(...)`, and clears it before pushing any `Failed*` outcome. `htn_method_completion_system` (Economy, after `drop_items_at_destination_system`) observes `aq.current == Idle && queued_is_empty && active_method.is_some()` and records `MethodOutcome::Success` against `MethodHistory`, then clears `active_method`. The Economy slot catches both Sequential-finishing chains (Eat / Withdraw / Gather / Scavenge — those executors call `aq.advance()` in Sequential) and Economy-finishing chains (DepositResource — finalised by `drop_items_at_destination_system`). `eat_task_system` calls `aq.advance()` on completion so the typed channel actually drains; without it the queue ring would accumulate stale `Task::Eat` entries each dispatch tick. `aq.cancel()` at non-instrumented external-preempt sites still leaves an `active_method` stamp behind, so the completion system records a noisy `Success` for the canceled chain; `score_method_with_history` ignores `Success` outcomes anyway (`recently_failed_count` filters on `is_failure()`), so the residual noise is benign until success-rate weighting actually consumes it.

### Methods

| Abstract task | Method | Utility | Expansion |
|---|---|---|---|
| `Sleep` | `SleepMethod` | n/a | `[Sleep { bed }]` (own-bed → faction-home → in-place) |
| `Eat` | `EatFromInventoryMethod` | 1.0 | `[Eat]` (gates on `edible_count > 0 && hunger ≥ EAT_TRIGGER_HUNGER`) |
| `AcquireFood` | `WithdrawFromStorageMethod` | 1.0 | `[WithdrawFood { tile }, Eat]` |
| `AcquireFood` | `ScavengeFoodFromGroundMethod` | 1.5 | `[Scavenge { target }, Eat]` (eat-on-the-spot) |
| `AcquireFood` | `ForageFromKnownMethod` | 1.0 | `[Gather { tile }, Eat]` (mature plant in memory) |
| `AcquireFood` | `ExploreForFoodMethod` | 0.3 | `[Explore { kind: Food }]` (fallback) |
| `AcquireGood` | `WithdrawMaterialFromStorageMethod` | 1.0 | `[WithdrawMaterial { good, qty: 1 }]` |
| `AcquireGood` | `WithdrawAndHaulToBlueprintMethod` | 2.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial, HaulToBlueprint { blueprint }]` |
| `AcquireGood` | `GatherFromKnownMethod` | 1.0 | `[Gather { tile }, DepositToFactionStorage { good }]` |
| `AcquireGood` | `ScavengeFromGroundMethod` | 1.5 | `[Scavenge { target }, DepositToFactionStorage { good }]` |
| `AcquireGood` | `ExploreForMaterialMethod` | 0.3 | `[Explore { kind }]` (Wood/Stone only; Iron/Fruit rejected) |
| `StockpileFood` | `ScavengeFoodForStorageMethod` | 1.5 | `[Scavenge, DepositToFactionStorage { good }]` (no hunger gate) |
| `StockpileFood` | `ForageFromKnownForStorageMethod` | 1.0 | `[Gather { tile }, DepositToFactionStorage { good }]` |
| `StockpileFood` | `ExploreForFoodForStorageMethod` | 0.3 | `[Explore { kind: Food }]` |

**Utility tiers (Phase 6c).** Method base utilities are pulled from four named consts in `htn.rs` (next to `METHOD_FAILURE_PENALTY`): `UTIL_CLAIMED_HAUL=2.0` (active `JobClaim` for a specific blueprint+material), `UTIL_VISIBLE_GROUND=1.5` (concrete loose `GroundItem` visible / freshly remembered), `UTIL_BASELINE=1.0` (sleep, eat, withdraw-from-storage, gather-from-known), `UTIL_EXPLORE_FALLBACK=0.3` (no concrete option fires). Tuning the inter-tier ranking touches one block instead of every method body.

**Distance-weighted utility.** Methods whose ctx carries a target tile (`nearest_storage_tile`, `material_storage_tile`, `gather_target_tile`, `scavenge_target_tile`) subtract `chebyshev_dist(ctx.tile, target) * DIST_DISCOUNT_PER_TILE` from their base utility, capped at `MAX_DIST_PENALTY` (0.30). The cap preserves the inter-tier ranking (Scavenge `1.5 - 0.30 = 1.20` still beats Gather/Withdraw `1.0`; Haul `1.70` still beats bare-withdraw at any spread). Within a method, closer targets outscore farther ones, so "nearest applicable" semantics survive method-level argmax. Haul + gather/scavenge methods discount on full-trip distance (`agent → storage → blueprint` or `agent → target → deposit`) when both ctx tiles are populated; methods whose chain ends in `Eat` use single-leg discount since the second hop is in-place.

### Dispatch systems

All run in `ParallelB`, ordered after `goal_dispatch_system` and chained `htn_dispatch_system → htn_eat_dispatch_system → htn_acquire_food_dispatch_system → htn_acquire_good_dispatch_system → htn_stockpile_food_dispatch_system`. Each system serialises on `&mut PersonAI` / `&mut ActionQueue` so the per-goal split costs no parallelism while keeping per-goal component-set requirements small.

- **`htn_dispatch_system`** — `AgentGoal::Sleep`. Short-circuits on already-`Sleeping`, just-arrived `Working` on the Sleep tile, or in-flight `Seeking`/`Routing`. Builds ctx with `home_bed`/`home_bed_tile` (despawned/unloaded bed silently degrades to "no live claim"), expands `SleepMethod`, and routes/state-transitions accordingly.
- **`htn_eat_dispatch_system`** — `AgentGoal::Survive` with food on hand. Skips when `ActivePlan` is set or `current != Idle`; cheap-rejects when `edible_count == 0` or `hunger < EAT_TRIGGER_HUNGER`. On `Task::Eat` primes the legacy channel (`state = Working`, `task_id = Eat`, `work_progress = 0`) so `eat_task_system` starts accumulating immediately.
- **`htn_acquire_food_dispatch_system`** — `AgentGoal::Survive` with no food on hand, hungry. SOLO agents skipped (no faction storage). Scans `SpatialIndex` within `VIEW_RADIUS=15` for visible loose edible `GroundItem`s (excluding faction storage tiles); populates `nearest_storage_tile` + `faction_food_stock` for the withdraw method, and reads `AgentMemory::best_for(MemoryKind::AnyEdible)` paired with a `PlantMap` lookup for `gather_target_tile` (only set when the tile holds a live, mature plant) so the Forage method can fire. Argmax picks the best of withdraw / scavenge / forage / explore. Routing failure abandons the chain; the next tick re-evaluates.
- **`htn_acquire_good_dispatch_system`** — `AgentGoal::Haul` (with `JobClaim::Haul` + `ClaimTarget`) and `AgentGoal::GatherWood` / `GatherStone`. Haul branch reads `target.resource_id` + `target.blueprint`, scans `StorageTileMap.by_faction` for a tile with effective stock (after `StorageReservations`) of the target good, and snapshots `claimed_blueprint_tile` for full-trip distance discount. Gather branch maps the goal to a `(Good, MemoryKind)` pair (`Wood/Stone`), reads `AgentMemory::best_for(memory_kind)` for `gather_target_tile`, and scans `SpatialIndex` for visible loose `GroundItem`s of the same good for `scavenge_target_*`. When both memory and visibility are populated, Scavenge (1.5) beats Gather (1.0) by argmax; with neither, Explore (0.3) wins as fallback.
- **`htn_stockpile_food_dispatch_system`** — `AgentGoal::GatherFood` with a live `JobClaim::Stockpile{food}`. SOLO agents skipped. Scans `SpatialIndex` for the nearest visible loose edible `GroundItem` (excluding faction storage) via `gi.item.resource_id.is_edible()`, recording `(entity, tile, resource_id)` and threading `resource_id` straight into `Task::DepositToFactionStorage`. Also reads `AgentMemory::best_for(MemoryKind::AnyEdible)` + `PlantMap` for `gather_target_tile` and derives `forage_food_good: Option<ResourceId>` directly from `plant.kind.harvest_yield(false).0` so the Forage-for-storage method's chain carries the right deposit resource. Gating on `JobClaim::Stockpile` (not just `AgentGoal::GatherFood`) ensures storage-fill walks only fire under explicit chief postings.

### Chain handoffs

The trailing legs of multi-task chains live in exit helpers on the executor that ran the head:

- **`production::finish_withdraw_food`** — after `WithdrawFood`. Promotes the queued `Task::Eat` and primes the legacy `task_id = Eat` channel so `eat_task_system` picks up next tick.
- **`production::finish_withdraw_material`** — after `WithdrawMaterial`. Releases the storage reservation; if the new current is `HaulToBlueprint { blueprint }`, looks up `Blueprint.tile` via `bp_query` and routes via `assign_task_with_routing(... TaskKind::HaulMaterials, Some(blueprint) ...)`. The haul leg piggybacks on `construction_system`'s existing hauler branch (`is_hauler = task == HaulMaterials`), which deposits-on-arrival and credits the `JobClaim::Haul` via `record_progress_filtered`.
- **`gather::finish_gather`** — after `Gather`. Every exit path (5 sites — despawned plant, immature plant, completed stone tile, invalid target, hands at haul cap) calls it. When the prefetch ring promotes `DepositToFactionStorage`, looks up nearest faction storage via `StorageTileMap::nearest_for_faction` and routes via `TaskKind::DepositResource`; when it promotes `Eat` (the AcquireFood Forage chain), primes `task_id = Eat` directly so `eat_task_system` picks up next tick (mirrors `production::finish_withdraw_food`). On deposit-routing failure (no storage / unreachable / SOLO) the chain is dropped via `aq.cancel()`; the agent stays Idle with full hands and re-evaluates next tick.
- **`items::finish_scavenge`** — after `Scavenge`. Mirrors `finish_gather` for the AcquireGood and StockpileFood scavenge cases; for the AcquireFood scavenge case, primes `task_id = Eat` instead of routing to storage.

`drop_items_at_destination_system` / `faction_dump_at_storage_system` is the executor for `TaskKind::DepositResource` — it dumps everything in hands at `dest_tile`, credits any `JobClaim::Stockpile` companion, and `aq.advance()`s so the typed channel returns to Idle.

### Stale-reset / catch-all (`goal_dispatch_system`)

`goal_dispatch_system` runs before the HTN dispatchers and now covers only:

- **No-plan stale reset** — when an agent has no `ActivePlan` but a stale legacy `task_id`, flip back to `(Idle, UNEMPLOYED)`. The match preserves in-flight HTN-driven tasks across plan boundaries: `Survive + (Eat | WithdrawFood | Scavenge)`, `Haul + (WithdrawMaterial | HaulMaterials)`, `GatherWood/GatherStone + (Gather | DepositResource | Explore)`, `GatherFood + (Scavenge | DepositResource | Explore)`. The catch-all at the bottom resets `Explore` arrivals to UNEMPLOYED so the next HTN dispatch tick re-evaluates with the populated ctx.

`goal_dispatch_system` no longer matches `AgentGoal::Sleep` — that path is HTN-owned. Its query is `&mut PersonAI`, `&mut ActionQueue`, `&AgentGoal`, `&LodLevel`, `Option<&ActivePlan>`.

### Adding a method

New `AbstractTask` variant (if needed) + new `Method` impl + register call in `register_builtin_methods`. Method tests live in `htn::tests` and exercise the trait directly without an `App`. Adding a new abstract-task *kind* also requires a per-goal dispatch system (or new branch in an existing one) that builds the right `PlannerCtx` shape and routes the expansion's head into the legacy `task_id` channel.

## Faction systems

- **Factions (`faction.rs`):** `FactionTechs` bitset is a derived projection of the chief's `PersonKnowledge.aware`; rebuilt every Economy tick by `sync_faction_techs_from_chief_system`. `FactionCenter`, bonding, storage rollup, raids. `SOLO=0` = ungrouped. `StorageTileMap` indexes storage tiles per faction. `FactionStorage::totals` refreshed every Economy tick.
- **Construction (`construction.rs`):** `BlueprintMap`, `WallMap`, `BedMap`. `faction_blueprint_system` decides what to build; `construction_system` consumes resources and finalizes tiles/entities. `generate_candidates` paces hearths by era (Paleo/Meso `(members+5)/6` gated on crescent saturation + bed deficit; Neolithic `(members+7)/8` gated on each hearth having ≥8 beds in 2..6 crescent ring; Chalcolithic+ single civic hearth). Defensive walls (`PalisadeSegment`) are Chalcolithic+ only. `best_wall_material` ladder: Palisade < WattleDaub (`PERM_SETTLEMENT`) < Mudbrick (`FIRED_POTTERY`) < Stone (`COPPER_WORKING`).
- **CraftOrders + jobs:** `CraftOrder` carries `spawn_tick`; `faction_craft_order_system` despawns orders > `CRAFT_ORDER_TIMEOUT_TICKS=600`. `chief_job_posting_system` (`jobs.rs`) emits one `JobKind::Craft` per faction, picking the recipe with largest `demand-supply` (subject to tech + station availability — Workbench within 12 tiles of `home_tile`, Loom for loom recipes; Loom recipes pass `bench: None` because `job_claim_release_system` only validates Workbench). `resource_demand_system` (`faction.rs`) populates demand for crafted outputs as a fraction of `member_count`.
- **WithdrawMaterial intent:** Storage tiles hold many goods, so the *resolver* picks. All `TaskKind::WithdrawMaterial` steps share `StepTarget::WithdrawForFactionNeed { need, selector }` (`need`: Blueprint/CraftOrder/HaulClaim; `selector`: MostDeficient/Specific). One resolver, parameterised. Per-good deliver-plans removed; `DeliverFromStorageToCraftOrder` covers everything.
- **StorageReservations resource:** `(tile, ResourceId) → reserved_qty`, mutex-wrapped (parallel resolver). Each successful `WithdrawMaterial` dispatch increments and stashes `(reserved_tile, reserved_resource, reserved_qty)` on `PersonAI`; `release_reservation` decrements on every teardown path. Resolver subtracts reservations from raw `GroundItem.qty` so two agents can't commit to the same one-unit stack. Legacy `Good`-typed call sites convert at the boundary via `good.into()`.
- **Activity log (`ui/activity_log.rs`):** Bottom-right egui panel; `ActivityLogEvent { tick, actor, faction_id, kind }` with kinds `Constructed`, `Crafted`, `TechDiscovered`, `RegionSettled`, `Taught`, `Read`. Filtered to player faction; capped at 16 entries.

## Knowledge & technology

- **Per-person knowledge (`knowledge.rs`):** `PersonKnowledge` carries two parallel `u64` bitsets — `aware` (heard of, free, gossiped during Socialize) and `learned` (mastered, costs `complexity()` points, subset of `aware`) plus `learned_at: [u32; 64]` for LRU eviction. `complexity(tech)` is era-based (Paleolithic 1 → Bronze Age 5; Cuneiform/Lunar/City-State bumped to 6). Capacity = `intelligence × 2` (`stats::knowledge_capacity`). Adding a Learned tech beyond capacity demotes the LRU entry back to Aware-only — no per-tick decay. Founders spawn with all-Paleolithic Aware+Learned via `PersonKnowledge::paleolithic_seed`; newborns inherit the OR of both parents' Aware ∪ Learned (free awareness only — no inherited mastery). Progressive study lives on the same component: `study_progress: AHashMap<TechId, u32>`. `add_study_progress(tech, amount, capacity, now)` grants awareness on the first tick, accumulates ticks, and on `study_threshold(tech) = complexity * STUDY_TICKS_PER_COMPLEXITY (3600)` runs `try_learn` (LRU eviction may apply) then clears the entry.
- **Discovery (per-action):** `discovery_system` consumes `DiscoveryActionEvent`s emitted by `gather_system`, `production_system`, `combat_system`, `social_fill_system`. Eligible techs are those whose prerequisites the actor has personally Learned ("next-level adjacent"). Roll `base × (1 + INT_mod × 0.1) × (1 + skill_xp / 1000)`, capped at 0.5; on success the tech lands directly in Learned.
- **Awareness gossip:** `plan_gossip_system` extension — adjacent socializing agents OR each other's `aware` bitsets every Economy tick. Free, fast.
- **Passive teaching:** `tech_teaching_system` (Economy, after gossip) scans pairs of socializing agents within 3 tiles, finds techs the teacher has Learned that the student is Aware of but not Learned, and rolls a low chance (`0.004 × INT_scale`). On success the highest-complexity teachable tech transfers to the student's Learned set (subject to capacity).
- **Directed teaching (`teaching.rs`):** Three pathways feed `study_progress`. (1) **Read** — `Task::Read{tech}`. `read_task_system` adds `+1 × INT_scale` per tick. Player triggers via inspector "Read <tech>" button on Aware-only rows that have a matching tablet/book in inventory; `apply_player_knowledge_orders_system` consumes `PlayerOrderKind::ReadItem` and inserts `Drafted`. (2) **Lecture** — inspector "Lecture" button on a Learned-tech row writes `LectureRequest(Some((lecturer, tech)))`. `apply_lecture_request_system` (Economy) drafts up to 8 same-faction adults within 6 tiles, drops their `ActivePlan`, and inserts `Lecturing { ends_tick=now+600, tech, anchor }` + `Attending`. `lecture_tick_system` (Sequential, after movement) credits `+2 × INT_scale` per tick to each attending student and releases on threshold/timeout/distance. (3) **1-on-1 teach** — right-click a friendly person → "Teach". `PlayerOrderKind::Teach(student)` routes the teacher adjacent; `apply_teach_order_system` then inserts `TeachingPair`/`BeingTaught` with `ends_tick=now+120`. `teach_task_system` credits `+3 × INT_scale` per adjacent tick and emits `ActivityEntryKind::Taught` on success. All three pathways carry `Drafted` while busy.
- **Faction-tech derivation:** `FactionData.techs` is a *cache* — `sync_faction_techs_from_chief_system` (Economy) projects the chief's `aware` bitset onto it every tick. Most read sites ask "can the faction direct this?" → chief-aware suffices. Per-person execution gates use `has_learned` instead:
  - Plan candidate filter (`plan/mod.rs:plan_execution_system`).
  - `job_claim_system` (`jobs.rs`) gating `JobKind::Craft` claims on `recipe.tech_gate`.
  - `faction_hunter_assignment_system` only promotes candidates with Learned `HUNTING_SPEAR`.
  - `mount_check_system` requires per-person Learned `HORSEBACK_RIDING`.
- **Tablets and books (`Item.tech_payload: Option<TechId>`):** `Good::ClayTablet` (`OneHand`, 1500 g) and `Good::Book` (`Small`, 600 g). Crafted via recipes in `CRAFT_RECIPES` gated on `CUNEIFORM_WRITING`, Workbench station. `CraftOrder.tech_payload` and `JobProgress::Crafting.tech_payload` thread the chosen TechId through to `craft_order_system`'s completion stamp where it lands on the produced `Item`. Reading does not consume them. Item equality partitions tablets-of-tech-A from tablets-of-tech-B in storage automatically.
- **Tablet posting (`chief_tablet_posting_system`, `jobs.rs`):** Runs every `CHIEF_TABLET_POSTING_INTERVAL=3600` ticks per faction. Reads chief's `learned`, tallies adult-awareness across all faction members, picks the highest-complexity bit the chief Learned that <50% of adults are Aware of (skipping techs already covered by a live tablet posting), and posts a `JobKind::Craft` with the tablet recipe and `tech_payload=Some(chosen)`. Player override: `PlayerCraftRequest` resource — written by inspector "Encode" button or `PlayerOrderKind::EncodeTablet`; consumed every tick (regardless of cadence) and posted with `JobSource::Player`/priority 180.
- **Tech tree (`technology.rs`):** 43 techs, 5 eras, prereq DAG, per-tech `triggers: &[TechTrigger]` mapping `ActivityKind → per_unit_chance`. `TechBonus` adds yield/storage/combat bonuses; bonuses still aggregate via `faction.techs` (chief-aware).

## Hunting pipeline (`corpse.rs` + plans HUNT_FOOD/SCOUT_FOR_PREY/ACQUIRE_HUNTING_SPEAR)

- Wolf/Deer no longer drop Meat/Skin on death — `combat.rs::death_system` strips AI/needs/species and inserts `Corpse { species, fresh_until_tick }`, indexed in `CorpseMap`. `corpse_decay_system` (Economy) despawns at `CORPSE_FRESHNESS_TICKS=600`.
- **Chief hunt orders (`HuntOrder` on `FactionData`):** `chief_hunt_order_system` posts daily (`TICKS_PER_DAY=3600`, staggered by `fid`). Scans `SpatialIndex` within `HUNT_SCAN_RADIUS=40` for prey, posts `Hunt { species, area_tile, target_party_size = 4(Wolf)/2(Deer), mustered, deployed_tick }` or `Scout`. Mid-day invalidation sweep clears stale orders.
- **`HuntFood`** (hunter-only, `HUNTING_SPEAR` gated, `PF_UNINTERRUPTIBLE`, gated on `Hunt`): muster at hearth → travel → hunt → PickUpCorpse → HaulCorpse → Butcher (60 ticks → drops `species_yield()` Meat+Skin, despawns corpse).
- **`ScoutForPrey`** (gated on `Scout`): single explore step writes `MemoryKind::Prey`; chief flips to Hunt next decision. Not `PF_UNINTERRUPTIBLE`.
- **`AcquireHuntingSpear`:** WithdrawGood(Weapon) → Equip(MainHand). `StepPreconditions::forbids_good` checks `Equipment::has_good` so it self-deselects after equip.
- `corpse_follow_system` (Sequential, after `movement_system`) snaps corpse Transform to carrier. `respond_to_distress_system` recruits any same-faction Hunter within `HUNTER_RESPOND_RANGE=50` regardless of LOS, removing the `Carrying` component to drop the corpse.
- **`Carrying(Entity)` component (in `corpse.rs`):** Marker for "this person is carrying corpse E." Inserted at pickup arrival, removed at butcher / decay / rescue / muster / hunter-demote. `corpse_follow_system` keys on `(&Carrying, &Transform)`.

## Game lifecycle and regions

- **`GameState`:** `SpawnSelect` (default) | `Playing`. Globe gen at `WorldPlugin::build`. Spawn-select UI in `SpawnSelect`; spawn systems on `OnEnter(Playing)`. Update-stage systems gated `in_state(Playing)`; FixedUpdate sim systems are not gated (queries empty until entities spawn).
- **`PendingSpawn(Option<(i32, i32)>)`:** Player's chosen mega-chunk; `None` = globe centre (sandbox).
- **Sandbox bypass:** `SandboxPlugin` flips state to `Playing` at Startup, skipping spawn-select.
- **`SettledRegions` (`region.rs`):** `AHashMap<RegionId, SettledRegion>` + `by_megachunk` reverse index. `SettledRegion { megachunk, founding_tick, name, camera_bookmark, player_owned }`. `settle()` idempotent.
- **`MegaChunkCoord`:** `from_chunk/from_tile/center_tile/chunk_range`. `MEGACHUNK_SIZE_CHUNKS=16`, independent of climate cells.
- **Multi-focus chunk streaming (`SimulationFocus`):** `Vec<FocusPoint>` rebuilt each tick from camera + every settled region centre. Chunk DATA loads for any focus disc (camera `LOAD_RADIUS=12`, region `REGION_LOAD_RADIUS=6`); SPRITES + plants + loose rocks only inside the camera focus.
- **Focus-aware LOD (`update_lod_levels_system`):** Camera distance produces base LOD; entities within 8 chunks of any non-camera focus are promoted from Dormant to Aggregate so off-screen agents keep ticking.
- **Edge-walk expansion (`region::detect_edge_crossing_system`, Economy):** Each tick checks every player-faction Person's mega-chunk; if unsettled, calls `settle()`, names "Outpost N", bookmarks position, emits `ActivityLogEvent::RegionSettled`.
