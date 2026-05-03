use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::carve::carve_tile;
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
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, world_to_tile, WorldGen};
use crate::world::tile::TileKind;
use bevy::prelude::*;

// ── Stone / ore tile harvest profile ──────────────────────────────────────────
// Coal/Iron and the new ores (Copper/Tin/Gold/Silver) are no longer random
// rolls on Stone tiles — they're real Ore tiles produced by `proc_tile`'s
// stratification model. `carve_tile` returns the per-block (Good, qty) drop.

struct StoneProfile {
    work_ticks: u8,
    base_yield_qty: u32,
    xp: u32,
}

const STONE: StoneProfile = StoneProfile {
    work_ticks: 30,
    base_yield_qty: 2,
    xp: 2,
};

/// Activity bucket to credit when a particular `Good` was just mined.
fn mining_activity(good: Good) -> Option<ActivityKind> {
    match good {
        Good::Stone => Some(ActivityKind::StoneMining),
        Good::Coal => Some(ActivityKind::CoalMining),
        Good::Iron => Some(ActivityKind::IronMining),
        Good::Copper => Some(ActivityKind::CopperMining),
        Good::Tin => Some(ActivityKind::TinMining),
        Good::Gold => Some(ActivityKind::GoldMining),
        Good::Silver => Some(ActivityKind::SilverMining),
        _ => None,
    }
}

// ── gather_system ─────────────────────────────────────────────────────────────

pub fn gather_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut wall_map: ResMut<WallMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    clock: Res<SimClock>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
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
                    mem.forget((tx as i32, ty as i32), MemoryKind::Food);
                    mem.forget((tx as i32, ty as i32), MemoryKind::Wood);
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
                    mem.forget((tx as i32, ty as i32), kind);
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
                mem.record((tx as i32, ty as i32), kind.harvest_memory_kind());
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

            if matches!(
                tile_kind,
                Some(TileKind::Wall) | Some(TileKind::Stone) | Some(TileKind::Ore)
            ) {
                // ── Mineable rock: Wall, Stone, or Ore.
                // Same carve operation; per-block (Good, qty) drops come from
                // `carve_tile` which reads the actual material via tile_at_3d.
                if ai.work_progress < STONE.work_ticks {
                    continue;
                }
                ai.work_progress = 0;

                let target_floor_z = ai.current_z as i32;
                let was_wall = tile_kind == Some(TileKind::Wall);

                let drops = carve_tile(
                    &mut chunk_map,
                    &gen,
                    &globe,
                    tx,
                    ty,
                    target_floor_z,
                    &mut tile_changed,
                );

                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                let mut total_qty: u32 = 0;
                for (good, qty) in drops {
                    if qty == 0 {
                        continue;
                    }
                    let activity = mining_activity(good).unwrap_or(ActivityKind::StoneMining);
                    let (_, _, mul) = faction_muls(&mut faction_registry, faction_id, activity);
                    let scaled = (qty as f32 * mul).round().max(1.0) as u32;
                    total_qty = total_qty.saturating_add(scaled);
                    route_yield(
                        &mut commands,
                        &mut carrier,
                        &mut agent,
                        good,
                        scaled,
                        agent_tx,
                        agent_ty,
                    );
                    if let Some(id) = faction_id {
                        if let Some(fd) = faction_registry.factions.get_mut(&id) {
                            fd.activity_log.increment(activity);
                        }
                    }
                }

                if total_qty == 0 {
                    // Carved a non-yielding tile (e.g. Dirt headspace); credit the
                    // baseline so XP/effort isn't entirely wasted.
                    total_qty = STONE.base_yield_qty;
                }

                skills.gain_xp(SkillKind::Mining, STONE.xp);

                // Despawn the Wall entity only if the column no longer has
                // any solid tile at or above the carved Z (the visible wall
                // is fully gone).
                if was_wall && chunk_map.surface_z_at(tx, ty) < target_floor_z + 1 {
                    if let Some(wall_entity) = wall_map.0.remove(&ai.dest_tile) {
                        commands.entity(wall_entity).despawn_recursive();
                    }
                }

                if let Some(ref mut mem) = memory_opt {
                    mem.record((tx as i32, ty as i32), MemoryKind::Stone);
                }
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += total_qty as f32 * 0.3;
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                ai.work_progress = 0;
            } else {
                // Not a stone/wall tile and not a plant -> target is invalid or already harvested
                if let Some(ref mut mem) = memory_opt {
                    mem.forget((tx as i32, ty as i32), MemoryKind::Stone);
                    mem.forget((tx as i32, ty as i32), MemoryKind::Food);
                    mem.forget((tx as i32, ty as i32), MemoryKind::Wood);
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
