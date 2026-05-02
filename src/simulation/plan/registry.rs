//! Built-in step and plan definitions.
//!
//! Split out of `plan/mod.rs` so plan-data edits don't force a re-read of the
//! types/scoring/execution code. `register_builtin_steps` and
//! `register_builtin_plans` are the only public entry points; everything else
//! is private static data shared between them.

use super::{
    mk_weights, AgentGoal, GoodSelector, MaterialNeed, MemoryKind, PlanDef, PlanRegistry, StepDef,
    StepId, StepPreconditions, StepRegistry, StepTarget, TaskKind, TileKind,
    PF_DROP_FOOD_ON_TIMEOUT, PF_EXPLORE, PF_NONE, PF_SCAVENGE, PF_TARGETS_FOOD, PF_TARGETS_STONE,
    PF_TARGETS_WOOD, PF_UNINTERRUPTIBLE, SI_HAS_FOOD, SI_HAS_STONE, SI_HAS_WOOD,
    SI_CRAFT_ORDER_NEEDS_MATERIAL, SI_IN_FACTION, SI_MEM_FOOD, SI_MEM_STONE, SI_MEM_WOOD,
    SI_SEASON_FOOD, SI_SKILL_BUILDING, SI_SKILL_COMBAT, SI_SKILL_CRAFTING, SI_SKILL_FARMING,
    SI_SOCIAL, SI_STORAGE_FOOD, SI_STORAGE_SEED, SI_STORAGE_STONE, SI_STORAGE_WOOD, SI_VIS_GROUND_FOOD,
    SI_VIS_GROUND_STONE,
    SI_VIS_GROUND_WOOD, SI_VIS_PLANT_FOOD, SI_VIS_STONE_TILE, SI_VIS_TREE, SI_WILLPOWER_DISTRESS,
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
// Gather plans always end in DepositGoods. Eating is handled by separate
// plans (EatFromInventory, WithdrawAndEat) selected when hunger is high
// enough to satisfy step 9's precondition; chaining Eat into gather plans
// would drop the whole plan when the worker isn't yet hungry, leaving food
// stranded in hand and no deposit run.
static PLAN_STEPS_0: &[StepId] = &[0, 12]; // ForageFood → DepositGoods
static PLAN_STEPS_1: &[StepId] = &[1, 12]; // FarmFood → DepositGoods
static PLAN_STEPS_2: &[StepId] = &[2, 12]; // GatherWood → DepositGoods
static PLAN_STEPS_3: &[StepId] = &[3, 12]; // GatherStone → DepositGoods
static PLAN_STEPS_4: &[StepId] = &[33, 4]; // PlantFromStorage: WithdrawSeed (from storage) → PlantSeed
// HuntFood: muster at hearth → wait for party → travel to chief's area →
// engage prey → corpse pickup/haul/butcher. The first three steps fold the
// chief's hunting-party formation into the existing hunter pipeline so all
// hunters depart together rather than each agent independently sniping prey.
static PLAN_STEPS_5: &[StepId] =
    &[57, 58, 5, 53, 54, 55]; // MusterAtHearth → TravelToHuntArea → Hunt → PickUpCorpse → HaulCorpse → Butcher
static PLAN_STEPS_HUNTER_ARM: &[StepId] = &[52, 56]; // AcquireHuntingSpear: WithdrawSpear → EquipMainHand
static PLAN_STEPS_SCOUT: &[StepId] = &[59]; // ScoutForPrey: WanderForPrey (single step, ends on prey memory)
static PLAN_STEPS_6: &[StepId] = &[6, 12]; // ScavengeFood → DepositGoods
static PLAN_STEPS_7: &[StepId] = &[2, 28, 25]; // GatherWood, HaulToBlueprint, BuildAnyBlueprint
static PLAN_STEPS_29: &[StepId] = &[32, 28, 25]; // FetchMaterialFromStorage, HaulToBlueprint, BuildAnyBlueprint
static PLAN_STEPS_9: &[StepId] = &[10, 9]; // WithdrawAndEat: WithdrawFood → Eat
static PLAN_STEPS_10: &[StepId] = &[11]; // TameHorse: TameAnimal

static SURVIVE_GOALS: &[AgentGoal] = &[AgentGoal::Survive];
static GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherFood];
static TAME_HORSE_GOALS: &[AgentGoal] = &[AgentGoal::TameHorse];
static GATHER_WOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherWood];
static GATHER_STONE_GOALS: &[AgentGoal] = &[AgentGoal::GatherStone];
static SURVIVE_AND_GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::Survive, AgentGoal::GatherFood];
static FARM_GOALS: &[AgentGoal] = &[AgentGoal::Farm];
static BUILD_GOALS: &[AgentGoal] = &[AgentGoal::Build];
static HAUL_GOALS: &[AgentGoal] = &[AgentGoal::Haul];
static CRAFT_GOALS: &[AgentGoal] = &[AgentGoal::Craft];
static RESCUE_GOALS: &[AgentGoal] = &[AgentGoal::Rescue];

static PLAN_STEPS_23: &[StepId] = &[27]; // RescueAlly: EngageRescue
static PLAN_STEPS_24: &[StepId] = &[12]; // ReturnSurplusFood: DepositGoods at faction storage
static PLAN_STEPS_25: &[StepId] = &[9]; // EatFromInventory: Eat (gated by hunger + edible-on-hand)
static PLAN_STEPS_26: &[StepId] = &[29]; // PlaySocial: PlayWithPartner (resolves partner inline)
static PLAN_STEPS_27: &[StepId] = &[30]; // PlaySolo: PlayWithItem (resolves item inline)
static PLAN_STEPS_28: &[StepId] = &[31]; // Explore: walk to a random reachable tile near home
static PLAN_STEPS_30: &[StepId] = &[33, 36]; // PlayByPlanting: WithdrawSeed → PlantSeedAsPlay
static PLAN_STEPS_31: &[StepId] = &[34, 37]; // PlayByThrowingRocks: WithdrawStone → ThrowRocksAsPlay
static PLAN_STEPS_32: &[StepId] = &[35, 30]; // PlayWithStoredToy: WithdrawPlayItem → PlayWithItem (step 30, plays in place when held)

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
static PLAN_STEPS_13: &[StepId] = &[5, 13, 38]; // DeliverHideToCraftOrder
static PLAN_STEPS_14: &[StepId] = &[1, 38]; // DeliverGrainToCraftOrder
static PLAN_STEPS_15: &[StepId] = &[40, 38]; // DeliverFromStorageToCraftOrder
static PLAN_STEPS_16: &[StepId] = &[39, 12]; // WorkOnCraft → DepositGoods

// Faction-directed Haul/Build pipeline. These plans are claim-driven: they
// only fire when the agent already holds a JobClaim of the matching kind.
//   41 = WithdrawClaimedHaulMaterial, 42 = HaulToClaimedBlueprint,
//   43 = BuildClaimedBlueprint.
static PLAN_STEPS_H: &[StepId] = &[41, 42]; // ClaimedHaulPlan
static PLAN_STEPS_BB: &[StepId] = &[43]; // ClaimedBuildPlan

// Scavenge plans for loose Wood/Stone GroundItems (siblings of ScavengeFood).
// Pick up world-litter from chop yields and prior spills, then deposit to
// faction storage.
static PLAN_STEPS_SW: &[StepId] = &[44, 12]; // ScavengeWood: CollectWood → DepositGoods
static PLAN_STEPS_SS: &[StepId] = &[45, 12]; // ScavengeStone: CollectStone → DepositGoods

// Social-goal plans (60-63). Each is a single-step plan whose target lookup
// matches what `goal_dispatch_system` used to do inline — see the StepDef
// notes at IDs 48-51 for resolver details.
static PLAN_STEPS_SOCIALIZE: &[StepId] = &[48];
static PLAN_STEPS_RAID: &[StepId] = &[49];
static PLAN_STEPS_DEFEND: &[StepId] = &[50];
static PLAN_STEPS_LEAD: &[StepId] = &[51];

static SOCIALIZE_GOALS: &[AgentGoal] = &[AgentGoal::Socialize];
static RAID_GOALS: &[AgentGoal] = &[AgentGoal::Raid];
static DEFEND_GOALS: &[AgentGoal] = &[AgentGoal::Defend];
static LEAD_GOALS: &[AgentGoal] = &[AgentGoal::Lead];

pub fn register_builtin_steps(registry: &mut StepRegistry) {
    registry.0 = vec![
        StepDef {
            // 0: ForageGrass — targets BerryBushes, falls back via memory
            id: 0,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::BerryBush),
            extra: 0,
        },
        StepDef {
            // 1: FarmFarmland — targets Grain, falls back via memory
            id: 1,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::Grain),
            extra: 0,
        },
        StepDef {
            // 2: ChopForest
            id: 2,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Wood),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 3: MineStone
            id: 3,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 4: PlantSeed (requires Seed in inventory)
            id: 4,
            task: TaskKind::Planter,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::Seed, 1),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 5: Hunt
            id: 5,
            task: TaskKind::Hunter,
            target: StepTarget::HuntPrey,
            preconditions: StepPreconditions::needs_good(Good::Weapon, 1),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 6: CollectFood
            id: 6,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestEdible,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 7: (unused — reserved for future use)
            id: 7,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 8: (unused — reserved for future use)
            id: 8,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 9: Eat — consume edibles from inventory in place. Gated on hunger
            // so plans don't waste food when the agent is already sated.
            id: 9,
            task: TaskKind::Eat,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::eat_when_hungry(EAT_TRIGGER_HUNGER),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 10: WithdrawFood — pull one edible from a faction storage tile
            id: 10,
            task: TaskKind::WithdrawFood,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 11: TameAnimal — work adjacent to a wild horse for ~100 ticks
            id: 11,
            task: TaskKind::TameAnimal,
            target: StepTarget::NearestWildHorse,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            extra: 0,
        },
        // ── Crafting steps ────────────────────────────────────────────────────
        StepDef {
            // 12: DepositGoods — deposit crafted items at faction storage
            id: 12,
            task: TaskKind::DepositResource,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::carry_anything(),
            reward_scale: 0.1,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 13: CollectSkin — pick up Skin from ground (after hunting)
            id: 13,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Skin),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        // 14-23: legacy per-recipe Craft steps. Replaced by the order-driven
        // pipeline (steps 38-40); kept as Idle placeholders so existing
        // step-id references in any in-flight ActivePlan don't panic on lookup.
        StepDef {
            id: 14,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 15,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 16,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 17,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 18,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 19,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 20,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 21,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 22,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            id: 23,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 24: (unused — reserved for future use)
            id: 24,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 25: BuildAnyBlueprint — finds the nearest accessible blueprint of any kind
            // and contributes wood + labor. Requirements come from the blueprint itself.
            id: 25,
            task: TaskKind::Construct,
            target: StepTarget::NearestAnyBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 26: (unused — formerly FindMate; reproduction is now passive via co-sleeping)
            id: 26,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 27: EngageRescue — route to the attacker stored on RescueTarget and engage.
            // CombatTarget is already set by respond_to_distress_system; combat_system
            // takes over as soon as the responder is adjacent.
            id: 27,
            task: TaskKind::Defend,
            target: StepTarget::RescueAttacker,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 28: HaulToBlueprint — carry currently-held materials to the nearest
            // blueprint that still needs them and drop them in. Excess stays in
            // the hauler's inventory; the step ends as soon as the drop is applied.
            id: 28,
            task: TaskKind::HaulMaterials,
            target: StepTarget::NearestBlueprintNeedingHeldMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 29: PlayWithPartner — route to the nearest play partner and
            // recreate together. play_system handles tick-by-tick willpower
            // refill, social fill, and bilateral affinity.
            id: 29,
            task: TaskKind::Play,
            target: StepTarget::NearestPlayPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 30: PlayWithItem — solo play. Resolves to the agent's tile if
            // they already hold an entertaining good, else the nearest ground
            // item with non-zero entertainment_value.
            id: 30,
            task: TaskKind::Play,
            target: StepTarget::NearestPlayItem,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 31: Explore — walk to a random reachable tile near home.
            // Used by the Explore plan as the NN's "no good options right now"
            // choice; reward_scale is intentionally low so the network reaches
            // for it only when other plans score worse.
            id: 31,
            task: TaskKind::Explore,
            target: StepTarget::ExploreTile,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.05,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 32: FetchMaterialFromStorage — route to the nearest faction
            // storage tile that holds a good currently needed by an unsatisfied
            // blueprint and pull the most-deficient material into the agent's
            // inventory. Pairs with step 28 (HaulToBlueprint) so stockpiled
            // wood/stone can ferry into in-progress build sites without a
            // fresh gather wave.
            id: 32,
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::WithdrawForFactionNeed {
                need: MaterialNeed::Blueprint,
                selector: GoodSelector::MostDeficient,
            },
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 33: WithdrawSeed — pull one Seed from a faction storage tile so
            // the agent can plant it as recreation in step 36.
            id: 33,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Seed),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: Good::Seed as u8,
        },
        StepDef {
            // 34: WithdrawStone — pull one Stone from a faction storage tile so
            // the agent can throw it as recreation in step 37.
            id: 34,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: Good::Stone as u8,
        },
        StepDef {
            // 35: WithdrawPlayItem — pull one entertainment-valued good from a
            // faction storage tile (sentinel 255 in craft_recipe_id signals
            // "first item with entertainment_value > 0").
            id: 35,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithEntertainment,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 255,
        },
        StepDef {
            // 36: PlantSeedAsPlay — plant a held Seed on a grass tile as
            // recreation. Same effect as Planter (spawns Grain, Farming XP +
            // activity), plus a one-shot willpower burst on completion.
            id: 36,
            task: TaskKind::PlayPlant,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::Seed, 1),
            reward_scale: 0.6,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 37: ThrowRocksAsPlay — throw a held Stone as recreation. Consumes
            // one Stone, awards Combat XP + ActivityKind::Combat, bursts
            // willpower. Resolves to the agent's current tile (they throw in
            // place; the rock is consumed).
            id: 37,
            task: TaskKind::PlayThrow,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Stone, 1),
            reward_scale: 0.6,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 38: HaulToCraftOrder — drop currently-held materials into the
            // nearest CraftOrder that needs them. Sibling of step 28
            // (HaulToBlueprint) for the order pipeline.
            id: 38,
            task: TaskKind::HaulToCraftOrder,
            target: StepTarget::NearestCraftOrderNeedingHeldMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 39: WorkOnCraftOrder — adjacent to a satisfied CraftOrder,
            // advance work_progress until the recipe completes.
            id: 39,
            task: TaskKind::WorkOnCraftOrder,
            target: StepTarget::NearestSatisfiedCraftOrder,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 40: FetchCraftOrderMaterialFromStorage — withdraw the most-
            // deficient good across open CraftOrders from a faction storage
            // tile so it can be hauled to the order. The faction's open
            // orders drive the choice; the agent doesn't need a per-good
            // plan variant.
            id: 40,
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::WithdrawForFactionNeed {
                need: MaterialNeed::CraftOrder,
                selector: GoodSelector::MostDeficient,
            },
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 41: WithdrawClaimedHaulMaterial — withdraw the good named in the
            // agent's active JobClaim::Haul from the nearest faction storage
            // tile holding that good. Pairs with step 42
            // (HaulToClaimedBlueprint) to deliver storage stock into a
            // specific blueprint.
            id: 41,
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
            extra: 0,
        },
        StepDef {
            // 42: HaulToClaimedBlueprint — carry the agent's hand contents to
            // the specific blueprint named in the active JobClaim::Haul and
            // deposit. Credits the Haul posting via record_progress on success.
            id: 42,
            task: TaskKind::HaulMaterials,
            target: StepTarget::HaulClaimBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 43: BuildClaimedBlueprint — perform labor at the specific
            // blueprint named in the agent's JobClaim::Build. The resolver
            // gates on the blueprint being satisfied, so this never starts
            // before all materials are in.
            id: 43,
            task: TaskKind::Construct,
            target: StepTarget::BuildClaimBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 44: CollectWood — pick up loose Wood GroundItems left behind by
            // tree harvesting (`harvest_ground_drops`) or earlier spills.
            id: 44,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Wood),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 45: CollectStone — pick up loose Stone GroundItems on the world.
            id: 45,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 46: (unused — was per-good FetchWoodFromStorage; superseded by
            // step 40's MostDeficient selector). Kept as Idle so any in-flight
            // ActivePlan referencing this id falls through to the lookup
            // failure path rather than panicking.
            id: 46,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 47: (unused — was per-good FetchStoneFromStorage; superseded by
            // step 40's MostDeficient selector).
            id: 47,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
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
            id: 48,
            task: TaskKind::Socialize,
            target: StepTarget::NearestPlayPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 49: Walk to the home tile of the faction we're raiding (per
            // `FactionRegistry::raid_target`). Solo agents and peacetime
            // factions resolve to None and the plan aborts harmlessly.
            id: 49,
            task: TaskKind::Raid,
            target: StepTarget::FactionRaidTarget,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 50: Walk to faction camp and run TaskKind::Defend. Reuses
            // `FactionCamp` — the resolver returns home_tile.
            id: 50,
            task: TaskKind::Defend,
            target: StepTarget::FactionCamp,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 51: Walk to faction camp and run TaskKind::Lead.
            id: 51,
            task: TaskKind::Lead,
            target: StepTarget::FactionCamp,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 52: Hunter pulls a Spear (Good::Weapon) from faction storage.
            // Used by `AcquireHuntingSpear` plan; the plan-level `forbids_good`
            // precondition ensures armed hunters skip this entirely.
            id: 52,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Weapon),
            preconditions: StepPreconditions::forbids(Good::Weapon),
            reward_scale: 0.4,
            plant_filter: None,
            extra: Good::Weapon as u8,
        },
        StepDef {
            // 53: Walk adjacent to a fresh Corpse and attach it to the
            // hunter via `PersonAI.carried_corpse`. No-op for non-hunters.
            id: 53,
            task: TaskKind::PickUpCorpse,
            target: StepTarget::NearestFreshCorpse,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 54: Drag the carried corpse to the nearest hearth or faction
            // home tile. `corpse_follow_system` keeps the corpse Transform
            // glued to the hunter while they walk.
            id: 54,
            task: TaskKind::HaulCorpse,
            target: StepTarget::NearestButcherSite,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 55: Butcher the carried corpse in place — work_ticks then yield
            // Meat+Skin per `species_yield()` and despawn the corpse.
            id: 55,
            task: TaskKind::Butcher,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 56: Equip a Weapon (Spear) into the MainHand slot. Instant
            // in-place transfer from inventory or hands → Equipment.MainHand.
            // Used as the second step of `AcquireHuntingSpear` (plan 64) so a
            // hunter who fetched the spear actually wields it for combat
            // damage. The plan-level `forbids_good(Weapon)` check now also
            // counts the equipped slot, so the plan self-deselects after this
            // step completes.
            id: 56,
            task: TaskKind::Equip,
            target: StepTarget::EquipItem {
                slot: EquipmentSlot::MainHand,
                good: Good::Weapon,
            },
            preconditions: StepPreconditions::needs_good(Good::Weapon, 1),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 0,
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
            id: 57,
            task: TaskKind::HuntPartyMuster,
            target: StepTarget::HearthForHunt,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 58: Travel to the chief's chosen hunt area. Reuses the Explore
            // task's "walk to tile, idle on arrival" semantics — once the
            // hunter is on the area_tile, step 5's HuntPrey scan finds prey
            // within VIEW_RADIUS naturally.
            id: 58,
            task: TaskKind::Explore,
            target: StepTarget::HuntArea,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 59: Wander for prey. Used by the ScoutForPrey plan when the
            // chief has no prey near the home tile; the agent ranges out to
            // unmapped tiles and `vision_system` writes prey memory along
            // the way, which the chief's next decision cycle picks up.
            id: 59,
            task: TaskKind::Explore,
            target: StepTarget::ScoutForPrey,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.1,
            plant_filter: None,
            extra: 0,
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
            // Visibility/memory drive the score; STORAGE_FOOD is a soft brake
            // so foragers stop wandering out when the granary is already full
            // and let WithdrawAndEat handle hungry siblings instead.
            id: 0,
            name: "ForageFood",
            steps: PLAN_STEPS_0,
            state_weights: mk_weights(&[
                (SI_VIS_PLANT_FOOD, 1.0),
                (SI_MEM_FOOD, 0.4),
                (SI_HAS_FOOD, -0.3),
                (SI_STORAGE_FOOD, -0.4),
            ]),
            bias: 0.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Farming is a deferred-payoff plan; weight it on skill + season,
            // and let it fade as storage fills. Hunger weight removed — under
            // Survive the eating plans now dominate via bias, and under
            // GatherFood hunger is moderate anyway.
            id: 1,
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
            memory_target_kind: Some(MemoryKind::Food),
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: 2,
            name: "GatherWood",
            steps: PLAN_STEPS_2,
            state_weights: mk_weights(&[
                (SI_VIS_TREE, 1.0),
                (SI_MEM_WOOD, 0.4),
                (SI_HAS_WOOD, -0.3),
                (SI_STORAGE_WOOD, -0.5),
            ]),
            bias: 0.0,
            serves_goals: GATHER_WOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: 3,
            name: "GatherStone",
            steps: PLAN_STEPS_3,
            state_weights: mk_weights(&[
                (SI_VIS_STONE_TILE, 1.0),
                (SI_MEM_STONE, 0.4),
                (SI_HAS_STONE, -0.3),
                (SI_STORAGE_STONE, -0.5),
            ]),
            bias: 0.0,
            serves_goals: GATHER_STONE_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Stone),
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            // Withdraw a Seed from faction storage, then plant it on the nearest
            // Grass tile. No food memory needed — targets storage then terrain.
            // Scores high when seeds are stockpiled and food supply is low.
            id: 4,
            name: "PlantFromStorage",
            steps: PLAN_STEPS_4,
            state_weights: mk_weights(&[
                (SI_STORAGE_SEED, 1.0),
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
            id: 5,
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
        PlanDef {
            id: 6,
            name: "ScavengeFood",
            steps: PLAN_STEPS_6,
            state_weights: mk_weights(&[
                (SI_VIS_GROUND_FOOD, 1.5),
                (SI_HAS_FOOD, -0.3),
                (SI_STORAGE_FOOD, -0.3),
            ]),
            bias: 0.1,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
            flags: PF_SCAVENGE | PF_TARGETS_FOOD,
            requires_profession: None,
        },
        PlanDef {
            // Gather-then-build sibling: only worth picking when storage is
            // dry and the agent can actually see/remember wood. Otherwise
            // HaulFromStorageAndBuild (29) is the cheaper path.
            id: 7,
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
            id: 8,
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
            // Plan 9 used to weight SI_HUNGER=1.5 — circular under Survive,
            // since hunger is what selected the goal. Now scores on whether
            // storage actually has food (the only viability question that
            // distinguishes this from EatFromInventory and ExploreForFood).
            id: 9,
            name: "WithdrawAndEat",
            steps: PLAN_STEPS_9,
            state_weights: mk_weights(&[
                (SI_STORAGE_FOOD, 1.5),
                (SI_IN_FACTION, 0.4),
                (SI_HAS_FOOD, -0.5),
            ]),
            bias: 0.3,
            serves_goals: SURVIVE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: 10,
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
            id: 11,
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
            id: 12,
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
            id: 13,
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
            id: 14,
            name: "DeliverGrainToCraftOrder",
            steps: PLAN_STEPS_14,
            state_weights: mk_weights(&[(SI_SKILL_FARMING, 0.5), (SI_SEASON_FOOD, 0.4)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        PlanDef {
            // Scores on SI_CRAFT_ORDER_NEEDS_MATERIAL (1.0 when any faction
            // CraftOrder has unmet deposits) so the plan only wins when hauling
            // work actually exists. The negative bias means that without an open
            // order the plan scores ≈ -0.6, which loses to WorkOnCraft (≈ 0.55)
            // and breaks the FailedNoTarget loop seen when no CraftOrders spawn.
            id: 15,
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
            id: 16,
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
            id: 17,
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
            id: 18,
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
            id: 19,
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
            id: 20,
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
            id: 21,
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
            id: 22,
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
            id: 23,
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
            id: 24,
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
        PlanDef {
            // Cheapest survive plan: if food is in hand, just eat. Bias 0.5
            // plus HAS_FOOD 2.0 wins decisively over WithdrawAndEat (~2.2)
            // and any food-gathering candidate when food is held.
            id: 25,
            name: "EatFromInventory",
            steps: PLAN_STEPS_25,
            state_weights: mk_weights(&[(SI_HAS_FOOD, 2.0)]),
            bias: 0.5,
            serves_goals: SURVIVE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_NONE,
            requires_profession: None,
        },
        PlanDef {
            id: 26,
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
            id: 27,
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
        // Explore is split per resource kind so that `explore_satisfaction_system`
        // can abort the wander the moment the agent records a sighting of the
        // target kind in memory. The candidate filter inverts the Food/Wood/Stone
        // gates for these IDs: each ExploreFor* plan is only available when the
        // agent has neither memory nor visibility of its target.
        PlanDef {
            id: 35,
            name: "ExploreForFood",
            steps: PLAN_STEPS_28,
            state_weights: mk_weights(&[]),
            bias: 0.3,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
            flags: PF_EXPLORE | PF_TARGETS_FOOD,
            requires_profession: None,
        },
        PlanDef {
            id: 36,
            name: "ExploreForWood",
            steps: PLAN_STEPS_28,
            state_weights: mk_weights(&[]),
            bias: 0.3,
            serves_goals: GATHER_WOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
            flags: PF_EXPLORE | PF_TARGETS_WOOD,
            requires_profession: None,
        },
        PlanDef {
            id: 37,
            name: "ExploreForStone",
            steps: PLAN_STEPS_28,
            state_weights: mk_weights(&[]),
            bias: 0.3,
            serves_goals: GATHER_STONE_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Stone),
            flags: PF_EXPLORE | PF_TARGETS_STONE,
            requires_profession: None,
        },
        PlanDef {
            // Sibling of BuildBlueprint that pulls materials out of communal
            // storage instead of gathering fresh from the world. Keeps in-progress
            // build sites moving once the initial gather wave has dropped its
            // surplus into granaries — without this plan, blueprints stall at
            // the "haulers can only deliver what they happen to be carrying"
            // step (NearestBlueprintNeedingHeldMaterial).
            id: 29,
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
            id: 30,
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
            id: 31,
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
            id: 32,
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
            // Claim-driven Haul plan. Fires only when the agent holds a
            // JobClaim::Haul (which gates the goal to AgentGoal::Haul). Step 41
            // withdraws the named good from the nearest storage tile; step 42
            // delivers it to the specific blueprint named in the claim.
            id: 33,
            name: "ClaimedHaul",
            steps: PLAN_STEPS_H,
            state_weights: mk_weights(&[]),
            bias: 1.0,
            serves_goals: HAUL_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_UNINTERRUPTIBLE,
            requires_profession: None,
        },
        PlanDef {
            // Claim-driven Build plan. Fires only when the agent holds a
            // JobClaim::Build (gating goal to AgentGoal::Build via job lock).
            // Step 43 routes to the claimed blueprint and labors there.
            id: 34,
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
        // ── Scavenge plans for loose wood/stone ───────────────────────────────
        // Sibling of ScavengeFood (plan 6). Workers pick up loose Wood/Stone
        // GroundItems left behind by tree/stone harvesting or prior spills,
        // then haul them to faction storage. Targets resolve via
        // `StepTarget::NearestItem`, so the plan only wins when an actual
        // GroundItem of the matching good is reachable.
        PlanDef {
            // Score is dominated by SI_VIS_GROUND_WOOD so the plan wins over
            // GatherWood (≈1.1 score) only when there's loose wood lying nearby
            // (≥1 hit → 0.375·1.5 ≈ 0.56; ≥4 hits → saturated at 1.5). With no
            // loose wood the score is 0 and GatherWood takes the goal.
            id: 38,
            name: "ScavengeWood",
            steps: PLAN_STEPS_SW,
            state_weights: mk_weights(&[
                (SI_VIS_GROUND_WOOD, 1.5),
                (SI_HAS_WOOD, -0.3),
                (SI_STORAGE_WOOD, -0.3),
            ]),
            bias: 0.1,
            serves_goals: GATHER_WOOD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_SCAVENGE | PF_TARGETS_WOOD,
            requires_profession: None,
        },
        PlanDef {
            id: 39,
            name: "ScavengeStone",
            steps: PLAN_STEPS_SS,
            state_weights: mk_weights(&[
                (SI_VIS_GROUND_STONE, 1.5),
                (SI_HAS_STONE, -0.3),
                (SI_STORAGE_STONE, -0.3),
            ]),
            bias: 0.1,
            serves_goals: GATHER_STONE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
            flags: PF_SCAVENGE | PF_TARGETS_STONE,
            requires_profession: None,
        },
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
            id: 60,
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
            id: 61,
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
            id: 62,
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
            id: 63,
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
            id: 64,
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
            id: 65,
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
    ];
}
