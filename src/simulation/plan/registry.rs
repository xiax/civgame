//! Built-in step and plan definitions.
//!
//! Split out of `plan/mod.rs` so plan-data edits don't force a re-read of the
//! types/scoring/execution code. `register_builtin_steps` and
//! `register_builtin_plans` are the only public entry points; everything else
//! is private static data shared between them.

use super::{
    mk_weights, AgentGoal, GoodSelector, MaterialNeed, MemoryKind, PlanDef, PlanId, PlanRegistry,
    StepDef, StepId, StepPreconditions, StepRegistry, StepTarget, TaskKind, TileKind,
    PF_DROP_FOOD_ON_TIMEOUT, PF_EXPLORE, PF_NONE, PF_SCAVENGE, PF_TARGETS_FOOD,
    PF_UNINTERRUPTIBLE, SI_HAS_FOOD,
    SI_CRAFT_ORDER_NEEDS_MATERIAL, SI_IN_FACTION, SI_MEM_FOOD, SI_MEM_WOOD,
    SI_SEASON_FOOD, SI_SKILL_BUILDING, SI_SKILL_COMBAT, SI_SKILL_CRAFTING, SI_SKILL_FARMING,
    SI_SOCIAL, SI_STORAGE_BERRY_SEED, SI_STORAGE_FOOD, SI_STORAGE_GRAIN_SEED, SI_STORAGE_STONE,
    SI_STORAGE_WOOD, SI_VIS_GROUND_FOOD, SI_VIS_PLANT_FOOD, SI_VIS_TREE,
    SI_WILLPOWER_DISTRESS,
};
use crate::simulation::items::EquipmentSlot;
use crate::simulation::needs::EAT_TRIGGER_HUNGER;
use crate::simulation::person::Profession;
use crate::simulation::plants::PlantKind;
use crate::simulation::technology;

// ── Built-in step and plan definitions ───────────────────────────────────────

static GRASS_TILES: &[TileKind] = &[TileKind::Grass];
static FARMLAND_TILES: &[TileKind] = &[TileKind::Farmland];
static FOREST_TILES: &[TileKind] = &[TileKind::Forest];

// Step 9 = Eat, Step 10 = WithdrawFood (defined in register_builtin_steps)
// Gather plans always end in DepositGoods. Eating-from-hand is handled by
// `htn_eat_dispatch_system` driven by `EatFromInventoryMethod` (Phase 5b-ii);
// the walk-to-storage-then-eat path is owned by
// `htn_acquire_food_dispatch_system` driven by `WithdrawFromStorageMethod`
// (Phase 5b-iii-ii). Chaining Eat into gather plans would drop the whole
// plan when the worker isn't yet hungry, leaving food stranded in hand and
// no deposit run.
// PLAN_STEPS_0 (ForageFood) was retired in the Forage→HTN migration. The
// `[Gather, Eat]` chain (under `AgentGoal::Survive`) is now driven by
// `ForageFromKnownMethod` and the `[Gather, DepositToFactionStorage]` chain
// (under `AgentGoal::GatherFood`) by `ForageFromKnownForStorageMethod`
// (see `htn.rs`). StepId(0) (ForageGrass) and StepId(12) (DepositGoods) are
// still defined: StepId(12) is shared by other deposit plans and the HTN
// gather handoff routes via `TaskKind::DepositResource`; StepId(0) is no
// longer referenced by any plan but kept for in-flight ActivePlan
// compatibility (mirrors the StepId 41/42 ClaimedHaul pattern).
static PLAN_STEPS_1: &[StepId] = &[StepId(1), StepId(12)]; // FarmFood → DepositGoods
// PLAN_STEPS_2 (GatherWood) and PLAN_STEPS_3 (GatherStone) were retired in
// Phase 5c-ii-c-ii — the gather → deposit chain is now driven by HTN
// `htn_acquire_good_dispatch_system` + `GatherFromKnownMethod`. Both StepId(2)
// (ChopForest) and StepId(3) (MineStone) are still defined because BuildBlueprint
// (PLAN_STEPS_7) embeds StepId(2) as its first step, and external dispatchers
// can reuse `TaskKind::Gather`.
// PLAN_STEPS_4 (PlantFromStorage) and PLAN_STEPS_66 (PlantBerryFromStorage)
// retired in Phase 5e-v — `htn_plant_from_storage_dispatch_system` +
// `WithdrawAndPlantSeedMethod` own the [WithdrawMaterial { seed, 1 },
// Planter { tile }] expansion now. Both plans were dead code (registered but
// never seeded into any agent's KnownPlans, so chiefs posting JobKind::Farm
// could only drive harvesting via FarmFood). StepId(4) (PlantGrainSeed) and
// StepId(61) (PlantBerrySeed) are no longer referenced by any plan but kept
// as `(unused)` placeholders for in-flight ActivePlan compatibility.
// Phase 5e-xii-d: StepIds 33 (WithdrawGrainSeed), 60 (WithdrawBerrySeed),
// 36 (PlantGrainSeedAsPlay), 62 (PlantBerrySeedAsPlay) all survive as
// orphans — their consumers PlanId 30 / 67 retired into HTN methods.
// PLAN_STEPS_67 (PlayByPlantingBerry) deleted with that migration.
// HuntFood: muster at hearth → wait for party → travel to chief's area →
// PLAN_STEPS_5 retired in Phase 5e-viii-c — the legacy `HuntFood` plan
// (PlanId 5) is fully replaced by three HTN abstract tasks:
// `JoinHuntParty` (Muster + Travel), `EngagePrey` (Hunt + PickUpCorpse), and
// `DeliverHuntKill` (HaulCorpse + Butcher). StepDefs 5/53/54/55/57/58 survive
// in the registry as orphans for in-flight ActivePlan compatibility;
// `StepId::HUNT` / `StepId::PICK_UP_CORPSE` / `StepId::HAUL_CORPSE` /
// `StepId::BUTCHER` / `StepId::MUSTER_AT_HEARTH` / `StepId::TRAVEL_TO_HUNT_AREA`
// consts are kept as PlanHistory ring-buffer sentinels.
// PLAN_STEPS_HUNTER_ARM retired in Phase 5e-ii — `htn_equip_hunting_spear_dispatch_system`
// + `WithdrawAndEquipHuntingSpearMethod` own the [WithdrawSpear, EquipMainHand]
// expansion now.
// PLAN_STEPS_SCOUT retired in Phase 5e — `htn_scout_dispatch_system` +
// `ScoutForPreyMethod` own the wander-for-prey expansion now.
// PLAN_STEPS_6 (ScavengeFood) was retired in Phase 5c-ii-d-vi — the
// scavenge-then-deposit chain is now driven by `ScavengeFoodForStorageMethod`
// under `StockpileFood` (see `htn.rs`). StepId(6) (CollectFood) and StepId(12)
// (DepositGoods) are still defined: StepId(12) is shared by other deposit
// plans (24 ReturnSurplusFood) and the HTN scavenge handoff routes via
// `TaskKind::DepositResource`; StepId(6) is no longer referenced by any plan
// but kept for in-flight ActivePlan compatibility (mirrors the StepId 41/42
// ClaimedHaul pattern).
static PLAN_STEPS_7: &[StepId] = &[StepId(2), StepId(28), StepId(25)]; // GatherWood, HaulToBlueprint, BuildAnyBlueprint
static PLAN_STEPS_29: &[StepId] = &[StepId(32), StepId(28), StepId(25)]; // FetchMaterialFromStorage, HaulToBlueprint, BuildAnyBlueprint
// PLAN_STEPS_9 (WithdrawAndEat) was retired in Phase 5b-iii-ii — the
// walk-to-storage-then-eat path is now driven by HTN
// `htn_acquire_food_dispatch_system` + `WithdrawFromStorageMethod`. Both
// StepId(10) (WithdrawFood) and StepId(9) (Eat) are still defined because
// the legacy Forage / ScavengeFood plans embed the shared Eat step and the
// HTN dispatcher reuses the WithdrawFood `TaskKind` executor.
// PLAN_STEPS_10 retired in Phase 5e-iv — `htn_tame_horse_dispatch_system`
// + `TameWildHorseMethod` own the single-step TameAnimal dispatch now.

static SURVIVE_GOALS: &[AgentGoal] = &[AgentGoal::Survive];
// GATHER_FOOD_GOALS was retired in Phase 5c-ii-d-vi when the last two
// GatherFood-only plans (PlanId 6 ScavengeFood, PlanId 35 ExploreForFood)
// were deleted. The HTN registry's StockpileFood methods own the goal now
// via `htn_stockpile_food_dispatch_system`. SURVIVE_AND_GATHER_FOOD_GOALS
// stays — Forage (PlanId 0) and HuntFood (PlanId 5) both serve Survive +
// GatherFood and remain plan-driven for now.
static TAME_HORSE_GOALS: &[AgentGoal] = &[AgentGoal::TameHorse];
// PlanId 2/3 (GatherWood/GatherStone) were retired in 5c-ii-c-ii, and
// PlanId 38/39 (ScavengeWood/ScavengeStone) were retired in 5c-ii-d-ii-b.
// The goal arrays stay because ExploreForWood/Stone (PlanId 36/37) still
// fire as the plan-driven fallback when memory is empty AND no ground item
// is visible. HTN's `GatherFromKnownMethod` (memory) and
// `ScavengeFromGroundMethod` (vision) cover the other two cases.
static GATHER_WOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherWood];
static GATHER_STONE_GOALS: &[AgentGoal] = &[AgentGoal::GatherStone];
static SURVIVE_AND_GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::Survive, AgentGoal::GatherFood];
static FARM_GOALS: &[AgentGoal] = &[AgentGoal::Farm];
static BUILD_GOALS: &[AgentGoal] = &[AgentGoal::Build];
static HAUL_GOALS: &[AgentGoal] = &[AgentGoal::Haul];
static CRAFT_GOALS: &[AgentGoal] = &[AgentGoal::Craft];
static RESCUE_GOALS: &[AgentGoal] = &[AgentGoal::Rescue];

// Phase 5e-x: PLAN_STEPS_23 retired. The HTN
// `htn_combat_faction_dispatch_system` dispatches the rescue chain directly.
// PLAN_STEPS_24 retired in Phase 5e-iii — `htn_return_surplus_dispatch_system`
// + `DepositSurplusAtStorageMethod` own the deposit-at-storage chain. StepId(12)
// is still defined and shared by FarmFood (PlanId 1) + WorkOnCraft (PlanId 16).
// PLAN_STEPS_25 (EatFromInventory) was retired in Phase 5b-ii — the in-place
// eat-with-food-on-hand dispatch is now driven by `EatFromInventoryMethod`
// (see `htn.rs`). The shared StepId(9) Eat step is still used as the final
// step of Forage/Scavenge/WithdrawAndEat plans.
// PLAN_STEPS_26 (PlaySocial) and PLAN_STEPS_27 (PlaySolo) retired in Phase
// 5e-xii-a — `htn_play_dispatch_system` + `PlayWithPartnerMethod` /
// `PlaySoloMethod` own both branches end-to-end. PlanIds 26/27 flipped to
// `(unused)`. StepId(29) (PlayWithPartner) and StepId(30) (PlayWithItem)
// stay defined as orphans — StepId(30) is shared with PlanId 32
// (PlayWithStoredToy) which still runs through the legacy plan registry.
// PLAN_STEPS_28 (Explore: walk to a random reachable tile near home) was
// retired in Phase 5c-ii-d-vi — the last consumer was PlanId 35 (ExploreForFood),
// itself retired in this PR. StepId(31) (Explore) survives in the StepRegistry
// as the legacy executor that HTN's `Task::Explore` dispatchers prime via
// `assign_task_with_routing(... TaskKind::Explore, None ...)`.
// Phase 5e-xii-d: PLAN_STEPS_30 (PlayByPlanting) retired — see PlanId 30
// definition below for the migration notes. StepIds 33 (WithdrawGrainSeed)
// and 36 (PlantGrainSeedAsPlay) survive in the StepRegistry as orphans.
// Phase 5e-xii-b: PLAN_STEPS_31 (PlayByThrowingRocks) retired — see PlanId 31
// definition below for the migration notes. StepIds 34 (WithdrawStone) and 37
// (ThrowRocksAsPlay) survive in the StepRegistry as orphans for in-flight
// ActivePlan compatibility.
// Phase 5e-xii-c: PLAN_STEPS_32 (PlayWithStoredToy) retired — see PlanId 32
// definition below. StepId 35 (WithdrawPlayItem) and StepId 30 (PlayWithItem,
// shared with retired PlanId 27 PlaySolo) survive in the StepRegistry as
// orphans for in-flight ActivePlan compatibility.

static PLAY_GOALS: &[AgentGoal] = &[AgentGoal::Play];

static RETURN_CAMP_GOALS: &[AgentGoal] = &[AgentGoal::ReturnCamp];

// New craft pipeline (order-driven). The Deliver*ToCraftOrder plans haul a
// specific good into an open CraftOrder's deposit slots; "WorkOnCraft" runs
// the recipe once the order is satisfied. Wood/stone draw from the faction
// stockpile through plan 15, which uses step 40's MostDeficient selector to
// pick whichever good open orders need most. Hide and grain still come from
// a fresh hunt/harvest because no storage-fetch path exists for those.
//   38 = HaulToCraftOrder, 39 = WorkOnCraftOrder,
//   40 = FetchCraftOrderMaterialFromStorage (most-deficient good).
static PLAN_STEPS_13: &[StepId] = &[StepId(5), StepId(13), StepId(38)]; // DeliverHideToCraftOrder
// PLAN_STEPS_14 (DeliverGrainToCraftOrder) retired in Phase 5e-xi-c —
// `htn_harvest_grain_for_craft_order_dispatch_system` +
// `HarvestAndHaulGrainToCraftOrderMethod` own the chain end-to-end.
// PlanId(14) is flipped to `(unused)`. StepId(1) (FarmFood) is shared with
// PlanId 1 (FarmFood), and StepId(38) is shared with PlanId 13 — both
// survive.
// PLAN_STEPS_15 (DeliverFromStorageToCraftOrder) retired in Phase 5e-xi-a —
// `htn_deliver_material_to_craft_order_dispatch_system` +
// `WithdrawAndHaulToCraftOrderMethod` own the chain end-to-end. PlanId(15) is
// flipped to `(unused)`. StepId(40) (FetchCraftOrderMaterialFromStorage) was
// the only consumer of `MaterialNeed::CraftOrder` from plan-driven dispatch; it
// stays defined as an Idle placeholder for in-flight ActivePlan compatibility.
// PLAN_STEPS_16 (WorkOnCraft) retired in Phase 5e-xi-b —
// `htn_work_on_craft_order_dispatch_system` + `WorkOnSatisfiedCraftOrderMethod`
// own the labor leg + trailing deposit chain. PlanId(16) is flipped to
// `(unused)`. StepId(39) (WorkOnCraftOrder) and StepId(12) (DepositGoods) stay
// defined as orphans — StepId(12) is shared with FarmFood (PlanId 1).

// Faction-directed Build pipeline. The Haul half of this pipeline (PLAN_STEPS_H,
// PlanId 33 ClaimedHaul) was retired in Phase 5c-ii-b — the claim-driven
// `WithdrawMaterial → HaulToBlueprint` chain now flows through
// `htn_acquire_good_dispatch_system` (`WithdrawAndHaulToBlueprintMethod`).
// StepId 41 (WithdrawClaimedHaulMaterial) and StepId 42 (HaulToClaimedBlueprint)
// are no longer referenced by any plan but kept in the StepRegistry for
// in-flight ActivePlan compatibility.
// PLAN_STEPS_BB (ClaimedBuild) was retired in Phase 5e-vi — the
// `[Construct { blueprint }]` expansion is now driven by HTN
// `htn_build_claimed_blueprint_dispatch_system` + `BuildClaimedBlueprintMethod`.
// StepId(43) (BuildClaimedBlueprint) is kept as an `(unused)` placeholder for
// in-flight ActivePlan compatibility (mirrors the StepId(41/42) ClaimedHaul
// pattern); the const `StepId::BUILD_CLAIMED_BLUEPRINT = StepId(43)` survives
// only as a stable sentinel for `PlanHistory` ring-buffer entries.

// PLAN_STEPS_SW / PLAN_STEPS_SS (ScavengeWood/Stone PlanId 38/39) were
// retired in Phase 5c-ii-d-ii-b — the vision-based scavenge chain
// `[Scavenge, DepositToFactionStorage]` now flows through
// `htn_acquire_good_dispatch_system`'s gather branch driven by
// `ScavengeFromGroundMethod` (utility 1.5 > GatherFromKnownMethod's 1.0).
// StepId(44) (CollectWood) and StepId(45) (CollectStone) are no longer
// referenced by any plan but kept in the StepRegistry for in-flight
// ActivePlan compatibility (mirrors the StepId(41/42) ClaimedHaul pattern).

// Phase 5e-ix retired PLAN_STEPS_SOCIALIZE / SOCIALIZE_GOALS;
// Phase 5e-x retired PLAN_STEPS_RAID / DEFEND / LEAD + corresponding
// *_GOALS arrays. All four social/military single-step plans now dispatch
// via `htn_combat_faction_dispatch_system` (Raid / Defend / Lead) and
// `htn_socialize_dispatch_system` (Socialize).

pub fn register_builtin_steps(registry: &mut StepRegistry) {
    registry.0 = vec![
        StepDef {
            // 0: ForageGrass — targets BerryBushes, falls back via memory
            id: StepId(0),
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::AnyEdible),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::BerryBush),
            withdraw_filter: None,
        },
        StepDef {
            // 1: FarmFarmland — targets Grain, falls back via memory
            id: StepId(1),
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::AnyEdible),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::Grain),
            withdraw_filter: None,
        },
        StepDef {
            // 2: ChopForest
            id: StepId(2),
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::wood()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 3: MineStone
            id: StepId(3),
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::stone()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 4: (unused — was PlantGrainSeed; retired in Phase 5e-v.
            // `htn_plant_from_storage_dispatch_system` + `WithdrawAndPlantSeedMethod`
            // emit `Task::Planter { tile }` directly. ID kept for in-flight
            // ActivePlan compatibility.)
            id: StepId(4),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 5: Hunt
            id: StepId(5),
            task: TaskKind::Hunter,
            target: StepTarget::HuntPrey,
            preconditions: StepPreconditions::needs_good(crate::economy::core_ids::weapon(), 1),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 6: CollectFood
            id: StepId(6),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestEdible,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 7: (unused — reserved for future use)
            id: StepId(7),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 8: (unused — reserved for future use)
            id: StepId(8),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 9: Eat — consume edibles from inventory in place. Gated on hunger
            // so plans don't waste food when the agent is already sated.
            id: StepId(9),
            task: TaskKind::Eat,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::eat_when_hungry(EAT_TRIGGER_HUNGER),
            reward_scale: 1.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 10: WithdrawFood — pull one edible from a faction storage tile
            id: StepId(10),
            task: TaskKind::WithdrawFood,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        // StepId 11 (TameAnimal) retired in Phase 5e-iv — the HTN
        // `TameWildHorseMethod` emits `Task::TameAnimal { target }` directly.
        // The const `StepId::TAME_ANIMAL = StepId(11)` survives only as a
        // stable sentinel.
        // ── Crafting steps ────────────────────────────────────────────────────
        StepDef {
            // 12: DepositGoods — deposit crafted items at faction storage
            id: StepId(12),
            task: TaskKind::DepositResource,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::carry_anything(),
            reward_scale: 0.1,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 13: CollectSkin — pick up Skin from ground (after hunting)
            id: StepId(13),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(crate::economy::core_ids::skin()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            withdraw_filter: None,
        },
        // 14-23: legacy per-recipe Craft steps. Replaced by the order-driven
        // pipeline (steps 38-40); kept as Idle placeholders so existing
        // step-id references in any in-flight ActivePlan don't panic on lookup.
        StepDef {
            id: StepId(14),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(15),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(16),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(17),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(18),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(19),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(20),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(21),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(22),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(23),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 24: (unused — reserved for future use)
            id: StepId(24),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 25: BuildAnyBlueprint — finds the nearest accessible blueprint of any kind
            // and contributes wood + labor. Requirements come from the blueprint itself.
            id: StepId(25),
            task: TaskKind::Construct,
            target: StepTarget::NearestAnyBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.2,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 26: (unused — formerly FindMate; reproduction is now passive via co-sleeping)
            id: StepId(26),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 27: (unused — Phase 5e-x retired the legacy `RescueAlly`
            // plan (PlanId 23) and its `EngageRescue` step. The HTN
            // `htn_combat_faction_dispatch_system` reads `RescueTarget`
            // directly. The const `StepId::ENGAGE_RESCUE = StepId(27)`
            // survives only as a stable sentinel.)
            id: StepId(27),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 28: HaulToBlueprint — carry currently-held materials to the nearest
            // blueprint that still needs them and drop them in. Excess stays in
            // the hauler's inventory; the step ends as soon as the drop is applied.
            id: StepId(28),
            task: TaskKind::HaulMaterials,
            target: StepTarget::NearestBlueprintNeedingHeldMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 29: PlayWithPartner — route to the nearest play partner and
            // recreate together. play_system handles tick-by-tick willpower
            // refill, social fill, and bilateral affinity.
            id: StepId(29),
            task: TaskKind::Play,
            target: StepTarget::NearestPlayPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 30: PlayWithItem — solo play. Resolves to the agent's tile if
            // they already hold an entertaining good, else the nearest ground
            // item with non-zero entertainment_value.
            id: StepId(30),
            task: TaskKind::Play,
            target: StepTarget::NearestPlayItem,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 31: Explore — walk to a random reachable tile near home.
            // Used by the Explore plan as the NN's "no good options right now"
            // choice; reward_scale is intentionally low so the network reaches
            // for it only when other plans score worse.
            id: StepId(31),
            task: TaskKind::Explore,
            target: StepTarget::ExploreTile,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.05,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 32: FetchMaterialFromStorage — route to the nearest faction
            // storage tile that holds a good currently needed by an unsatisfied
            // blueprint and pull the most-deficient material into the agent's
            // inventory. Pairs with step 28 (HaulToBlueprint) so stockpiled
            // wood/stone can ferry into in-progress build sites without a
            // fresh gather wave.
            id: StepId(32),
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::WithdrawForFactionNeed {
                need: MaterialNeed::Blueprint,
                selector: GoodSelector::MostDeficient,
            },
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 33: WithdrawGrainSeed — pull one GrainSeed from faction storage.
            id: StepId(33),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(crate::economy::core_ids::grain_seed()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(
                    crate::economy::core_ids::grain_seed(),
                ),
            ),
        },
        StepDef {
            // 34: WithdrawStone — pull one Stone from a faction storage tile so
            // the agent can throw it as recreation in step 37.
            id: StepId(34),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(crate::economy::core_ids::stone()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(crate::economy::core_ids::stone()),
            ),
        },
        StepDef {
            // 35: WithdrawPlayItem — pull one entertainment-valued good from a
            // faction storage tile.
            id: StepId(35),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithEntertainment,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::AnyEntertainment,
            ),
        },
        StepDef {
            // 36: PlantGrainSeedAsPlay — plant a held GrainSeed on a grass tile
            // as recreation. Spawns Grain, awards Farming XP + activity, plus a
            // one-shot willpower burst on completion.
            id: StepId(36),
            task: TaskKind::PlayPlant,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(crate::economy::core_ids::grain_seed(), 1),
            reward_scale: 0.6,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 37: ThrowRocksAsPlay — throw a held Stone as recreation. Consumes
            // one Stone, awards Combat XP + ActivityKind::Combat, bursts
            // willpower. Resolves to the agent's current tile (they throw in
            // place; the rock is consumed).
            id: StepId(37),
            task: TaskKind::PlayThrow,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(crate::economy::core_ids::stone(), 1),
            reward_scale: 0.6,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 38: HaulToCraftOrder — drop currently-held materials into the
            // nearest CraftOrder that needs them. Sibling of step 28
            // (HaulToBlueprint) for the order pipeline.
            id: StepId(38),
            task: TaskKind::HaulToCraftOrder,
            target: StepTarget::NearestCraftOrderNeedingHeldMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 39: (unused — was WorkOnCraftOrder; retired in Phase 5e-xi-b
            // along with PlanId 16. The HTN
            // `htn_work_on_craft_order_dispatch_system` does the equivalent
            // satisfied-order scan inline before snapshotting into
            // `PlannerCtx.target_craft_order`. ID kept as Idle placeholder for
            // in-flight ActivePlan compat.)
            id: StepId(39),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 40: (unused — was FetchCraftOrderMaterialFromStorage; retired in
            // Phase 5e-xi-a along with PlanId 15. The HTN
            // `htn_deliver_material_to_craft_order_dispatch_system` does the
            // equivalent demand-aggregation + most-deficient-resource pick
            // inline before snapshotting into `PlannerCtx.target_craft_order`.
            // ID kept as Idle placeholder for in-flight ActivePlan compat.)
            id: StepId(40),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 41: WithdrawClaimedHaulMaterial — withdraw the good named in the
            // agent's active JobClaim::Haul from the nearest faction storage
            // tile holding that good. Pairs with step 42
            // (HaulToClaimedBlueprint) to deliver storage stock into a
            // specific blueprint.
            id: StepId(41),
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::WithdrawForFactionNeed {
                need: MaterialNeed::HaulClaim,
                // Selector is overridden by the resolver after reading the
                // claim's good; placeholder kept for the type.
                selector: GoodSelector::MostDeficient,
            },
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 42: HaulToClaimedBlueprint — carry the agent's hand contents to
            // the specific blueprint named in the active JobClaim::Haul and
            // deposit. Credits the Haul posting via record_progress on success.
            id: StepId(42),
            task: TaskKind::HaulMaterials,
            target: StepTarget::HaulClaimBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 43: (unused — was BuildClaimedBlueprint; retired in Phase 5e-vi.
            // The walk-to-claimed-bp + on-site labor path is now driven by HTN
            // `htn_build_claimed_blueprint_dispatch_system` +
            // `BuildClaimedBlueprintMethod`, which dispatches the existing
            // `Task::Construct { blueprint }` typed variant. ID kept for
            // in-flight ActivePlan compatibility and PlanHistory sentinel
            // stability via `StepId::BUILD_CLAIMED_BLUEPRINT`.)
            id: StepId(43),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 44: CollectWood — pick up loose Wood GroundItems left behind by
            // tree harvesting (`harvest_ground_drops`) or earlier spills.
            id: StepId(44),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(crate::economy::core_ids::wood()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 45: CollectStone — pick up loose Stone GroundItems on the world.
            id: StepId(45),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(crate::economy::core_ids::stone()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 46: (unused — was per-good FetchWoodFromStorage; superseded by
            // step 40's MostDeficient selector). Kept as Idle so any in-flight
            // ActivePlan referencing this id falls through to the lookup
            // failure path rather than panicking.
            id: StepId(46),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 47: (unused — was per-good FetchStoneFromStorage; superseded by
            // step 40's MostDeficient selector).
            id: StepId(47),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        // 48-51: social/military steps that previously lived as hard-coded
        // goal arms in `tasks::goal_dispatch_system`. Now plan-driven so
        // every goal flows through `plan_execution_system`.
        StepDef {
            // 48: (unused — Phase 5e-ix retired the legacy `Socialize` plan
            // (PlanId 60) and its `NearestPlayPartner` step. The HTN
            // `htn_socialize_dispatch_system` does the equivalent
            // `SpatialIndex` scan and dispatches `Task::Socialize { partner }`
            // directly. The const `StepId::SOCIALIZE = StepId(48)` survives
            // only as a stable sentinel for in-flight ActivePlan
            // compatibility.)
            id: StepId(48),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        // 49-51: Phase 5e-x retired the Raid / Defend / Lead StepDefs.
        // The HTN `htn_combat_faction_dispatch_system` resolves the
        // destination tile directly from `FactionRegistry`. Consts
        // `StepId::RAID_TARGET / DEFEND_CAMP / LEAD_CAMP` survive only
        // as stable sentinels.
        StepDef {
            id: StepId(49),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(50),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            id: StepId(51),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        // StepId 52 (WithdrawSpear) retired in Phase 5e-ii — the HTN
        // `WithdrawAndEquipHuntingSpearMethod` emits `Task::WithdrawMaterial`
        // directly. The const `StepId::WITHDRAW_SPEAR = StepId(52)` survives
        // only as a stable sentinel.
        StepDef {
            // 53: Walk adjacent to a fresh Corpse and attach it to the
            // hunter via `PersonAI.carried_corpse`. No-op for non-hunters.
            id: StepId(53),
            task: TaskKind::PickUpCorpse,
            target: StepTarget::NearestFreshCorpse,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 54: Drag the carried corpse to the nearest hearth or faction
            // home tile. `corpse_follow_system` keeps the corpse Transform
            // glued to the hunter while they walk.
            id: StepId(54),
            task: TaskKind::HaulCorpse,
            target: StepTarget::NearestButcherSite,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 55: Butcher the carried corpse in place — work_ticks then yield
            // Meat+Skin per `species_yield()` and despawn the corpse.
            id: StepId(55),
            task: TaskKind::Butcher,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        // StepId 56 (EquipMainHand) retired in Phase 5e-ii — the HTN
        // `WithdrawAndEquipHuntingSpearMethod` emits `Task::Equip { MainHand,
        // weapon }` directly. The const `StepId::EQUIP_WEAPON = StepId(56)`
        // survives only as a stable sentinel.
        StepDef {
            // 57: Muster for hunt. Walk to the hearth tile selected by the
            // chief's HuntOrder (closest campfire to the hunt area, or
            // home_tile fallback). On arrival the executor
            // `wait_for_party_task_system` registers the hunter into
            // `hunt_order.mustered` and blocks until the party has filled
            // (`mustered.len() >= target_party_size`) or `deployed_tick` is
            // already set. Resolves to None when no Hunt order is active,
            // making the candidate filter naturally drop the plan when the
            // chief stops asking.
            id: StepId(57),
            task: TaskKind::HuntPartyMuster,
            target: StepTarget::HearthForHunt,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 58: Travel to the chief's chosen hunt area. Reuses the Explore
            // task's "walk to tile, idle on arrival" semantics — once the
            // hunter is on the area_tile, step 5's HuntPrey scan finds prey
            // within VIEW_RADIUS naturally.
            id: StepId(58),
            task: TaskKind::Explore,
            target: StepTarget::HuntArea,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: None,
        },
        // StepId 59 (WanderForPrey) retired in Phase 5e — replaced by
        // `htn_scout_dispatch_system` + `ScoutForPreyMethod`. The
        // `StepId::WANDER_FOR_PREY = StepId(59)` const survives only as a
        // stable sentinel; no PlanDef references it.
        StepDef {
            // 60: WithdrawBerrySeed — pull one BerrySeed from faction storage.
            id: StepId(60),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(crate::economy::core_ids::berry_seed()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(
                    crate::economy::core_ids::berry_seed(),
                ),
            ),
        },
        StepDef {
            // 61: (unused — was PlantBerrySeed; retired in Phase 5e-v alongside
            // StepId(4) PlantGrainSeed. The HTN `WithdrawAndPlantSeedMethod`
            // emits `Task::Planter { tile }` directly. ID kept for in-flight
            // ActivePlan compatibility.)
            id: StepId(61),
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 62: PlantBerrySeedAsPlay — plant a held BerrySeed on a grass tile
            // as recreation. Spawns BerryBush, awards Farming XP + activity,
            // plus a one-shot willpower burst on completion.
            id: StepId(62),
            task: TaskKind::PlayPlant,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(crate::economy::core_ids::berry_seed(), 1),
            reward_scale: 0.6,
            plant_filter: None,
            withdraw_filter: None,
        },
    ];
}

pub fn register_builtin_plans(registry: &mut PlanRegistry) {
    // Hand-tuned linear weights on `build_state_vec` features (see `STATE_DIM`
    // and `SI_*` constants). Score = dot(state, state_weights) + bias + manual
    // bonuses (persistence, ally, distance) applied at selection time.
    //
    // Design rule: weights score *plan-specific viability*, not the need that
    // selected the goal. Inside a need-driven goal (Survive, Build, Socialize)
    // the triggering need is constant across all candidate plans, so weighting
    // it again is circular noise. Plans discriminate via inventory presence,
    // visibility, memory, skills, and faction-storage stocks (slots 29-32).
    registry.0 = vec![
        PlanDef {
            // Forage→HTN migration: the legacy `ForageFood` plan
            // (`[ForageGrass, DepositGoods]`, served Survive + GatherFood) is
            // now driven by `ForageFromKnownMethod` (AcquireFood, ends in
            // `Eat`) and `ForageFromKnownForStorageMethod` (StockpileFood,
            // ends in `DepositToFactionStorage`). PlanId 0 is retired with no
            // successor; the const survives as a stable sentinel for
            // PlanHistory ring-buffer entries (`faction.rs::hunter_demote`
            // uses it as a placeholder write).
            id: PlanId(0),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Farming is a deferred-payoff plan; weight it on skill + season,
            // and let it fade as storage fills. Hunger weight removed — under
            // Survive the eating plans now dominate via bias, and under
            // GatherFood hunger is moderate anyway.
            id: PlanId(1),
            name: "FarmFood",
            steps: PLAN_STEPS_1,
            state_weights: mk_weights(&[
                (SI_SKILL_FARMING, 0.4),
                (SI_SEASON_FOOD, 0.6),
                (SI_STORAGE_FOOD, -0.2),
            ]),
            bias: 0.0,
            serves_goals: FARM_GOALS,
            tech_gate: Some(technology::CROP_CULTIVATION),
            memory_target_kind: Some(MemoryKind::AnyEdible),
            flags: PF_NONE,
            requires_profession: None,
        },
        // PlanId 2 (GatherWood) and PlanId 3 (GatherStone) were retired in
        // Phase 5c-ii-c-ii. The gather → deposit chain is now produced by
        // `htn_acquire_good_dispatch_system` + `GatherFromKnownMethod` under
        // `AgentGoal::GatherWood` / `AgentGoal::GatherStone`. The
        // `GATHER_WOOD_GOALS` / `GATHER_STONE_GOALS` static arrays are also
        // retired (their only consumers were these two PlanDefs).
        PlanDef {
            // 4: (unused — was PlantFromStorage; retired in Phase 5e-v. The
            // [WithdrawGrainSeed, PlantGrainSeed] chain is now produced by
            // `htn_plant_from_storage_dispatch_system` +
            // `WithdrawAndPlantSeedMethod` under `AgentGoal::Farm`. The plan
            // was dead code anyway — never seeded into any KnownPlans. ID
            // kept for PlanHistory ring-buffer stability; bias is wired so
            // the candidate filter never selects it.)
            id: PlanId(4),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-viii-c (2026-05-06): PlanId 5 (`HuntFood`) fully retired.
        // The complete hunting pipeline now runs through HTN:
        // - `JoinHuntParty` (`htn_join_hunt_party_dispatch_system`):
        //   `MusterAtHearthMethod` + `TravelToHuntAreaMethod`
        // - `EngagePrey` (`htn_engage_prey_dispatch_system`):
        //   `HuntPreyMethod` + `PickUpFreshCorpseMethod`
        // - `DeliverHuntKill` (`htn_deliver_hunt_kill_dispatch_system`):
        //   `DeliverHuntKillMethod`
        // The const `PlanId::HUNT_FOOD = PlanId(5)` survives only as a stable
        // sentinel for `PlanHistory` ring-buffer entries (e.g.
        // `faction.rs::hunter_demote`). bias is wired so the candidate filter
        // never selects this entry.
        PlanDef {
            id: PlanId(5),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5c-ii-d-vi: PlanId 6 (`ScavengeFood`) deleted. The HTN
        // registry's `ScavengeFoodForStorageMethod` under `StockpileFood`
        // (utility 1.5) replaces the GatherFood path; the
        // `ScavengeFoodFromGroundMethod` under `AcquireFood` (5c-ii-d-iii-ii)
        // covered the Survive path. PlanId 6 is retired with no successor;
        // ID is kept reserved by the constant in `plan/mod.rs` for
        // PlanHistory ring-buffer stability.
        PlanDef {
            // Gather-then-build sibling: only worth picking when storage is
            // dry and the agent can actually see/remember wood. Otherwise
            // HaulFromStorageAndBuild (29) is the cheaper path.
            id: PlanId(7),
            name: "BuildBlueprint",
            steps: PLAN_STEPS_7,
            state_weights: mk_weights(&[
                (SI_SKILL_BUILDING, 0.5),
                (SI_VIS_TREE, 0.4),
                (SI_MEM_WOOD, 0.3),
                (SI_STORAGE_WOOD, -0.5),
            ]),
            bias: 0.2,
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(8),
            name: "BuildBed",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0, // unused — never selected
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // 9: (unused — was WithdrawAndEat; retired in Phase 5b-iii-ii.
            // The walk-to-storage-then-eat path is owned by HTN
            // `htn_acquire_food_dispatch_system` + `WithdrawFromStorageMethod`.
            // ID kept for PlanHistory ring-buffer stability; bias is wired so
            // the candidate filter never selects it.)
            id: PlanId(9),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-iv: PlanId 10 (`TameHorse`) retired. The HTN
        // `TameWildHorseMethod` under `AbstractTaskKind::TameWildHorse`
        // (`htn_tame_horse_dispatch_system`) owns the wild-horse-taming
        // dispatch end-to-end. Const survives as a PlanHistory sentinel.
        PlanDef {
            id: PlanId(10),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // ── Crafting plans (order-driven) ─────────────────────────────────────
        // Each Deliver*ToCraftOrder gathers one raw resource and hauls it into
        // an open CraftOrder's deposit slots; WorkOnCraft runs the recipe once
        // the order is satisfied. Plans are filtered out at dispatch time when
        // no order needs the corresponding good (resolve_target → None).
        PlanDef {
            // 11: (unused — was DeliverWoodToCraftOrder; collapsed into plan
            // 15 whose step 40 now picks the most-deficient material across
            // all open orders. ID kept for PlanHistory ring-buffer stability;
            // bias is wired so the candidate filter never selects it.)
            id: PlanId(11),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // 12: (unused — was DeliverStoneToCraftOrder; collapsed into plan 15.)
            id: PlanId(12),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(13),
            name: "DeliverHideToCraftOrder",
            steps: PLAN_STEPS_13,
            state_weights: mk_weights(&[(SI_SKILL_COMBAT, 0.5), (SI_HAS_FOOD, -0.1)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        // PlanId 14 retired in Phase 5e-xi-c — the HTN
        // `htn_harvest_grain_for_craft_order_dispatch_system` +
        // `HarvestAndHaulGrainToCraftOrderMethod` own the chain end-to-end.
        // The const `PlanId::DELIVER_GRAIN_TO_CRAFT_ORDER` survives only as a
        // PlanHistory ring-buffer sentinel.
        PlanDef {
            id: PlanId(14),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // PlanId 15 retired in Phase 5e-xi-a — the HTN
        // `htn_deliver_material_to_craft_order_dispatch_system` +
        // `WithdrawAndHaulToCraftOrderMethod` own the deliver-from-storage
        // chain. The const `PlanId::DELIVER_FROM_STORAGE_TO_CRAFT_ORDER`
        // survives only as a stable PlanHistory ring-buffer sentinel.
        PlanDef {
            id: PlanId(15),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // PlanId 16 retired in Phase 5e-xi-b — the HTN
            // `htn_work_on_craft_order_dispatch_system` +
            // `WorkOnSatisfiedCraftOrderMethod` own the labor leg + trailing
            // deposit. The const `PlanId::WORK_ON_CRAFT` survives only as a
            // PlanHistory ring-buffer sentinel.
            id: PlanId(16),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(17),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(18),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(19),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(20),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(21),
            name: "BuildCampfire",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0, // unused
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(22),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-x: PlanId 23 (`RescueAlly`) retired. The HTN
        // `EngageRescueAttackerMethod` under `AbstractTaskKind::RescueAlly`
        // (`htn_combat_faction_dispatch_system`) owns the dispatch end-to-end:
        // reads the agent's `RescueTarget`, writes `CombatTarget`, routes via
        // `assign_task_with_routing(... TaskKind::Defend, Some(attacker) ...)`,
        // and dispatches `Task::RescueAlly { attacker, dest }`. The const
        // `PlanId::RESCUE_ALLY = PlanId(23)` survives only as a stable
        // sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(23),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-iii: PlanId 24 (`ReturnSurplusFood`) retired. The HTN
        // `DepositSurplusAtStorageMethod` under `AbstractTaskKind::ReturnSurplus`
        // (`htn_return_surplus_dispatch_system`) owns the walk-back-to-storage
        // path. The const `PlanId::RETURN_SURPLUS_FOOD = PlanId(24)` survives
        // only as a stable sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(24),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // PlanId(25) "EatFromInventory" was retired in Phase 5b-ii. The
        // single-step "if you have food and you're hungry, eat in place"
        // dispatch now flows through `htn::htn_eat_dispatch_system` driven by
        // `EatFromInventoryMethod`. PlanId(9) "WithdrawAndEat" was retired in
        // Phase 5b-iii-ii — replaced by `htn::htn_acquire_food_dispatch_system`
        // + `WithdrawFromStorageMethod`. Other Survive plans (Forage,
        // ScavengeFood) still embed `StepId(9)` Eat as their final step and
        // dispatch through `plan_execution_system`.
        // PlanIds 26 (PlaySocial) and 27 (PlaySolo) retired in Phase 5e-xii-a
        // — the HTN `htn_play_dispatch_system` + `PlayWithPartnerMethod` /
        // `PlaySoloMethod` own both branches under `AgentGoal::Play`. The
        // consts `PlanId::PLAY_SOCIAL` / `PlanId::PLAY_SOLO` survive only as
        // PlanHistory ring-buffer sentinels.
        PlanDef {
            id: PlanId(26),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(27),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5c-ii-d-iv-ii: PlanId 36/37 (`ExploreForWood`/`ExploreForStone`)
        // deleted. The HTN `ExploreForMaterialMethod` (utility 0.3) under
        // `AcquireGood` replaces both — `htn_acquire_good_dispatch_system`'s
        // gather branch dispatches `Task::Explore { kind }` whenever no
        // concrete method's precondition fires.
        //
        // Phase 5c-ii-d-vi: PlanId 35 (`ExploreForFood`) deleted. The HTN
        // `ExploreForFoodForStorageMethod` (utility 0.3) under `StockpileFood`
        // replaces the GatherFood case; the `ExploreForFoodMethod` under
        // `AcquireFood` (5c-ii-d-iv-ii) covered the Survive case. ID is kept
        // reserved by the constant in `plan/mod.rs` for PlanHistory
        // ring-buffer stability.
        PlanDef {
            // Sibling of BuildBlueprint that pulls materials out of communal
            // storage instead of gathering fresh from the world. Keeps in-progress
            // build sites moving once the initial gather wave has dropped its
            // surplus into granaries — without this plan, blueprints stall at
            // the "haulers can only deliver what they happen to be carrying"
            // step (NearestBlueprintNeedingHeldMaterial).
            id: PlanId(29),
            name: "HaulFromStorageAndBuild",
            steps: PLAN_STEPS_29,
            state_weights: mk_weights(&[
                (SI_SKILL_BUILDING, 0.5),
                (SI_STORAGE_WOOD, 0.6),
                (SI_STORAGE_STONE, 0.4),
                (SI_IN_FACTION, 0.3),
            ]),
            bias: 0.2,
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        // Phase 5e-xii-d: PlanId 30 (`PlayByPlanting`) retired. The HTN
        // `WithdrawAndPlantGrainSeedAsPlayMethod` under
        // `AbstractTaskKind::Play` (`htn_play_dispatch_system`) owns the
        // dispatch end-to-end — withdraws a Grain seed from faction storage
        // and routes via `TaskKind::PlayPlant` to the nearest unplanted
        // grass tile. The const `PlanId::PLAY_BY_PLANTING = PlanId(30)`
        // survives only as a stable sentinel for `PlanHistory` ring-buffer
        // entries. StepIds 33 (WithdrawGrainSeed) and 36 (PlantSeedAsPlay)
        // survive as orphans for in-flight ActivePlan compatibility.
        PlanDef {
            id: PlanId(30),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-xii-b: PlanId 31 (`PlayByThrowingRocks`) retired. The HTN
        // `WithdrawAndThrowStonesAsPlayMethod` under `AbstractTaskKind::Play`
        // (`htn_play_dispatch_system`) owns the dispatch end-to-end —
        // withdraws one Stone from faction storage, primes the legacy channel
        // for the in-place PlayThrow on arrival, consumes the stone and
        // bursts willpower. The const `PlanId::PLAY_BY_THROWING_ROCKS = PlanId(31)`
        // survives only as a stable sentinel for `PlanHistory` ring-buffer
        // entries. StepIds 34 (WithdrawStone) and 37 (ThrowRocksAsPlay)
        // survive as orphans in the registry — neither is consumed by any
        // live PlanDef, but the IDs stay reserved for in-flight ActivePlan
        // compatibility.
        PlanDef {
            id: PlanId(31),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-xii-c: PlanId 32 (`PlayWithStoredToy`) retired. The HTN
        // `WithdrawAndPlayWithToyMethod` under `AbstractTaskKind::Play`
        // (`htn_play_dispatch_system`) owns the dispatch end-to-end —
        // withdraws the highest-entertainment-valued resource from faction
        // storage, primes the legacy channel for solo Play once the toy is in
        // hand, and `play_system` accumulates willpower scaled by the toy's
        // `entertainment_value`. The const `PlanId::PLAY_WITH_STORED_TOY = PlanId(32)`
        // survives only as a stable sentinel for `PlanHistory` ring-buffer
        // entries. StepIds 35 (WithdrawPlayItem) and 30 (PlayWithItem) survive
        // as orphans — StepId 30 is shared with the retired PlanId 27
        // PlaySolo, also unconsumed today.
        PlanDef {
            id: PlanId(32),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-vi: PlanId 34 (`ClaimedBuild`) retired. The HTN
        // `BuildClaimedBlueprintMethod` under
        // `AbstractTaskKind::ConstructBlueprint`
        // (`htn_build_claimed_blueprint_dispatch_system`) owns the
        // walk-to-claimed-bp-and-labor path end-to-end. The const
        // `PlanId::CLAIMED_BUILD = PlanId(34)` survives only as a stable
        // sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(34),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // PlanId 38/39 (ScavengeWood/ScavengeStone) were retired in
        // Phase 5c-ii-d-ii-b — see the registry preamble notes above and
        // `ScavengeFromGroundMethod` in `htn.rs`.
        // ── Social-goal plans (60-63) ───────────────────────────────────
        // Migrated out of `tasks::goal_dispatch_system` so every goal
        // dispatches through `plan_execution_system`. Each is a single-step
        // plan; the candidate filter only selects them when the matching
        // goal is active (Socialize/Raid/Defend/Lead) and the agent is the
        // sole plan serving that goal. A high `bias` keeps them dominant
        // against any future siblings.
        // Phase 5e-ix: PlanId 60 (`Socialize`) retired. The HTN
        // `SocializeWithPartnerMethod` under `AbstractTaskKind::Socialize`
        // (`htn_socialize_dispatch_system`) owns the dispatch end-to-end —
        // scans `SpatialIndex` for the nearest other Person, routes via
        // `assign_task_with_routing(... TaskKind::Socialize ...)`, and
        // dispatches `Task::Socialize { partner }`. The const
        // `PlanId::SOCIALIZE = PlanId(60)` survives only as a stable
        // sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(60),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-x: PlanIds 61/62/63 (Raid/Defend/Lead) retired. The HTN
        // methods under `htn_combat_faction_dispatch_system` own the
        // dispatch end-to-end. Consts survive as PlanHistory sentinels.
        PlanDef {
            id: PlanId(61),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(62),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(63),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-ii: PlanId 64 (`AcquireHuntingSpear`) retired. The HTN
        // `WithdrawAndEquipHuntingSpearMethod` under
        // `AbstractTaskKind::EquipHuntingSpear`
        // (`htn_equip_hunting_spear_dispatch_system`) owns the hunter-arming
        // path end-to-end — the dispatcher runs ahead of the food path so an
        // unarmed hunter prefers fetching their spear, then the typed
        // [WithdrawMaterial, Equip] chain auto-promotes via
        // `finish_withdraw_material`'s Equip arm. The const
        // `PlanId::ACQUIRE_HUNTING_SPEAR = PlanId(64)` survives only as a
        // stable sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(64),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e: PlanId 65 (`ScoutForPrey`) retired. The HTN
        // `ScoutForPreyMethod` under `AbstractTaskKind::Scout`
        // (`htn_scout_dispatch_system`) owns the chief-Scout flow end-to-end —
        // hunters with `HuntOrder::Scout` dispatch via the typed Explore
        // channel instead of competing in the plan registry. The const
        // `PlanId::SCOUT_FOR_PREY = PlanId(65)` survives only as a stable
        // sentinel for `PlanHistory` ring-buffer entries.
        PlanDef {
            id: PlanId(65),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // 66: (unused — was PlantBerryFromStorage; retired in Phase 5e-v
            // alongside PlanId(4) PlantFromStorage. The HTN
            // `htn_plant_from_storage_dispatch_system` argmaxes across
            // grain/berry seed stocks to pick the higher-stocked seed and
            // emits the same chain. Dead code historically — never seeded
            // into any KnownPlans. ID kept for PlanHistory stability.)
            id: PlanId(66),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        // Phase 5e-xii-d: PlanId 67 (`PlayByPlantingBerry`) retired. The HTN
        // `WithdrawAndPlantBerrySeedAsPlayMethod` under
        // `AbstractTaskKind::Play` (`htn_play_dispatch_system`) owns the
        // dispatch end-to-end. PlanId 67 was already dead code (never seeded
        // into any agent's `KnownPlans`); the HTN method restores planting
        // berry seeds end-to-end. The const survives as a PlanHistory
        // sentinel; StepIds 60 (WithdrawBerrySeed) and 62 (PlantBerrySeedAsPlay)
        // survive as orphans for in-flight ActivePlan compatibility.
        PlanDef {
            id: PlanId(67),
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
    ];
}
