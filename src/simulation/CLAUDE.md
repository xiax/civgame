# Simulation (`src/simulation/`)

Agent AI, factions, knowledge, hunting, typed-task pipeline, pluralist economy. See root `CLAUDE.md` for cross-cutting `SimulationSet` ordering.

## Agent AI (Goals → HTN → Tasks)

The legacy plan registry is gone. AI dispatch runs end-to-end through HTN.

- **Goals (`goals.rs`):** high-level objectives (`Survive`, `GatherWood`, `GatherStone`, `GatherFood`, `Haul`, `Defend`, `Farm`, `Build`, `Socialize`, `Sleep`, …) driven by Needs + Faction state.
- **HTN registry (`htn.rs`):** per-goal decomposition. Methods carry `precondition` / `utility` / `expand(ctx) -> Vec<Task>`; the dispatcher argmaxes over applicable methods and routes the head into the typed-task channel, with the tail prefetched on `ActionQueue`'s queue ring.
- **Tasks (`tasks.rs`):** the agent's *current* action — `TaskKind` enum (Gather, Construct, WithdrawMaterial, Hunt, Butcher, Equip, …). Transient; `Idle` between tasks.
- **Professions (`person.rs::Profession`):** `None | Farmer | Hunter | Bureaucrat | Trader`. Persistent role. Farmer auto-assigned by `faction_profession_system` when food < 100. Hunter is chief-driven via `faction_hunter_assignment_system` (Economy, every `TICKS_PER_DAY/4`): target headcount = `max(1, adults*0.20)` × martial × prey-density, capped at adults/2; demotes do full teardown. Bureaucrat/Trader cover settlement administration and arbitrage (see Pluralist Economy below).
- **Skills (`skills.rs`):** `[u8; 8]` — Farming, Mining, Building, Trading, Combat, Crafting, Social, Medicine. Default 5; `gain_xp()` saturating.
- **Bucketing:** agents sliced across 20 fixed `BucketSlot`s.
- **LOD:** `Detail / Aggregate / Dormant`. Dormant skip sim. Focus-aware (see Game lifecycle).
- **Memory & gossip (`memory.rs`):** known locations + agent sightings, `u8` freshness decay. Tech awareness gossips between socializing agents via `knowledge::awareness_gossip_system`. `MemoryKind` is a 3-variant `Copy` enum: `AnyEdible`, `Resource(ResourceId)`, `Prey`. Helper constructors `MemoryKind::wood() / stone() / grain_seed() / berry_seed()`.
- **Needs:** 6 needs (hunger, sleep, shelter, safety, social, reproduction), `[0,255]`, decay over time. Maslow tiers (`Physiological → Safety → Belonging → Esteem → SelfActualization`) layer on top via `MaslowTier::next_unmet(needs)`.

## Method-design rules

- **Method scoring is *viability* + *distance*.** Methods supply a base utility tier (`UTIL_BASELINE=1.0` / `UTIL_VISIBLE_GROUND=1.5` / `UTIL_CLAIMED_HAUL=2.0` / `UTIL_EXPLORE_FALLBACK=0.3`); the dispatcher subtracts a chebyshev-distance penalty capped at `MAX_DIST_PENALTY=0.30` so inter-tier ranking is preserved. Recent failures bias the next argmax via `MethodHistory` + `score_method_with_history` (`METHOD_FAILURE_PENALTY=0.5`, `METHOD_HISTORY_TTL_TICKS=100`).
- **Farming is `Farm`-goal only.** Both halves are HTN-driven: `PlantFromStorage` plants; `HarvestPlant` harvests. Seed↔plant mapping centralised in `PlantKind::seed_resource()` + `PlantKind::ALL` (`plants.rs`). Adding a seed/plant pair = one `PlantKind::ALL` entry + arm in `seed_resource()`.
- **Faction storage refresh:** `FactionStorage.totals` rebuilt every Economy tick by `compute_faction_storage_system`. HTN dispatchers read these directly when scoring.
- **`MF_UNINTERRUPTIBLE`:** methods set this when their multi-leg chain must survive across goal-dispatch ticks.

## ActionQueue and typed Task variants (`typed_task.rs`)

`aq.current: Task` is canonical "task running now"; `Task::Idle` default; every Person spawn site bundles `ActionQueue::idle()`. Behind `current` sits a fixed-capacity `queued: [Task; 4]` prefetch ring (private; access via `enqueue` / `pop_next` / `peek_next` / `queued_len` / `queued_is_empty` / `clear_queued`).

- **Producers** (HTN dispatchers, `ui/orders.rs`, `teaching.rs::ReadItem`) route through `aq.dispatch(task)` — enqueues then promotes head into `current` if `current == Idle`.
- **Consumers** (executor exit paths in `gather.rs`, `dig.rs`, `corpse.rs`, `construction.rs`, `items.rs`, `production.rs`, `teaching.rs`, plus `MilitaryMove` arrival in `military.rs`) call `aq.advance()` instead of writing `current = Idle`.
- **External preempts** (`apply_muster_hunters_system`, hunter demote, `goal_dispatch_system` stale reset) call `aq.cancel()`, dropping both `current` and the queue.
- Per-tick "pin" sites (lecture/teach pin writes in `teaching.rs`) stay as direct `aq.current = X` writes — idempotent re-assertions, not fresh dispatches.

**Variants:** `Idle`, `WalkTo { tile, z, why }`, `WithdrawGood { filter }`, `WithdrawMaterial { resource_id, qty }`, `WithdrawFood { tile }`, `Equip { slot, resource_id }`, `Construct { blueprint }`, `Gather { tile }`, `Dig { tile }`, `Scavenge { target }`, `Read/Teach/HoldLecture/AttendLecture { tech }`, `PickUpCorpse { corpse }`, `HuntPartyMuster { hearth }`, `Hunt { prey }`, `HaulCorpse { dest }`, `Butcher`, `TameAnimal { target }`, `Sleep { bed }`, `Eat`, `HaulToBlueprint { blueprint }`, `HaulToCraftOrder { order }`, `WorkOnCraftOrder { order }`, `DepositToFactionStorage { resource_id }`, `PlayThrow`, `PlayPlant { tile }`, `Explore { kind }`, plus `Lead/Defend/Raid/RescueAlly/Socialize/Play` (faction/social).

**Rules:**

- Systems mutating typed task **must** include `&mut ActionQueue` alongside `&mut PersonAI`.
- Every executor exit (success/abort/timeout/precondition fail) must `aq.advance()` (or `aq.cancel()` for chain-drop).
- External teardowns clear both `aq` and the legacy `task_id`.
- `military_task_system`, `withdraw_good_task_system`, `withdraw_material_task_system` fall back to `Idle` if `task_id == X` but `aq.current` is the wrong variant — defence in depth.

## HTN domain (`htn.rs`)

- **`AbstractTask` / `AbstractTaskKind`** — high-level goal a method decomposes (`Sleep`, `Eat`, `AcquireFood`, `AcquireGood { resource_id }`, `StockpileFood`, `Scout`, `EquipHuntingSpear`, `ReturnSurplus`, `TameWildHorse`, `PlantFromStorage { resource_id }`, `HarvestPlant`, `ConstructBlueprint`, `Socialize`, `DeliverMaterialToCraftOrder { resource_id }`, `WorkOnCraftOrder`, `HarvestGrainForCraftOrder`, `Play`, `JoinHuntParty`, `EngagePrey`, `DeliverHuntKill`, plus four single-step combat/faction goals).
- **`Method` trait** — `precondition` / `utility` / `expand(ctx) -> Vec<Task>` / `flags` / `tech_gate` / `profession_gate` / `policy_gate` / `name`. `policy_gate` returns `&'static [(ResourceId, RequiredFlag)]` for methods that require a specific `ResourceControlPolicy` flag (default `&[]`).
- **`MethodRegistry`** — `Resource`, `AHashMap<AbstractTaskKind, Vec<Box<dyn Method>>>`. Built once in `SimulationPlugin::build` via `register_builtin_methods`. Read-only after init.
- **`MethodFlags`** — plain `u8` bitmask. `MF_UNINTERRUPTIBLE` survives goal flips.
- **`MethodId`** — `u16` newtype with one `pub const` per registered method. Stable identity for `MethodHistory` keying and `PersonAI.active_method`.
- **`MethodHistory`** — per-agent ring (`METHOD_HISTORY_LEN=2`, TTL 100 ticks) of `(MethodId, MethodOutcome, tick)`. Spawned at every Person spawn site.
- **Failure-biased scoring:** multi-method dispatchers score via `score_method_with_history(method, abstract_task, ctx, history, now) = utility - failures * METHOD_FAILURE_PENALTY`. Routing/missing-target failures push `FailedRouting` / `FailedTarget` so the next decision biases away.
- **Success recording:** dispatchers stamp `PersonAI.active_method = Some(method.id())` on successful `aq.dispatch(...)` and clear before pushing `Failed*`. `htn_method_completion_system` (Economy) observes `aq.current == Idle && queued_is_empty && active_method.is_some()` and records `Success`.

### Methods (highlights)

| Abstract task | Method | Utility | Expansion |
|---|---|---|---|
| `Sleep` | `SleepMethod` | n/a | `[Sleep { bed }]` (own → home → in-place) |
| `Eat` | `EatFromInventoryMethod` | 1.0 | `[Eat]` (gates on `edible_count > 0 && hunger ≥ EAT_TRIGGER_HUNGER`) |
| `AcquireFood` | Withdraw / Scavenge / Forage / Explore | 1.0 / 1.5 / 1.0 / 0.3 | chains end in `Eat` |
| `AcquireGood` | Withdraw / Haul / Gather / Scavenge / Explore | 1.0 / 2.0 / 1.0 / 1.5 / 0.3 | chains end in `DepositToFactionStorage` (or `HaulToBlueprint` for the Haul variant) |
| `StockpileFood` | Scavenge / Forage / Explore | 1.5 / 1.0 / 0.3 | end in `DepositToFactionStorage` |
| `Scout` | `ScoutForPreyMethod` | 1.0 | `[Explore { kind: Prey }]` |
| `EquipHuntingSpear` | `WithdrawAndEquipHuntingSpearMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial { weapon, 1 }, Equip { MainHand, weapon }]` |
| `ReturnSurplus` | `DepositSurplusAtStorageMethod` | 1.0 | `[DepositToFactionStorage { food_id }]` |
| `TameWildHorse` | `TameWildHorseMethod` | 1.0 | `[TameAnimal { target }]` |
| `PlantFromStorage` | `WithdrawAndPlantSeedMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial { seed, 1 }, Planter { tile }]` |
| `HarvestPlant` | `HarvestMaturePlantForStorageMethod` | 1.0 | `[Gather { tile }, DepositToFactionStorage { resource_id }]` |
| `ConstructBlueprint` | Build / Withdraw+Haul / Gather+Haul | 1.0 / 2.0 / 1.0 (all `MF_UNINTERRUPTIBLE`) | claimed-build path + personal-blueprint paths |
| `JoinHuntParty` | Muster / Travel | 1.0 (`MF_UNINTERRUPTIBLE`) | `HuntPartyMuster` or `Explore { Prey }` toward area_tile |
| `EngagePrey` | Hunt / PickUpFreshCorpse | 1.0 / 1.5 (`MF_UNINTERRUPTIBLE`) | `Hunt { prey }` or `PickUpCorpse { corpse }` |
| `DeliverHuntKill` | `DeliverHuntKillMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[HaulCorpse { dest }, Butcher]` |
| `DeliverMaterialToCraftOrder` | `WithdrawAndHaulToCraftOrderMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial, HaulToCraftOrder]` |
| `WorkOnCraftOrder` | `WorkOnSatisfiedCraftOrderMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WorkOnCraftOrder, DepositToFactionStorage { output }]` |
| `HarvestGrainForCraftOrder` | `HarvestAndHaulGrainToCraftOrderMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[Gather, HaulToCraftOrder]` |
| `Socialize` | `SocializeWithPartnerMethod` | 1.0 | `[Socialize { partner }]` |
| `Play` | 6 methods (partner / solo / throw stones / toy / plant grain / plant berry) | 1.5 / 1.0 / 1.0 / 1.0 / 1.0 / 1.0 | varies; storage-fed branches `MF_UNINTERRUPTIBLE` |
| `Raid` / `Defend` / `Lead` / `RescueAlly` | one method each | 1.0 | single `[Task::X { dest }]`, dispatched by `htn_combat_faction_dispatch_system` |

**Distance-weighted utility.** Methods whose ctx carries a target tile subtract `chebyshev_dist(ctx.tile, target) * DIST_DISCOUNT_PER_TILE` from base utility, capped at `MAX_DIST_PENALTY (0.30)`. Cap preserves inter-tier ranking. Haul + gather/scavenge methods discount on full-trip distance when both ctx tiles are populated.

### Dispatch systems

All run in `ParallelB`, ordered after `goal_dispatch_system`. Each system serialises on `&mut PersonAI` / `&mut ActionQueue`; Bevy schedules them in parallel where component sets allow. The craft-order + Harvest + Play dispatchers share their own `add_systems` call (Bevy's 20-tuple `IntoSystemConfigs` ceiling). Spear-arming runs *before* food dispatchers so an unarmed hunter prefers fetching their spear over eating.

Each dispatcher: gates on goal + Idle + non-Dormant + `task_id == UNEMPLOYED`, builds `PlannerCtx`, runs argmax across registered methods, routes the head via `assign_task_with_routing`, prefetches the tail on `ActionQueue`. SOLO/unsettled agents skip storage-dependent methods.

### Chain handoffs

Trailing legs of multi-task chains live in exit helpers on the head executor:

- **`production::finish_withdraw_food`** — primes `task_id = Eat`.
- **`production::finish_withdraw_material`** — releases reservation; routes `HaulToBlueprint` / `HaulToCraftOrder` / `Planter`; primes `Equip` / `PlayThrow` / `Play` in-place.
- **`gather::finish_gather`** (5 exit sites: despawned plant, immature plant, completed stone tile, invalid target, hands at haul cap) — routes `DepositToFactionStorage` to nearest storage; routes `HaulToBlueprint` / `HaulToCraftOrder`; primes `Eat` for the Forage chain. Routing failure drops the chain (`aq.cancel()`).
- **`items::finish_scavenge`** — mirror of `finish_gather` for AcquireGood/StockpileFood; primes `Eat` for the AcquireFood path.
- **`drop_items_at_destination_system`** — executor for `TaskKind::DepositResource`; dumps hands at `dest_tile`, credits any `JobClaim::Stockpile`, `aq.advance()`s.

### Stale-reset / catch-all (`goal_dispatch_system`)

Runs before HTN dispatchers. Covers only:

- **Stale-task reset** — leftover `task_id != UNEMPLOYED` clears to `(Idle, UNEMPLOYED)` (and `aq.cancel()`) unless the goal legitimately keeps it (preserve-arms enumerate the (goal, task_id) pairs each HTN dispatcher's chain needs to survive). Catch-all at bottom resets `Explore` arrivals.

### Adding a method

New `AbstractTask` variant (if needed) + `Method` impl + register call in `register_builtin_methods`. Method tests in `htn::tests` exercise the trait directly without an `App`. New abstract-task *kinds* also need a per-goal dispatch system (or new branch).

## Memory & gathering (`shared_knowledge.rs`)

3-tier `SharedKnowledge` map replaces the old per-agent tile ring as the system of record for static-resource queries.

- **`SharedKnowledge` resource** — per-tier `KnowledgeMap`s keyed by `KnowledgeTier::{Household(u32), Settlement(SettlementId), Faction(u32)}`. Resources stored as `ResourceCluster` influence nodes (center + radius + LRU `representative_tiles[4]` + `estimated_count`); `report_sighting` merges nearby same-(kind,owner) sightings within `CLUSTER_MERGE_RADIUS=8`, `report_depleted` decrements and despawns at zero. `nearest_in_tier_set(TierSet, kind, from, owner_filter, claim_penalty)` walks finest-tier-first via spiral chunk search. `cluster_decay_system` drops clusters older than `CLUSTER_DECAY_TTL_TICKS=25200` once per game-day.
- **`ResourceOwner`** — `Public | Person(Entity) | Household(u32) | Settlement(SettlementId) | Faction(u32)`. `is_accessible_to(viewer, household, settlement, faction)` gates harvest without theft semantics.
- **`LandClaim` component** — stamped on `Plant` entities at planting (`production.rs` Planter arm): `HouseholdMember` planter → `Household(id)`; chief Farm posting → `Faction(id)`; otherwise → `Person(self)`. Wild reseed leaves no `LandClaim` → `Public`.
- **`GatherClaims`** (`gather_claims.rs`) — mutex-wrapped per-`(tile, kind)` reservation map. `pressure(tile, now, viewer)` feeds `nearest_in_tier_set`'s `claim_penalty` so two agents fan out across separate clusters. `gather_claim_expiry_system` (Economy, every 150 ticks) sweeps expired entries.
- **Vision write-through (`memory.rs::vision_system`)** — every active agent reports plant/item/prey/stone sightings to `SharedKnowledge`'s **finest tier** (Household if a member, else Faction). Settlement and Faction tiers materialise via gossip propagation. **Vision is additive only** — `report_depleted` is never called from vision. Cluster shrinkage happens at gather-arrival via `gather_system::report_depleted` on every `finish_gather` exit. `report_depleted` is a no-op when the tile was never a sighted rep of the targeted cluster.
- **Tier gossip propagation (`knowledge.rs::cluster_tier_promotion_system`)** — Economy, every 200 ticks. When a `HouseholdMember` socialises within 3 tiles of a `Profession::Bureaucrat` or `FactionChief` of the same root faction, household-tier clusters promote to the official's `Settlement` tier. Two same-faction officials → settlement-tier clusters bubble to `Faction` tier. A faction only "knows" what its members have told an official; chief-less / bureaucrat-less factions get coarser maps.
- **Cluster-density-gated chief postings** (`jobs.rs::chief_job_posting_system`): `faction_knows_cluster(shared, settlement_map, faction_id, kind, from, max_chunk_radius)` walks the faction's `Faction(fid)` tier plus every owned `Settlement(sid)` tier. No known cluster → skip the food / Wood / Stone posting. CraftOrder-driven Stockpile postings (Skin / Tools / Cloth) and Build postings remain ungated.

`AgentMemory` is now ~30 lines: `visited_settlements: [Option<(SettlementId, u8)>; 8]` + `record_settlement` / `known_settlements`. `relationship_decay_system` walks `RelationshipMemory` only — `cluster_decay_system` owns cluster freshness.

## Behavioural test fixture (`test_fixture.rs`)

Headless `App` harness for AI assertions without rendering/UI/globe gen. `TestSim::new(seed)`, `flat_world(radius, z, kind)`, `spawn_person(faction, tile, |b| b.hunger(...).add_inventory(...))`, then `tick()` / `tick_n(n)`.

- Time deterministic via `TimeUpdateStrategy::ManualDuration` — every `app.update()` advances `Time` by exactly one fixed-tick (1/20 s).
- State stays in `SpawnSelect` so `OnEnter(Playing)` systems never fire; FixedUpdate sim runs regardless.
- Camera spawned at origin so `update_lod_levels_system` doesn't drop test agents to `Dormant`.
- `test_fixture::person_task(&app, entity) -> Task` reads `ActionQueue.current`.
- Currency-invariant helpers: `set_currency` / `get_currency` / `assert_currency`; `total_system_currency` + `CurrencySnapshot::capture` + `assert_total_currency_invariant(app, baseline, eps)`. Snapshot sums `EconomicAgent.currency` + `FactionData.treasury` + `Settlement.treasury` + `JobEscrow.amount`.
- `test_fixture::inject_faction_sighting` pre-populates a faction-tier cluster. Inject **outside** `VIEW_RADIUS=15` of any test agent or vision will deplete the singleton cluster.

## Faction systems

- **Factions (`faction.rs`):** `FactionTechs` is a derived projection of the chief's `PersonKnowledge.aware`; rebuilt every Economy tick by `sync_faction_techs_from_chief_system`. `FactionCenter`, bonding, storage rollup, raids. `SOLO=0`. `StorageTileMap` indexes storage tiles per faction. `FactionStorage::totals` refreshed every Economy tick.
- **Construction (`construction.rs`):** `BlueprintMap`, `WallMap`, `BedMap`. `faction_blueprint_system` decides what to build; `construction_system` consumes resources and finalizes tiles/entities. `generate_candidates` paces hearths by era (Paleo/Meso `(members+5)/6` gated on crescent saturation + bed deficit; Neolithic `(members+7)/8` gated on each hearth having ≥8 beds in 2..6 crescent ring; Chalcolithic+ single civic hearth). Defensive walls (`PalisadeSegment`) Chalcolithic+ only. `best_wall_material` ladder: Palisade < WattleDaub (`PERM_SETTLEMENT`) < Mudbrick (`FIRED_POTTERY`) < Stone (`COPPER_WORKING`).
- **CraftOrders + jobs:** `CraftOrder` carries `spawn_tick`; `faction_craft_order_system` despawns orders > `CRAFT_ORDER_TIMEOUT_TICKS=600`. `chief_job_posting_system` (`jobs.rs`) emits one `JobKind::Craft` per faction, picking the recipe with largest `demand-supply` (subject to tech + station — Workbench within 12 of `home_tile`, Loom for loom recipes). `resource_demand_system` populates demand for crafted outputs as a fraction of `member_count`.
- **WithdrawMaterial intent:** dispatcher picks the resource. Each HTN dispatcher emitting a `Task::WithdrawMaterial` reads `FactionStorage.totals` + `StorageReservations` to find the nearest tile with effective stock and stamps the resource into the typed task.
- **`StorageReservations`:** `(tile, ResourceId) → reserved_qty`, mutex-wrapped. Every successful `WithdrawMaterial` dispatch increments and stashes `(reserved_tile, reserved_resource, reserved_qty)` on `PersonAI`; `release_reservation` decrements on every teardown path. Resolver subtracts reservations from raw `GroundItem.qty` so two agents can't commit to the same one-unit stack.

## Knowledge & technology

- **Per-person (`knowledge.rs`):** `PersonKnowledge` carries `aware` and `learned` (`u64` bitsets — learned ⊆ aware) plus `learned_at: [u32; 64]` for LRU eviction. `complexity(tech)` is era-based (Paleolithic 1 → Bronze Age 5; Cuneiform/Lunar/City-State 6). Capacity = `intelligence × 2`. Adding a Learned tech beyond capacity demotes the LRU entry to Aware-only — no per-tick decay. Founders spawn with all-Paleolithic Aware+Learned via `paleolithic_seed`; newborns inherit the OR of both parents' Aware ∪ Learned (free awareness only). `study_progress: AHashMap<TechId, u32>` lives on the same component; `add_study_progress(tech, amount, capacity, now)` accumulates ticks and at `study_threshold(tech) = complexity * STUDY_TICKS_PER_COMPLEXITY (3600)` runs `try_learn`.
- **Discovery (per-action):** `discovery_system` consumes `DiscoveryActionEvent`s emitted by `gather_system`, `production_system`, `combat_system`, `social_fill_system`. Eligible techs are those whose prereqs the actor has personally Learned. Roll `base × (1 + INT_mod × 0.1) × (1 + skill_xp / 1000)`, cap 0.5; success lands the tech directly in Learned.
- **Awareness gossip:** `awareness_gossip_system` (Economy) — adjacent socializing agents OR each other's `aware` bitsets every tick. Also OR-merges `AgentMemory.visited_settlements`.
- **Passive teaching:** `tech_teaching_system` (Economy, after gossip) scans pairs within 3 tiles, finds techs the teacher has Learned that the student is Aware of but not Learned, rolls `0.004 × INT_scale`. Highest-complexity teachable tech transfers (subject to capacity).
- **Directed teaching (`teaching.rs`):** Three pathways feed `study_progress`. **Read** — `Task::Read{tech}` adds `+1 × INT_scale` per tick (player triggers via inspector "Read" button on Aware-only rows with matching tablet/book). **Lecture** — inspector "Lecture" button writes `LectureRequest`; `apply_lecture_request_system` drafts up to 8 same-faction adults within 6 tiles and inserts `Lecturing { ends_tick=now+600, tech, anchor }` + `Attending`; `lecture_tick_system` (Sequential, after movement) credits `+2 × INT_scale` per tick. **1-on-1 teach** — right-click → "Teach"; `apply_teach_order_system` inserts `TeachingPair`/`BeingTaught` with `ends_tick=now+120`; `teach_task_system` credits `+3 × INT_scale` per adjacent tick. All carry `Drafted` while busy.
- **Faction-tech derivation:** `FactionData.techs` is a *cache* — `sync_faction_techs_from_chief_system` (Economy) projects the chief's `aware` bitset every tick. Most read sites ask "can the faction direct this?" → chief-aware suffices. Per-person execution gates use `has_learned`: HTN `tech_gate`, `JobKind::Craft` claim gating, `faction_hunter_assignment_system` (`HUNTING_SPEAR`), `mount_check_system` (`HORSEBACK_RIDING`).
- **Tablets and books (`Item.tech_payload: Option<TechId>`):** `Good::ClayTablet` (`OneHand`, 1500 g) and `Good::Book` (`Small`, 600 g). Crafted via recipes gated on `CUNEIFORM_WRITING`, Workbench station. Reading does not consume them.
- **Tablet posting (`chief_tablet_posting_system`):** every `CHIEF_TABLET_POSTING_INTERVAL=3600` ticks per faction. Reads chief's `learned`, picks the highest-complexity bit the chief Learned that <50% of adults are Aware of (skipping techs already covered by a live tablet posting), posts a `JobKind::Craft` with `tech_payload=Some(chosen)`. Player override: `PlayerCraftRequest` (inspector "Encode") — consumed every tick, `JobSource::Player`/priority 180.
- **Tech tree (`technology.rs`):** 43 techs, 5 eras, prereq DAG, per-tech `triggers: &[TechTrigger]`. `TechBonus` adds yield/storage/combat bonuses; bonuses aggregate via `faction.techs` (chief-aware).

## Hunting pipeline (`corpse.rs` + the three-phase HTN pipeline)

- Wolf/Deer no longer drop Meat/Skin on death. `combat.rs::death_system` strips AI/needs/species and inserts `Corpse { species, fresh_until_tick }`, indexed in `CorpseMap`. `corpse_decay_system` (Economy) despawns at `CORPSE_FRESHNESS_TICKS=600`.
- **Chief hunt orders (`HuntOrder` on `FactionData`):** `chief_hunt_order_system` posts daily (`TICKS_PER_DAY=3600`, staggered by `fid`). Scans `SpatialIndex` within `HUNT_SCAN_RADIUS=40`, posts `Hunt { species, area_tile, target_party_size = 4(Wolf)/2(Deer), mustered, deployed_tick }` or `Scout`. Mid-day invalidation sweep clears stale orders.
- **HuntFood pipeline:** three HTN abstract tasks (hunter-only, `HUNTING_SPEAR`-gated, `MF_UNINTERRUPTIBLE`): `JoinHuntParty` (Muster + Travel) under chief `HuntOrder::Hunt`; `EngagePrey` (Hunt + PickUpCorpse) at the area; `DeliverHuntKill` (HaulCorpse + Butcher) gated on `Carrying`. Butcher drops `species_yield()` Meat+Skin, despawns corpse.
- **Scout:** `htn_scout_dispatch_system` + `ScoutForPreyMethod` emits `Task::Explore { kind: Prey }` while chief holds `HuntOrder::Scout`; chief flips to Hunt the moment `vision_system` writes a prey sighting.
- **EquipHuntingSpear:** runs ahead of food dispatchers so unarmed hunters fetch + equip before eating.
- `corpse_follow_system` (Sequential, after `movement_system`) snaps corpse Transform to carrier. `respond_to_distress_system` recruits any same-faction Hunter within `HUNTER_RESPOND_RANGE=50` regardless of LOS, removing `Carrying`.
- **`Carrying(Entity)`** — marker for "carrying corpse E"; inserted at pickup arrival, removed at butcher / decay / rescue / muster / hunter-demote.

## Pluralist Economy

Per-resource policy flags + per-settlement markets + sub-faction households + Bureaucrat/Trader professions + P2P currency + escrow + U_bid scoring + tribute + craft contracts. Currency invariant: `EconomicAgent.currency` + `FactionData.treasury` + `Settlement.treasury` + `JobEscrow.amount` is conserved across every operation.

### Settlements (`settlement.rs`)

The **economic** unit (market + treasury + market_tile), distinct from `SettlementPlan` (the layout of zones around a hearth). One faction can own many settlements; a megachunk can host many competing settlements.

- `Settlement` (Component): `id`, `owner_faction`, `market_tile`, `founding_tick`, `name`, `treasury: f32`, `market: SettlementMarket`.
- `SettlementId(u32)` newtype; `SettlementMap` (Resource) with `by_id` / `by_megachunk` / `by_faction` indices.
- `auto_found_default_settlements_system` (FixedUpdate, before `settlement_planner_system`): spawns one Settlement per non-SOLO faction at its `home_tile` if missing.

### Currency + escrow

- **`pay(world, from, to, amount) -> bool`** (`economy/transactions.rs`): atomic agent-to-agent transfer. Only sanctioned way to move currency between agents.
- **`FactionData.treasury: f32`** — faction-level wealth pool, distinct from per-settlement treasuries.
- **`JobEscrow { amount, beneficiary }`** Bevy component on a sidecar entity per funded posting. Lifecycle: producer debits wallet + spawns sidecar; on payout, producer calls `pay()`, zeros `escrow.amount`, despawns; on cancellation/expiry, sidecar despawns with amount intact and the `on_job_escrow_remove` hook refunds `beneficiary`. **All existing `aq.cancel()` sites stay untouched** — refund piggybacks on entity despawn (mirrors `Indexed::on_remove`).
- **Hook registration:** `JobsPlugin::build` calls `register_component_hooks::<JobEscrow>().on_remove(on_job_escrow_remove)`. `TestSim::new` inherits via `add_plugins(SimulationPlugin)`.

### Per-resource policy (`economy/policy.rs`)

`ResourceControlPolicy` flags: `chief_allocates_labor`, `private_actors_allowed`, `state_sells_at_market`, `prices_fixed_by_state`, `fixed_price`. `Default` = all-communist (matches pre-pluralist behaviour); `capitalist()` preset.

`FactionData.economic_policy: AHashMap<ResourceId, ResourceControlPolicy>` — empty by default. `Method::policy_gate` returns `&'static [(ResourceId, RequiredFlag)]`; `method_passes_policy_gate(method, faction)` returns true iff every gate entry is satisfied. SOLO agents reject any non-empty gate.

### Sub-factions / households

A household is a `FactionData` with `parent_faction = Some(village_id)`. Reuses the entire faction primitive — storage, treasury, chief-equivalent (`household_head`), member tracking.

- **`FactionData.parent_faction: Option<u32>`** — `None` = top-level village, `Some(id)` = sub-faction.
- **`FactionData.household_head: Option<Entity>`** — analogue to `chief_entity`.
- **`FactionData.children_factions: Vec<u32>`** — reverse pointer; villages list every household nested under them.
- **`FactionRegistry::spawn_household(parent, home_tile, head, &catalog)`** — creates the sub-faction, wires parent/child links, sets `household_head`, stamps `ResourceControlPolicy::capitalist()` on every catalog resource. Caller still moves member `FactionMember.faction_id`s, calls `add_member`, spawns a `FactionStorageTile`.
- **`FactionRegistry::root_faction(id)`** — walks `parent_faction` to the village.
- **Formation:** `CoSleepTracker.bond_strength: u16` accumulates ticks of cosleep with the *same* partner (resets on switch). At `HOUSEHOLD_BOND_THRESHOLD` (one game-week) `household_formation_system` (Economy) spawns the household. Both members get `HouseholdMember { household_id }`.
- **Inheritance:** `pregnancy_system` threads the mother's `Option<&HouseholdMember>` to the newborn. Households grow generationally.
- **`HouseholdMember` is a marker only — not a faction migration.** Parents keep `FactionMember.faction_id` pointing at the village; the household exists as a container for private storage / treasury.

### Bureaucrat profession

Officials physically employed by the settlement treasury — demote when treasury can't fund them.

- **`Profession::Bureaucrat`** + `FactionData.state_funds_public_works: bool` (default false) governance flag.
- **`FactionData.bureaucrat_treasury_empty_streak: u32`** — running tally; **decoupled from bureaucrat count** so a demote-to-zero can't immediately re-promote until funds arrive.
- **`chief_bureaucrat_appointment_system`** (Economy, every `BUREAUCRAT_ASSIGNMENT_CADENCE = TICKS_PER_DAY/4`): mirrors `faction_hunter_assignment_system`. Target = `max(1, member_count * BUREAUCRAT_MIN_RATIO)`; ranks by Social skill. When `bureaucrat_treasury_empty_streak >= BUREAUCRAT_QUIT_DAYS * TICKS_PER_DAY`, target=0 → all demote with full teardown.
- **`bureaucrat_salary_tick_system`** (Economy, every `TICKS_PER_DAY/24`): debits the faction's first settlement treasury, credits each bureaucrat by `BUREAUCRAT_DAILY_WAGE/24`. Two-pass for borrow-checker; treasury bottoms at 0.
- **`bureaucrat_admin_dispatch_system`** (`htn.rs`, ParallelB after combat dispatcher): for any Bureaucrat with `task_id == UNEMPLOYED + Idle + non-Dormant`, dispatches `Task::Lead { dest = settlement.market_tile }` via `assign_task_with_routing`. Direct dispatch (no HTN method) — bureaucrat behaviour is deterministic. Need-driven goals preempt naturally.

### Chief postings gated on policy

`JobPosting` carries `poster_class: PosterClass` (`Chief / Bureaucrat / HouseholdHead / Individual`), `reward: f32`, and `settlement_id: Option<SettlementId>`. `JobPosting::chief_defaults()` returns the chief stub.

Per-resource gates in `chief_job_posting_system`:

| Branch | Gate |
|---|---|
| Stockpile Calories (food) | `policy_for(Fruit).chief_allocates_labor` |
| Stockpile Wood/Stone | `policy_for(target_rid).chief_allocates_labor` |
| Stockpile (CraftOrder demand) | per `target_rid` |
| Haul (per-blueprint deposit) | `policy_for(slot.resource_id).chief_allocates_labor` |
| Build | `!faction.state_funds_public_works` |
| Craft | `policy_for(recipe.output_resource).chief_allocates_labor` |
| Farm | `policy_for(Grain).chief_allocates_labor` |

Default factions have an empty policy map → all chief postings fire as before. Capitalist factions opt out selectively.

**Household income skim:** `split_market_earnings_with_household(world, agent, earned)` (`economy/transactions.rs`) redirects `HOUSEHOLD_INCOME_SKIM (10%)` of market earnings to the household treasury when the agent has `HouseholdMember`. Called from `trader_sell_at_settlement` and `market_sell_system`.

**Household-poster path:** `household_contract_posting_system` (Economy, every `HOUSEHOLD_POSTING_CADENCE = TICKS_PER_DAY`) walks every household with `treasury >= HOUSEHOLD_MIN_TREASURY_FOR_POSTING (10.0)` and posts a Tools craft contract via `post_craft_contract_from_treasury`. `poster_class=HouseholdHead`, `reward = HOUSEHOLD_CONTRACT_REWARD (5.0)`, lands on the **village's** job board.

### Per-settlement markets

Production trade routes through the agent's faction's first settlement market when one exists; SOLO/unsettled fall back to the global `Market`.

- **`SettlementMarket`** (`economy/market.rs`): `calculate_price`, `sell_item`, `try_buy_item`, `clear_flow`, `set_stock` (trader fast path).
- **`market_sell_system` / `market_buy_system`**: take `Res<SettlementMap>` + `Query<&mut Settlement>` + `&FactionMember`. Disjoint borrows.
- **`settlement_price_update_system`** (Economy, alongside global `price_update_system`): walks every Settlement, ticks `update_prices`, clears bid counters. Bid-driven discovery — see `economy/CLAUDE.md` for the price formula. No synthetic demand injection.
- **`ui/economy_panel.rs`** prefers the player faction's first settlement market.

### Maslow needs + esteem/self-actualization triggers

Two additive needs on `Needs`: `esteem: f32` (Tier 4) and `self_actualization: f32` (Tier 5). Both inverted polarity (0 = unfulfilled).

`MaslowTier::next_unmet(needs)` returns the lowest-numbered tier below threshold. Strictly *additive* — does not replace `goal_update_system`'s goal selection (the load-bearing path for need-driven goals).

- **`esteem_driven_posting_system`** (Economy, every `ESTEEM_POSTING_CADENCE = TICKS_PER_DAY`, in its own `add_systems` call to dodge Bevy's 20-tuple ceiling): walks agents whose `next_unmet == Esteem` AND `currency >= ESTEEM_POSTING_MIN_CURRENCY (50.0)`; posts a Torch contract via `post_craft_contract` with `reward = ESTEEM_CONTRACT_REWARD (8.0)`, bumps `esteem` by `ESTEEM_POSTING_GAIN (30.0)`. Hungry/unsafe/lonely agents never reach the gate.
- **`self_actualization_teaching_system`** (`teaching.rs`, Economy, every `SELF_ACTUALIZATION_CADENCE = TICKS_PER_DAY`): walks agents whose `next_unmet == SelfActualization` with at least one Learned tech; picks the highest-complexity Learned tech and writes `LectureRequest`. `apply_lecture_request_system` consumes it on the same tick. Bumps `self_actualization` by `SELF_ACTUALIZATION_LECTURE_GAIN (30.0)`.

### U_bid scoring at job-claim layer

`job_claim_system` (`jobs.rs`) branches on `posting.reward`:

- **Paid (`reward > 0.0`):** `U_bid = E(R) - C_action - C_opportunity` where `E(R) = posting.reward * wealth_modifier(agent.currency)` (`wealth_modifier(c) = 1.0 + 0.5 / (c + 50)`, bounded at 1.0 — poor agents value the same reward more); `C_action = euclidean(agent_tile, work_tile) * BID_DIST_DISCOUNT`; `C_opportunity = 0.0` (stub).
- **Unpaid (`reward == 0.0`, chief / legacy):** legacy `priority + skill + bias - distance`.

Workers query gains `&EconomicAgent` for `wealth_modifier`.

### Trader profession (autonomous arbitrage)

`Profession::Trader`. `trader_buy_at_settlement` / `trader_sell_at_settlement` are atomic primitives in `economy/transactions.rs` — debits agent currency, credits/debits settlement treasury, mutates market stock + flow. Returns per-unit price actually paid. `SettlementMarket::set_stock(id, qty)` is the fast-path setter so the helpers don't double-mutate currency.

- **`TraderPlan { phase, buy_settlement, sell_settlement, resource_id, qty }`** component (`person.rs`). `TraderPhase::TravelingToBuy | TravelingToSell`. Removed when cycle completes.
- **`trader_market_step_system`** (Economy, exclusive `&mut World`, after `self_actualization_teaching_system`): on arrival at the phase's market tile, calls buy/sell helpers and advances or clears the plan. Seeds new plans on idle traders by scanning `AgentMemory.visited_settlements` pairs for the best Cloth gap exceeding `TRADER_MIN_GAP=0.25`. Currency floor `TRADER_MIN_CAPITAL=30.0` gates plan creation.
- **`trader_route_dispatch_system`** (ParallelB, after `bureaucrat_admin_dispatch_system`): for plan-bearing traders not at their phase target, dispatches `Task::Lead { dest }` — same gates as bureaucrat (Idle aq + UNEMPLOYED + non-Dormant + non-Drafted + no PlayerOrder + non-preempting goal). `goal_preempts_trade` (Survive/Sleep/Defend/Raid/Lead/Rescue) lets need-driven goals preempt; the next idle tick reinstalls a plan.
- V1 scope: Cloth-only arbitrage + `TRADER_TRADE_QTY=5`. Future versions can iterate over multiple resources per dispatch tick.

### Tribute

Treasury-to-treasury transfer between factions — agent currency untouched.

- **`FactionData.dominance_over: Vec<u32>`** + **`subordinate_to: Option<u32>`**.
- **`FactionRegistry::set_dominance(dominant, subordinate)`** — idempotent, maintains both ends.
- **`tribute_payment_system`** (Economy, after `bureaucrat_salary_tick_system`, every `TRIBUTE_CADENCE = TICKS_PER_DAY`): walks every faction with `subordinate_to.is_some()`; transfers `min(TRIBUTE_PER_DAY, subordinate.treasury)` to overlord. Destitute subordinates pay 0 — no debt.

### P2P craft contracts

- **`post_craft_contract(world, poster, faction_id, recipe, qty, reward, deadline)`** (`jobs.rs`): atomic — debits poster's currency, pushes a `JobPosting` with `poster_class=Individual` + contract terms, spawns a `JobEscrow` sidecar. Refuses on insufficient funds / invalid recipe / qty=0 / non-positive reward.
- Lifecycle: completion = caller zeros `escrow.amount` then despawns (no-op refund). Cancellation/expiry = caller despawns with amount intact; `on_job_escrow_remove` refunds the poster.
- **U_bid integration:** `reward > 0` fires the U_bid branch for smiths claiming — wealthy poster's contract outscores an equidistant chief Craft posting.

## Game lifecycle and regions

- **`GameState`:** `SpawnSelect` (default) | `Playing`. Globe gen at `WorldPlugin::build`. Spawn-select UI in `SpawnSelect`; spawn systems on `OnEnter(Playing)`. Update-stage systems gated `in_state(Playing)`; FixedUpdate sim systems are not gated (queries empty until entities spawn).
- **`PendingSpawn(Option<(i32, i32)>)`:** player's chosen mega-chunk; `None` = globe centre (sandbox).
- **Sandbox bypass:** `SandboxPlugin` flips state to `Playing` at Startup.
- **`SettledRegions` (`region.rs`):** `AHashMap<RegionId, SettledRegion>` + `by_megachunk` reverse index. `SettledRegion { megachunk, founding_tick, name, camera_bookmark, player_owned }`. `settle()` idempotent.
- **`MegaChunkCoord`:** `from_chunk/from_tile/center_tile/chunk_range`. `MEGACHUNK_SIZE_CHUNKS=16`, independent of climate cells.
- **Multi-focus chunk streaming (`SimulationFocus`):** `Vec<FocusPoint>` rebuilt each tick from camera + every settled region centre. Chunk DATA loads for any focus disc (camera `LOAD_RADIUS=12`, region `REGION_LOAD_RADIUS=6`); SPRITES + plants + loose rocks only inside camera focus.
- **Focus-aware LOD (`update_lod_levels_system`):** camera distance produces base LOD; entities within 8 chunks of any non-camera focus are promoted from Dormant to Aggregate.
- **Edge-walk expansion (`region::detect_edge_crossing_system`, Economy):** each tick checks every player-faction Person's mega-chunk; if unsettled, calls `settle()`, names "Outpost N", bookmarks position, emits `ActivityLogEvent::RegionSettled`.
