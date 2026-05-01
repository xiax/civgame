use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::carve::{carve_tile, STONE_PER_BLOCK};
use crate::simulation::construction::WallMap;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, MemoryKind};
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::plants::{GrowthStage, PlantKind, PlantMap, PlantSpriteIndex};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::ActivityKind;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::terrain::{tile_to_world, world_to_tile};
use crate::world::tile::TileKind;
use bevy::prelude::*;

// ── Stone tile harvest profile ────────────────────────────────────────────────
// Plants carry their own harvest data via PlantKind methods; stone uses this
// small inline struct until TileKind gets the same treatment.

struct StoneProfile {
    work_ticks: u8,
    base_yield_qty: u32,
    bonus_yields: &'static [(Good, u32, u8)], // (good, qty, percent_chance)
    xp: u32,
}

const STONE: StoneProfile = StoneProfile {
    work_ticks: 30,
    base_yield_qty: 2,
    bonus_yields: &[(Good::Coal, 1, 5), (Good::Iron, 1, 2)],
    xp: 2,
};

// ── gather_system ─────────────────────────────────────────────────────────────

pub fn gather_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut wall_map: ResMut<WallMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    clock: Res<SimClock>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut faction_registry: ResMut<FactionRegistry>,
    mut plant_query: Query<&mut crate::simulation::plants::Plant>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        &Transform,
        Option<&mut AgentMemory>,
        Option<&mut ActivePlan>,
        Option<&FactionMember>,
        &AgentGoal,
    )>,
) {
    for (
        mut ai,
        mut agent,
        mut carrier,
        mut skills,
        slot,
        lod,
        transform,
        mut memory_opt,
        mut plan_opt,
        faction_member,
        _goal,
    ) in agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if ai.task_id != TaskKind::Gather as u16 {
            continue;
        }

        let tx = ai.dest_tile.0 as i32;
        let ty = ai.dest_tile.1 as i32;

        let faction_id = faction_member
            .map(|fm| fm.faction_id)
            .filter(|&id| id != SOLO);

        if let Some(entity) = plant_map.0.get(&(tx, ty)).copied() {
            // ── Plant harvest ────────────────────────────────────────────────

            let Ok(mut plant) = plant_query.get_mut(entity) else {
                plant_map.0.remove(&(tx, ty));
                if let Some(ref mut mem) = memory_opt {
                    mem.forget((tx as i16, ty as i16), MemoryKind::Food);
                    mem.forget((tx as i16, ty as i16), MemoryKind::Wood);
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                continue;
            };
            if plant.stage != GrowthStage::Mature {
                if let Some(ref mut mem) = memory_opt {
                    let kind = match plant.kind {
                        PlantKind::BerryBush | PlantKind::Grain => MemoryKind::Food,
                        PlantKind::Tree => MemoryKind::Wood,
                    };
                    mem.forget((tx as i16, ty as i16), kind);
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                continue;
            }

            let kind = plant.kind;
            let has_tool = agent.has_tool();

            if ai.work_progress < kind.harvest_work_ticks() {
                continue;
            }
            ai.work_progress = 0;

            // Faction multipliers & activity log
            let (food_mul, wood_mul, _) =
                faction_muls(&mut faction_registry, faction_id, kind.harvest_activity());

            let (yield_good, base_qty) = kind.harvest_yield(has_tool);
            let yield_mul = if yield_good.is_edible() {
                food_mul
            } else if yield_good == Good::Wood {
                wood_mul
            } else {
                1.0
            };
            let qty = (base_qty as f32 * yield_mul).round().max(1.0) as u32;
            let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
            route_yield(
                &mut commands,
                &mut carrier,
                &mut agent,
                yield_good,
                qty,
                agent_tx,
                agent_ty,
            );

            for &(good, extra_qty) in kind.harvest_extra_yields() {
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    good,
                    extra_qty,
                    agent_tx,
                    agent_ty,
                );
            }

            let (skill, xp) = kind.harvest_skill_xp(has_tool);
            skills.gain_xp(skill, xp);

            if let Some(ref mut mem) = memory_opt {
                mem.record((tx as i16, ty as i16), kind.harvest_memory_kind());
            }
            if let Some(ref mut plan) = plan_opt {
                plan.reward_acc += qty as f32 * kind.harvest_reward_per_unit();
            }

            for &(drop_good, drop_qty) in kind.harvest_ground_drops(has_tool) {
                spawn_ground_drop(&mut commands, tx, ty, drop_good, drop_qty);
            }

            if kind.harvest_despawns(has_tool) {
                plant_map.0.remove(&(tx, ty));
                let cx = tx.div_euclid(CHUNK_SIZE as i32);
                let cy = ty.div_euclid(CHUNK_SIZE as i32);
                if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
                    vec.retain(|(e, _)| *e != entity);
                }
                commands.entity(entity).despawn_recursive();
            } else {
                plant.stage = GrowthStage::Harvested;
                plant.growth_ticks = 0;
            }
        } else {
            // ── Tile harvest (stone / wall) ───────────────────────────────────

            let tile_kind = chunk_map.tile_kind_at(tx, ty);

            if tile_kind == Some(TileKind::Wall) {
                // ── Wall mining: open the target column at the agent's foot Z.
                // For a tunnel into a hillside, this carves the headspace tile
                // and reveals the floor below. For a flat Wall on flat ground,
                // it just converts Wall → Dirt with no headspace change.
                if ai.work_progress < STONE.work_ticks {
                    continue;
                }
                ai.work_progress = 0;

                let target_floor_z = ai.current_z as i32;

                let blocks = carve_tile(&mut chunk_map, tx, ty, target_floor_z, &mut tile_changed);
                let stone_yield = (blocks * STONE_PER_BLOCK).max(STONE.base_yield_qty);
                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    Good::Stone,
                    stone_yield,
                    agent_tx,
                    agent_ty,
                );
                skills.gain_xp(SkillKind::Mining, STONE.xp);

                // Despawn the Wall entity only if the column no longer has
                // any solid tile at or above the carved Z (i.e. the visible
                // wall is fully gone). For now: if surface_z dropped below
                // the carved head, the wall column is open; otherwise rock
                // remains above as ceiling and we keep the Wall entity in
                // its place visually until rendering rework in Phase 6.
                if chunk_map.surface_z_at(tx, ty) < target_floor_z + 1 {
                    if let Some(wall_entity) = wall_map.0.remove(&ai.dest_tile) {
                        commands.entity(wall_entity).despawn_recursive();
                    }
                }

                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += stone_yield as f32 * 0.3;
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                ai.work_progress = 0;
            } else if tile_kind == Some(TileKind::Stone) {
                if ai.work_progress < STONE.work_ticks {
                    continue;
                }
                ai.work_progress = 0;

                let target_floor_z = ai.current_z as i32;

                let (_, _, stone_mul) =
                    faction_muls(&mut faction_registry, faction_id, ActivityKind::StoneMining);
                let blocks = carve_tile(&mut chunk_map, tx, ty, target_floor_z, &mut tile_changed);
                let base = (blocks * STONE_PER_BLOCK).max(STONE.base_yield_qty);
                let qty = (base as f32 * stone_mul).round().max(1.0) as u32;
                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    Good::Stone,
                    qty,
                    agent_tx,
                    agent_ty,
                );

                for &(good, bonus_qty, chance) in STONE.bonus_yields {
                    if fastrand::u8(..100) < chance {
                        route_yield(
                            &mut commands,
                            &mut carrier,
                            &mut agent,
                            good,
                            bonus_qty,
                            agent_tx,
                            agent_ty,
                        );
                        if let Some(id) = faction_id {
                            if let Some(fd) = faction_registry.factions.get_mut(&id) {
                                let act = match good {
                                    Good::Coal => Some(ActivityKind::CoalMining),
                                    Good::Iron => Some(ActivityKind::IronMining),
                                    _ => None,
                                };
                                if let Some(a) = act {
                                    fd.activity_log.increment(a);
                                }
                            }
                        }
                    }
                }

                skills.gain_xp(SkillKind::Mining, STONE.xp);

                if let Some(ref mut mem) = memory_opt {
                    mem.record((tx as i16, ty as i16), MemoryKind::Stone);
                }
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += qty as f32 * 0.3;
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                ai.work_progress = 0;
            } else {
                // Not a stone/wall tile and not a plant -> target is invalid or already harvested
                if let Some(ref mut mem) = memory_opt {
                    mem.forget((tx as i16, ty as i16), MemoryKind::Stone);
                    mem.forget((tx as i16, ty as i16), MemoryKind::Food);
                    mem.forget((tx as i16, ty as i16), MemoryKind::Wood);
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                ai.work_progress = 0;
            }
        }

        // ── Hands at haul cap → end gather step so the plan advances to deposit ──

        if carrier.is_at_haul_cap() {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            ai.work_progress = 0;
        }
    }
}

/// Pick up `qty` of `good` into the carrier; spill any leftover at `(tx, ty)` as a GroundItem.
/// Light "personal" goods (Tools, Seeds when farmer-eligible) are not routed here — those
/// go through the inventory path during Scavenge or production. Gathering loads always go
/// to hands first.
fn route_yield(
    commands: &mut Commands,
    carrier: &mut Carrier,
    _agent: &mut EconomicAgent,
    good: Good,
    qty: u32,
    tx: i32,
    ty: i32,
) {
    if qty == 0 {
        return;
    }
    let item = Item::new_commodity(good);
    let leftover = carrier.try_pick_up(item, qty);
    if leftover > 0 {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            GroundItem {
                item,
                qty: leftover,
            },
            Transform::from_xyz(pos.x, pos.y, 0.3),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ));
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn faction_muls(
    registry: &mut FactionRegistry,
    faction_id: Option<u32>,
    activity: ActivityKind,
) -> (f32, f32, f32) {
    if let Some(id) = faction_id {
        if let Some(fd) = registry.factions.get_mut(&id) {
            fd.activity_log.increment(activity);
            return (
                fd.food_yield_multiplier(),
                fd.wood_yield_multiplier(),
                fd.stone_yield_multiplier(),
            );
        }
    }
    (1.0, 1.0, 1.0)
}

fn spawn_ground_drop(commands: &mut Commands, tx: i32, ty: i32, good: Good, qty: u32) {
    let (dx, dy) = adjacent_offset();
    let pos = tile_to_world(tx + dx, ty + dy);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(good),
            qty,
        },
        Transform::from_xyz(pos.x, pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));
}

fn adjacent_offset() -> (i32, i32) {
    match fastrand::u8(..4) {
        0 => (1, 0),
        1 => (-1, 0),
        2 => (0, 1),
        _ => (0, -1),
    }
}
