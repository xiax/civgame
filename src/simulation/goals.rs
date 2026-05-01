use super::construction::{AutonomousBuildingToggle, Blueprint, BlueprintMap};
use super::faction::{FactionChief, FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, Drafted, PersonAI, PlayerOrder};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::simulation::animals::{Horse, Tamed};
use crate::simulation::items::{GroundItem, TargetItem};
use crate::simulation::plants::{GrowthStage, Plant, PlantMap};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::{
    BOW_AND_ARROW, BRONZE_WEAPONS, COPPER_TOOLS, FIRED_POTTERY, FIRE_MAKING, FLINT_KNAPPING,
    HORSE_TAMING, HUNTING_SPEAR, LOOM_WEAVING,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::TILE_SIZE;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

/// Bundles the storage-reachability lookup resources so `goal_update_system`
/// stays under Bevy's 16-param limit.
#[derive(SystemParam)]
pub struct StorageReachability<'w> {
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub storage_tile_map: Res<'w, StorageTileMap>,
}

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Personality {
    #[default]
    Gatherer = 0,
    Socialite = 1,
    Explorer = 2,
    Nurturer = 3,
    /// Prefers solitary play with items over playing with other agents.
    /// Loners get a small bonus from solo play and a heavily reduced bonus
    /// from social play.
    Loner = 4,
}

impl Personality {
    pub fn name(self) -> &'static str {
        match self {
            Personality::Gatherer => "Gatherer",
            Personality::Socialite => "Socialite",
            Personality::Explorer => "Explorer",
            Personality::Nurturer => "Nurturer",
            Personality::Loner => "Loner",
        }
    }

    pub fn random() -> Self {
        // 10% Loner; rest split evenly across the original four.
        let r = fastrand::u8(..10);
        if r == 0 {
            return Personality::Loner;
        }
        match fastrand::u8(..4) {
            0 => Personality::Gatherer,
            1 => Personality::Socialite,
            2 => Personality::Explorer,
            _ => Personality::Nurturer,
        }
    }
}

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgentGoal {
    #[default]
    GatherFood = 0,
    GatherWood = 1,
    GatherStone = 2,
    Survive = 3,
    ReturnCamp = 4,
    Socialize = 5,
    Raid = 7,
    Defend = 8,
    Sleep = 9,
    Build = 10,
    TameHorse = 11,
    Craft = 12,
    Lead = 13,
    Rescue = 14,
    /// Drained agents seek out a play partner or an item to play with to
    /// refill willpower.
    Play = 15,
}

impl AgentGoal {
    pub fn name(self) -> &'static str {
        match self {
            AgentGoal::GatherFood => "GatherFood",
            AgentGoal::GatherWood => "GatherWood",
            AgentGoal::GatherStone => "GatherStone",
            AgentGoal::Survive => "Survive",
            AgentGoal::ReturnCamp => "ReturnCamp",
            AgentGoal::Socialize => "Socialize",
            AgentGoal::Raid => "Raid",
            AgentGoal::Defend => "Defend",
            AgentGoal::Sleep => "Sleep",
            AgentGoal::Build => "Build",
            AgentGoal::TameHorse => "TameHorse",
            AgentGoal::Craft => "Craft",
            AgentGoal::Lead => "Lead",
            AgentGoal::Rescue => "Rescue",
            AgentGoal::Play => "Play",
        }
    }
}

/// Set on a responder by `sound::respond_to_distress_system` when they are recruited
/// to defend a faction-mate (or affinity-bonded ally). Carries the attacker plus
/// the attacker's last-known tile so the `RescueAlly` plan can route the responder
/// without re-querying the attacker's `Transform` (avoids borrow conflicts in
/// `plan_execution_system`). Refreshed on each distress event from the victim;
/// cleared when the attacker is dead/despawned or after a timeout.
#[derive(Component, Clone, Copy)]
pub struct RescueTarget {
    pub attacker: Entity,
    pub attacker_tile: (i16, i16),
    pub set_tick: u64,
}

#[derive(Component, Default)]
pub struct GoalReason(pub &'static str);

pub fn goal_update_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    calendar: Res<Calendar>,
    auto_build: Res<AutonomousBuildingToggle>,
    chunk_map: Res<ChunkMap>,
    storage: StorageReachability,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    item_query: Query<(), With<GroundItem>>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    wild_horse_q: Query<(), (With<Horse>, Without<Tamed>)>,
    rescue_q: Query<&RescueTarget>,
    attacker_alive_q: Query<
        Entity,
        Or<(
            With<crate::simulation::combat::Health>,
            With<crate::simulation::combat::Body>,
        )>,
    >,
    mut query: Query<
        (
            Entity,
            &mut AgentGoal,
            &Needs,
            &Personality,
            &EconomicAgent,
            &mut PersonAI,
            &BucketSlot,
            &LodLevel,
            &FactionMember,
            &Transform,
            &mut TargetItem,
            Option<&mut GoalReason>,
            Option<&FactionChief>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    for (
        entity,
        mut goal,
        needs,
        personality,
        agent,
        mut ai,
        slot,
        lod,
        member,
        transform,
        mut target_item,
        reason_opt,
        chief_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        // Unemployed agents need immediate goal re-evaluation (e.g. just finished a deposit).
        // Bucket-gate only agents that are actively working a task.
        if ai.task_id != PersonAI::UNEMPLOYED && !clock.is_active(slot.0) {
            continue;
        }

        // 1. Cooldown & Staggered Update Fix
        if ai.task_id != PersonAI::UNEMPLOYED
            && clock.tick.saturating_sub(ai.last_goal_eval_tick) < 32
        {
            continue;
        }
        ai.last_goal_eval_tick = clock.tick;

        // 2. Target Validation (for moving agents)
        if matches!(ai.state, AiState::Routing | AiState::Seeking) {
            let mut invalid = false;
            let tid = ai.task_id;

            if tid == TaskKind::Gather as u16 {
                // If targeting a plant, check if it still exists and is mature
                if let Some(ent) = ai.target_entity {
                    if let Ok(plant) = plant_query.get(ent) {
                        if plant.stage != GrowthStage::Mature {
                            invalid = true;
                        }
                    } else {
                        invalid = true;
                    }
                } else {
                    // Targeting a tile (Stone)
                    let tx = ai.dest_tile.0 as i32;
                    let ty = ai.dest_tile.1 as i32;
                    if !matches!(
                        chunk_map.tile_kind_at(tx, ty),
                        Some(crate::world::tile::TileKind::Stone)
                    ) {
                        invalid = true;
                    }
                }
            } else if tid == TaskKind::Planter as u16 {
                let tx = ai.dest_tile.0 as i32;
                let ty = ai.dest_tile.1 as i32;
                if plant_map.0.contains_key(&(tx, ty)) {
                    invalid = true;
                }
            } else if tid == TaskKind::Scavenge as u16 {
                if let Some(ent) = ai.target_entity {
                    if item_query.get(ent).is_err() {
                        invalid = true;
                    }
                } else {
                    invalid = true;
                }
            } else if tid == TaskKind::Construct as u16
                || tid == TaskKind::ConstructBed as u16
                || tid == TaskKind::HaulMaterials as u16
            {
                // Invalidate if the target blueprint entity no longer exists.
                match ai.target_entity {
                    Some(ent) if bp_query.get(ent).is_err() => {
                        invalid = true;
                    }
                    None => {
                        invalid = true;
                    }
                    _ => {}
                }
            }

            if invalid {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                target_item.0 = None;
            }
        }

        // Rescue override: if a distress responder still has a live attacker target,
        // hold the Rescue goal until they engage / it dies / or it times out.
        if let Ok(rt) = rescue_q.get(entity) {
            let attacker_alive = attacker_alive_q.get(rt.attacker).is_ok();
            let timed_out = clock.tick.saturating_sub(rt.set_tick) > 200;
            if attacker_alive && !timed_out {
                if *goal != AgentGoal::Rescue {
                    *goal = AgentGoal::Rescue;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Helping Ally";
                } else {
                    commands.entity(entity).insert(GoalReason("Helping Ally"));
                }
                continue;
            } else {
                // Attacker is dead or rescue timed out — drop the marker so the
                // agent re-evaluates a normal goal next tick.
                commands.entity(entity).remove::<RescueTarget>();
            }
        }

        // Don't interrupt combat or sleep
        if matches!(ai.state, AiState::Attacking | AiState::Sleeping) {
            continue;
        }

        // Faction war state overrides individual needs
        if member.faction_id != SOLO {
            if registry.is_under_raid(member.faction_id) {
                if *goal != AgentGoal::Defend {
                    *goal = AgentGoal::Defend;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Under Raid";
                } else {
                    commands.entity(entity).insert(GoalReason("Under Raid"));
                }
                continue;
            }
            if registry.raid_target(member.faction_id).is_some() && needs.hunger < 120.0 {
                if *goal != AgentGoal::Raid {
                    *goal = AgentGoal::Raid;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Participating in Raid";
                } else {
                    commands
                        .entity(entity)
                        .insert(GoalReason("Participating in Raid"));
                }
                continue;
            }
        }

        // Chief override: tribal chiefs lead when not in crisis or at war.
        if chief_opt.is_some()
            && member.faction_id != SOLO
            && !registry.is_under_raid(member.faction_id)
            && registry.raid_target(member.faction_id).is_none()
            && needs.hunger < 150.0
            && needs.sleep < 170.0
        {
            if *goal != AgentGoal::Lead {
                *goal = AgentGoal::Lead;
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            if let Some(mut r) = reason_opt {
                r.0 = "Leading";
            } else {
                commands.entity(entity).insert(GoalReason("Leading"));
            }
            continue;
        }

        let social_threshold = if *personality == Personality::Socialite {
            120.0
        } else {
            160.0
        };

        // Loners get drained slightly faster (less social refill) and trigger
        // play earlier; everyone else waits until willpower is genuinely low.
        let play_threshold = if *personality == Personality::Loner {
            100.0
        } else {
            80.0
        };

        let has_horse_taming = member.faction_id != SOLO
            && registry
                .factions
                .get(&member.faction_id)
                .map(|f| f.techs.has(HORSE_TAMING))
                .unwrap_or(false)
            && !wild_horse_q.is_empty();

        let (faction_food_ratio, can_return_camp) = if member.faction_id != SOLO {
            let per_member: f32 = match calendar.season {
                Season::Summer => 30.0,
                Season::Autumn => 25.0,
                Season::Spring => 15.0,
                Season::Winter => 5.0,
            };
            let cap = registry
                .factions
                .get(&member.faction_id)
                .map(|f| f.member_count as f32 * per_member)
                .unwrap_or(0.0);

            let stock = registry.food_stock(member.faction_id);
            let ratio = if cap > 0.0 { stock / cap } else { 1.0 };
            // Gate ReturnCamp on storage reachability so agents don't gather
            // food destined for storage they can't actually reach.
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let storage_reachable = storage
                .storage_tile_map
                .nearest_for_faction(member.faction_id, (cur_tx, cur_ty))
                .map(|(stx, sty)| {
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );
                    let storage_chunk = ChunkCoord(
                        (stx as i32).div_euclid(CHUNK_SIZE as i32),
                        (sty as i32).div_euclid(CHUNK_SIZE as i32),
                    );
                    cur_chunk == storage_chunk
                        || storage
                            .chunk_router
                            .first_waypoint(
                                &storage.chunk_graph,
                                cur_chunk,
                                storage_chunk,
                                ai.current_z,
                            )
                            .is_some()
                })
                .unwrap_or(false);
            (ratio, stock < cap && storage_reachable)
        } else {
            (1.0, false)
        };

        let faction_has_food =
            member.faction_id != SOLO && registry.food_stock(member.faction_id) >= 1.0;
        let is_starving = needs.hunger > 120.0 && agent.total_food() == 0;

        let has_build_site = auto_build.0
            && bp_map.0.values().any(|&bp_e| {
                bp_query
                    .get(bp_e)
                    .map(|bp| {
                        bp.personal_owner == Some(entity)
                            || (member.faction_id != SOLO
                                && bp.faction_id == member.faction_id
                                && bp.personal_owner.is_none())
                    })
                    .unwrap_or(false)
            });

        // Stable probabilistic selection: agents with high hash_val will gather if food ratio is low.
        let agent_hash_val = ((entity.index() as u64 * 2654435761) % 100) as f32 / 100.0;
        let prioritize_food = faction_food_ratio < 1.0 && agent_hash_val > faction_food_ratio;

        let mut gather_goal = AgentGoal::GatherFood;
        let mut gather_reason = "General Gathering (Food)";

        if prioritize_food {
            gather_goal = AgentGoal::GatherFood;
            gather_reason = "Prioritized Gathering (Food Low)";
        } else if member.faction_id != SOLO {
            if let Some(faction) = registry.factions.get(&member.faction_id) {
                let wood_demand = faction
                    .resource_demand
                    .get(&Good::Wood)
                    .copied()
                    .unwrap_or(0);
                let wood_supply = faction
                    .resource_supply
                    .get(&Good::Wood)
                    .copied()
                    .unwrap_or(0);
                let stone_demand = faction
                    .resource_demand
                    .get(&Good::Stone)
                    .copied()
                    .unwrap_or(0);
                let stone_supply = faction
                    .resource_supply
                    .get(&Good::Stone)
                    .copied()
                    .unwrap_or(0);

                let wood_deficit = wood_demand.saturating_sub(wood_supply);
                let stone_deficit = stone_demand.saturating_sub(stone_supply);

                if wood_deficit > 0 && wood_deficit >= stone_deficit {
                    gather_goal = AgentGoal::GatherWood;
                    gather_reason = "Gathering Wood for Blueprints";
                } else if stone_deficit > 0 {
                    gather_goal = AgentGoal::GatherStone;
                    gather_reason = "Gathering Stone for Blueprints";
                }
            }
        }

        let (new_goal, reason) = if is_starving && faction_has_food {
            (AgentGoal::Survive, "Starving (Faction has food)")
        } else if needs.hunger > 200.0 && agent.total_food() == 0 {
            (AgentGoal::Survive, "Very Hungry")
        } else if needs.hunger > 180.0 && agent.total_food() > 0 {
            (AgentGoal::Survive, "Hungry (Eating)")
        } else if agent.total_food() >= 3 && can_return_camp {
            (AgentGoal::ReturnCamp, "Returning Surplus Food")
        } else if needs.hunger > 150.0 && agent.total_food() == 0 {
            (AgentGoal::Survive, "Hungry")
        } else if needs.sleep > 180.0 {
            (AgentGoal::Sleep, "Tired")
        } else if prioritize_food {
            (gather_goal, gather_reason)
        } else if has_horse_taming {
            (AgentGoal::TameHorse, "Taming Horse")
        } else if needs.social > social_threshold {
            (AgentGoal::Socialize, "Social Need")
        } else if needs.willpower < play_threshold {
            (AgentGoal::Play, "Low Willpower")
        } else if has_build_site {
            (AgentGoal::Build, "Building Projects")
        } else if should_craft(&registry, member.faction_id, needs) {
            (AgentGoal::Craft, "Crafting for Faction")
        } else {
            (gather_goal, gather_reason)
        };

        if *goal != new_goal {
            *goal = new_goal;
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        }
        if let Some(mut r) = reason_opt {
            r.0 = reason;
        } else {
            commands.entity(entity).insert(GoalReason(reason));
        }
    }
}

/// Returns true when a faction agent should switch to crafting.
/// Triggers when the faction has at least one craft tech unlocked and is short on
/// crafted goods (Tools + Weapon + Armor + Shield + Cloth < member_count / 3).
fn should_craft(registry: &FactionRegistry, faction_id: u32, needs: &Needs) -> bool {
    if faction_id == SOLO {
        return false;
    }
    // Only craft when not hungry or tired
    if needs.hunger > 100.0 || needs.sleep > 100.0 {
        return false;
    }
    let Some(faction) = registry.factions.get(&faction_id) else {
        return false;
    };
    let has_craft_tech = faction.techs.has(FLINT_KNAPPING)
        || faction.techs.has(HUNTING_SPEAR)
        || faction.techs.has(FIRE_MAKING)
        || faction.techs.has(BOW_AND_ARROW)
        || faction.techs.has(LOOM_WEAVING)
        || faction.techs.has(FIRED_POTTERY)
        || faction.techs.has(COPPER_TOOLS)
        || faction.techs.has(BRONZE_WEAPONS);
    if !has_craft_tech {
        return false;
    }
    let crafted_total: u32 = [
        Good::Tools,
        Good::Weapon,
        Good::Armor,
        Good::Shield,
        Good::Cloth,
    ]
    .iter()
    .map(|g| faction.storage.totals.get(g).copied().unwrap_or(0))
    .sum();
    crafted_total < faction.member_count.saturating_div(3).max(1)
}
