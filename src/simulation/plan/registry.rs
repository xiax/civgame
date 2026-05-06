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
use crate::economy::goods::Good;
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
static PLAN_STEPS_4: &[StepId] = &[StepId(33), StepId(4)]; // PlantGrainFromStorage: WithdrawGrainSeed → PlantGrainSeed
static PLAN_STEPS_66: &[StepId] = &[StepId(60), StepId(61)]; // PlantBerryFromStorage: WithdrawBerrySeed → PlantBerrySeed
static PLAN_STEPS_67: &[StepId] = &[StepId(60), StepId(62)]; // PlayByPlantingBerry: WithdrawBerrySeed → PlantBerrySeedAsPlay
// HuntFood: muster at hearth → wait for party → travel to chief's area →
// engage prey → corpse pickup/haul/butcher. The first three steps fold the
// chief's hunting-party formation into the existing hunter pipeline so all
// hunters depart together rather than each agent independently sniping prey.
static PLAN_STEPS_5: &[StepId] =
    &[StepId(57), StepId(58), StepId(5), StepId(53), StepId(54), StepId(55)]; // MusterAtHearth → TravelToHuntArea → Hunt → PickUpCorpse → HaulCorpse → Butcher
static PLAN_STEPS_HUNTER_ARM: &[StepId] = &[StepId(52), StepId(56)]; // AcquireHuntingSpear: WithdrawSpear → EquipMainHand
static PLAN_STEPS_SCOUT: &[StepId] = &[StepId(59)]; // ScoutForPrey: WanderForPrey (single step, ends on prey memory)
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
static PLAN_STEPS_10: &[StepId] = &[StepId(11)]; // TameHorse: TameAnimal

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

static PLAN_STEPS_23: &[StepId] = &[StepId(27)]; // RescueAlly: EngageRescue
static PLAN_STEPS_24: &[StepId] = &[StepId(12)]; // ReturnSurplusFood: DepositGoods at faction storage
// PLAN_STEPS_25 (EatFromInventory) was retired in Phase 5b-ii — the in-place
// eat-with-food-on-hand dispatch is now driven by `EatFromInventoryMethod`
// (see `htn.rs`). The shared StepId(9) Eat step is still used as the final
// step of Forage/Scavenge/WithdrawAndEat plans.
static PLAN_STEPS_26: &[StepId] = &[StepId(29)]; // PlaySocial: PlayWithPartner (resolves partner inline)
static PLAN_STEPS_27: &[StepId] = &[StepId(30)]; // PlaySolo: PlayWithItem (resolves item inline)
// PLAN_STEPS_28 (Explore: walk to a random reachable tile near home) was
// retired in Phase 5c-ii-d-vi — the last consumer was PlanId 35 (ExploreForFood),
// itself retired in this PR. StepId(31) (Explore) survives in the StepRegistry
// as the legacy executor that HTN's `Task::Explore` dispatchers prime via
// `assign_task_with_routing(... TaskKind::Explore, None ...)`.
static PLAN_STEPS_30: &[StepId] = &[StepId(33), StepId(36)]; // PlayByPlanting: WithdrawSeed → PlantSeedAsPlay
static PLAN_STEPS_31: &[StepId] = &[StepId(34), StepId(37)]; // PlayByThrowingRocks: WithdrawStone → ThrowRocksAsPlay
static PLAN_STEPS_32: &[StepId] = &[StepId(35), StepId(30)]; // PlayWithStoredToy: WithdrawPlayItem → PlayWithItem (step 30, plays in place when held)

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
static PLAN_STEPS_14: &[StepId] = &[StepId(1), StepId(38)]; // DeliverGrainToCraftOrder
static PLAN_STEPS_15: &[StepId] = &[StepId(40), StepId(38)]; // DeliverFromStorageToCraftOrder
static PLAN_STEPS_16: &[StepId] = &[StepId(39), StepId(12)]; // WorkOnCraft → DepositGoods

// Faction-directed Build pipeline. The Haul half of this pipeline (PLAN_STEPS_H,
// PlanId 33 ClaimedHaul) was retired in Phase 5c-ii-b — the claim-driven
// `WithdrawMaterial → HaulToBlueprint` chain now flows through
// `htn_acquire_good_dispatch_system` (`WithdrawAndHaulToBlueprintMethod`).
// StepId 41 (WithdrawClaimedHaulMaterial) and StepId 42 (HaulToClaimedBlueprint)
// are no longer referenced by any plan but kept in the StepRegistry for
// in-flight ActivePlan compatibility.
//   43 = BuildClaimedBlueprint.
static PLAN_STEPS_BB: &[StepId] = &[StepId(43)]; // ClaimedBuildPlan

// PLAN_STEPS_SW / PLAN_STEPS_SS (ScavengeWood/Stone PlanId 38/39) were
// retired in Phase 5c-ii-d-ii-b — the vision-based scavenge chain
// `[Scavenge, DepositToFactionStorage]` now flows through
// `htn_acquire_good_dispatch_system`'s gather branch driven by
// `ScavengeFromGroundMethod` (utility 1.5 > GatherFromKnownMethod's 1.0).
// StepId(44) (CollectWood) and StepId(45) (CollectStone) are no longer
// referenced by any plan but kept in the StepRegistry for in-flight
// ActivePlan compatibility (mirrors the StepId(41/42) ClaimedHaul pattern).

// Social-goal plans (60-63). Each is a single-step plan whose target lookup
// matches what `goal_dispatch_system` used to do inline — see the StepDef
// notes at IDs 48-51 for resolver details.
static PLAN_STEPS_SOCIALIZE: &[StepId] = &[StepId(48)];
static PLAN_STEPS_RAID: &[StepId] = &[StepId(49)];
static PLAN_STEPS_DEFEND: &[StepId] = &[StepId(50)];
static PLAN_STEPS_LEAD: &[StepId] = &[StepId(51)];

static SOCIALIZE_GOALS: &[AgentGoal] = &[AgentGoal::Socialize];
static RAID_GOALS: &[AgentGoal] = &[AgentGoal::Raid];
static DEFEND_GOALS: &[AgentGoal] = &[AgentGoal::Defend];
static LEAD_GOALS: &[AgentGoal] = &[AgentGoal::Lead];

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
            // 4: PlantGrainSeed (requires GrainSeed in inventory)
            id: StepId(4),
            task: TaskKind::Planter,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::GrainSeed, 1),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 5: Hunt
            id: StepId(5),
            task: TaskKind::Hunter,
            target: StepTarget::HuntPrey,
            preconditions: StepPreconditions::needs_good(Good::Weapon, 1),
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
        StepDef {
            // 11: TameAnimal — work adjacent to a wild horse for ~100 ticks
            id: StepId(11),
            task: TaskKind::TameAnimal,
            target: StepTarget::NearestWildHorse,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            withdraw_filter: None,
        },
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
            target: StepTarget::NearestItem(Good::Skin.into()),
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
            // 27: EngageRescue — route to the attacker stored on RescueTarget and engage.
            // CombatTarget is already set by respond_to_distress_system; combat_system
            // takes over as soon as the responder is adjacent.
            id: StepId(27),
            task: TaskKind::Defend,
            target: StepTarget::RescueAttacker,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
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
            target: StepTarget::NearestFactionStorageWithGood(Good::GrainSeed.into()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(
                    Good::GrainSeed.into(),
                ),
            ),
        },
        StepDef {
            // 34: WithdrawStone — pull one Stone from a faction storage tile so
            // the agent can throw it as recreation in step 37.
            id: StepId(34),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Stone.into()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(Good::Stone.into()),
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
            preconditions: StepPreconditions::needs_good(Good::GrainSeed, 1),
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
            preconditions: StepPreconditions::needs_good(Good::Stone, 1),
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
            // 39: WorkOnCraftOrder — adjacent to a satisfied CraftOrder,
            // advance work_progress until the recipe completes.
            id: StepId(39),
            task: TaskKind::WorkOnCraftOrder,
            target: StepTarget::NearestSatisfiedCraftOrder,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 40: FetchCraftOrderMaterialFromStorage — withdraw the most-
            // deficient good across open CraftOrders from a faction storage
            // tile so it can be hauled to the order. The faction's open
            // orders drive the choice; the agent doesn't need a per-good
            // plan variant.
            id: StepId(40),
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::WithdrawForFactionNeed {
                need: MaterialNeed::CraftOrder,
                selector: GoodSelector::MostDeficient,
            },
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
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
            // 43: BuildClaimedBlueprint — perform labor at the specific
            // blueprint named in the agent's JobClaim::Build. The resolver
            // gates on the blueprint being satisfied, so this never starts
            // before all materials are in.
            id: StepId(43),
            task: TaskKind::Construct,
            target: StepTarget::BuildClaimBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 44: CollectWood — pick up loose Wood GroundItems left behind by
            // tree harvesting (`harvest_ground_drops`) or earlier spills.
            id: StepId(44),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Wood.into()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 45: CollectStone — pick up loose Stone GroundItems on the world.
            id: StepId(45),
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Stone.into()),
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
            // 48: Socialize at the nearest other Person. Reuses
            // `NearestPlayPartner` (radius 12 spatial scan filtering out
            // animals/blueprints) — the resolver returns a partner entity
            // and tile, then `assign_task_with_routing` walks the agent
            // there with TaskKind::Socialize.
            id: StepId(48),
            task: TaskKind::Socialize,
            target: StepTarget::NearestPlayPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 49: Walk to the home tile of the faction we're raiding (per
            // `FactionRegistry::raid_target`). Solo agents and peacetime
            // factions resolve to None and the plan aborts harmlessly.
            id: StepId(49),
            task: TaskKind::Raid,
            target: StepTarget::FactionRaidTarget,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 50: Walk to faction camp and run TaskKind::Defend. Reuses
            // `FactionCamp` — the resolver returns home_tile.
            id: StepId(50),
            task: TaskKind::Defend,
            target: StepTarget::FactionCamp,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 51: Walk to faction camp and run TaskKind::Lead.
            id: StepId(51),
            task: TaskKind::Lead,
            target: StepTarget::FactionCamp,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 52: Hunter pulls a Spear (Good::Weapon) from faction storage.
            // Used by `AcquireHuntingSpear` plan; the plan-level `forbids_good`
            // precondition ensures armed hunters skip this entirely.
            id: StepId(52),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Weapon.into()),
            preconditions: StepPreconditions::forbids(Good::Weapon),
            reward_scale: 0.4,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(
                    Good::Weapon.into(),
                ),
            ),
        },
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
        StepDef {
            // 56: Equip a Weapon (Spear) into the MainHand slot. Instant
            // in-place transfer from inventory or hands → Equipment.MainHand.
            // Used as the second step of `AcquireHuntingSpear` (plan 64) so a
            // hunter who fetched the spear actually wields it for combat
            // damage. The plan-level `forbids_good(Weapon)` check now also
            // counts the equipped slot, so the plan self-deselects after this
            // step completes.
            id: StepId(56),
            task: TaskKind::Equip,
            target: StepTarget::EquipItem {
                slot: EquipmentSlot::MainHand,
                resource_id: Good::Weapon.into(),
            },
            preconditions: StepPreconditions::needs_good(Good::Weapon, 1),
            reward_scale: 0.5,
            plant_filter: None,
            withdraw_filter: None,
        },
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
        StepDef {
            // 59: Wander for prey. Used by the ScoutForPrey plan when the
            // chief has no prey near the home tile; the agent ranges out to
            // unmapped tiles and `vision_system` writes prey memory along
            // the way, which the chief's next decision cycle picks up.
            id: StepId(59),
            task: TaskKind::Explore,
            target: StepTarget::ScoutForPrey,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.1,
            plant_filter: None,
            withdraw_filter: None,
        },
        StepDef {
            // 60: WithdrawBerrySeed — pull one BerrySeed from faction storage.
            id: StepId(60),
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::BerrySeed.into()),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            withdraw_filter: Some(
                crate::simulation::typed_task::WithdrawGoodFilter::Specific(
                    Good::BerrySeed.into(),
                ),
            ),
        },
        StepDef {
            // 61: PlantBerrySeed (requires BerrySeed in inventory)
            id: StepId(61),
            task: TaskKind::Planter,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::BerrySeed, 1),
            reward_scale: 0.2,
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
            preconditions: StepPreconditions::needs_good(Good::BerrySeed, 1),
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
            // Withdraw a GrainSeed from faction storage, then plant it on the
            // nearest Grass tile. Scores high when grain seeds are stockpiled
            // and food supply is low.
            id: PlanId(4),
            name: "PlantFromStorage",
            steps: PLAN_STEPS_4,
            state_weights: mk_weights(&[
                (SI_STORAGE_GRAIN_SEED, 1.0),
                (SI_SKILL_FARMING, 0.2),
                (SI_STORAGE_FOOD, -0.3),
            ]),
            bias: 0.0,
            serves_goals: FARM_GOALS,
            tech_gate: Some(technology::CROP_CULTIVATION),
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(5),
            name: "HuntFood",
            steps: PLAN_STEPS_5,
            state_weights: mk_weights(&[
                (SI_SKILL_COMBAT, 0.6),
                (SI_HAS_FOOD, -0.2),
                (SI_STORAGE_FOOD, -0.3),
            ]),
            bias: 0.5,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: Some(technology::HUNTING_SPEAR),
            memory_target_kind: Some(MemoryKind::Prey),
            // Multi-step faction commitment — survival need spikes shouldn't
            // peel a hunter off a corpse mid-haul. The plan still ends via
            // completion / timeout / target invalidation / rescue preempt.
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: Some(Profession::Hunter),
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
        PlanDef {
            id: PlanId(10),
            name: "TameHorse",
            steps: PLAN_STEPS_10,
            state_weights: mk_weights(&[]),
            bias: 0.1,
            serves_goals: TAME_HORSE_GOALS,
            tech_gate: Some(technology::HORSE_TAMING),
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
        PlanDef {
            id: PlanId(14),
            name: "DeliverGrainToCraftOrder",
            steps: PLAN_STEPS_14,
            state_weights: mk_weights(&[(SI_SKILL_FARMING, 0.5), (SI_SEASON_FOOD, 0.4)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::AnyEdible),
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        PlanDef {
            // Scores on SI_CRAFT_ORDER_NEEDS_MATERIAL (1.0 when any faction
            // CraftOrder has unmet deposits) so the plan only wins when hauling
            // work actually exists. The negative bias means that without an open
            // order the plan scores ≈ -0.6, which loses to WorkOnCraft (≈ 0.55)
            // and breaks the FailedNoTarget loop seen when no CraftOrders spawn.
            id: PlanId(15),
            name: "DeliverFromStorageToCraftOrder",
            steps: PLAN_STEPS_15,
            state_weights: mk_weights(&[
                (SI_CRAFT_ORDER_NEEDS_MATERIAL, 2.0),
                (SI_STORAGE_WOOD, 0.3),
                (SI_STORAGE_STONE, 0.3),
                (SI_IN_FACTION, 0.3),
            ]),
            bias: -1.5,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(16),
            name: "WorkOnCraft",
            steps: PLAN_STEPS_16,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.5)]),
            bias: 0.3,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
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
        PlanDef {
            // Sole candidate under Rescue goal; weights only need to keep
            // combat-skilled responders preferred. Old `SI_SAFETY=0.5` was
            // perversely scoring the *agent's own* safety need, biasing AWAY
            // from rescue under threat — dropped.
            id: PlanId(23),
            name: "RescueAlly",
            steps: PLAN_STEPS_23,
            state_weights: mk_weights(&[(SI_SKILL_COMBAT, 0.3)]),
            bias: 0.5,
            serves_goals: RESCUE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(24),
            name: "ReturnSurplusFood",
            steps: PLAN_STEPS_24,
            state_weights: mk_weights(&[(SI_HAS_FOOD, 0.3), (SI_IN_FACTION, 0.3)]),
            bias: 0.0,
            serves_goals: RETURN_CAMP_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_DROP_FOOD_ON_TIMEOUT,
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
        PlanDef {
            id: PlanId(26),
            name: "PlaySocial",
            steps: PLAN_STEPS_26,
            state_weights: mk_weights(&[(SI_SOCIAL, 1.5), (SI_WILLPOWER_DISTRESS, 0.5)]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(27),
            name: "PlaySolo",
            steps: PLAN_STEPS_27,
            state_weights: mk_weights(&[(SI_SOCIAL, 0.6), (SI_WILLPOWER_DISTRESS, 0.7)]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
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
        PlanDef {
            // Take a Seed from faction storage and plant it as recreation.
            // Doubles as low-effort farming progress: each completion spawns a
            // Grain plant and feeds Farming activity for tech discovery.
            id: PlanId(30),
            name: "PlayByPlanting",
            steps: PLAN_STEPS_30,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.6),
                (SI_SKILL_FARMING, 0.4),
                (SI_SEASON_FOOD, 0.3),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Take a Stone from faction storage and throw it as recreation.
            // Each completion increments ActivityKind::Combat (driving combat
            // tech discovery) and grants a small Combat XP bump.
            id: PlanId(31),
            name: "PlayByThrowingRocks",
            steps: PLAN_STEPS_31,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.6),
                (SI_SKILL_COMBAT, 0.4),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Pull an entertainment-valued good (luxury, cloth, tools, …) from
            // faction storage and play with it in place. Chains into PlaySolo's
            // PlayWithItem step so the willpower-per-tick refill scales by the
            // toy's `entertainment_value`.
            id: PlanId(32),
            name: "PlayWithStoredToy",
            steps: PLAN_STEPS_32,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.7),
                (SI_SOCIAL, 0.3),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Claim-driven Build plan. Fires only when the agent holds a
            // JobClaim::Build (gating goal to AgentGoal::Build via job lock).
            // Step 43 routes to the claimed blueprint and labors there.
            id: PlanId(34),
            name: "ClaimedBuild",
            steps: PLAN_STEPS_BB,
            state_weights: mk_weights(&[
                (SI_SKILL_BUILDING, 0.4),
            ]),
            bias: 1.0,
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
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
        PlanDef {
            // Sole candidate under Socialize goal — bias alone wins.
            // Old SI_SOCIAL=1.5 was the goal-trigger need re-amplified.
            id: PlanId(60),
            name: "Socialize",
            steps: PLAN_STEPS_SOCIALIZE,
            state_weights: mk_weights(&[]),
            bias: 1.0,
            serves_goals: SOCIALIZE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(61),
            name: "Raid",
            steps: PLAN_STEPS_RAID,
            state_weights: mk_weights(&[(SI_SKILL_COMBAT, 0.5)]),
            bias: 1.0,
            serves_goals: RAID_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(62),
            name: "Defend",
            steps: PLAN_STEPS_DEFEND,
            state_weights: mk_weights(&[(SI_SKILL_COMBAT, 0.5)]),
            bias: 1.0,
            serves_goals: DEFEND_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: PlanId(63),
            name: "Lead",
            steps: PLAN_STEPS_LEAD,
            state_weights: mk_weights(&[]),
            bias: 1.0,
            serves_goals: LEAD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Hunter-only fetch plan: pull a Spear (Good::Weapon) from
            // faction storage when unarmed. The step's `forbids_good`
            // precondition means the plan auto-deselects the moment the
            // hunter is armed, so HuntFood (id 5) takes over from there.
            id: PlanId(64),
            name: "AcquireHuntingSpear",
            steps: PLAN_STEPS_HUNTER_ARM,
            state_weights: mk_weights(&[]),
            bias: 5.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: Some(technology::HUNTING_SPEAR),
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: Some(Profession::Hunter),
        },
        PlanDef {
            // Hunter-only scout plan: chief posts `HuntOrder::Scout` when no
            // prey is visible from camp; hunters wander outward writing prey
            // memory. The candidate filter gates this plan on the faction
            // holding a Scout order (HuntFood gates on Hunt), so a single
            // chief flip swaps the active plan. NOT uninterruptible — a
            // scouting hunter can still be peeled off by survival pressures.
            id: PlanId(65),
            name: "ScoutForPrey",
            steps: PLAN_STEPS_SCOUT,
            state_weights: mk_weights(&[]),
            bias: 1.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: Some(technology::HUNTING_SPEAR),
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: Some(Profession::Hunter),
        },
        PlanDef {
            // Withdraw a BerrySeed from faction storage and plant it on the
            // nearest Grass tile, spawning a BerryBush. Scores high when berry
            // seeds are stockpiled and food supply is low.
            id: PlanId(66),
            name: "PlantBerryFromStorage",
            steps: PLAN_STEPS_66,
            state_weights: mk_weights(&[
                (SI_STORAGE_BERRY_SEED, 1.0),
                (SI_SKILL_FARMING, 0.2),
                (SI_STORAGE_FOOD, -0.3),
            ]),
            bias: 0.0,
            serves_goals: FARM_GOALS,
            tech_gate: Some(technology::CROP_CULTIVATION),
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Take a BerrySeed from faction storage and plant it as recreation,
            // spawning a BerryBush. Awards Farming XP + willpower burst.
            id: PlanId(67),
            name: "PlayByPlantingBerry",
            steps: PLAN_STEPS_67,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.6),
                (SI_SKILL_FARMING, 0.4),
                (SI_SEASON_FOOD, 0.3),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
    ];
}
