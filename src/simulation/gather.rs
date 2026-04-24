use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::items::GroundItem;
use crate::simulation::jobs::JobKind;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, MemoryKind};
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::plants::{
    GrowthStage, PlantKind, PlantMap, PlantSpriteIndex, get_plant_texture,
};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::technology::ActivityKind;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::tile_to_world;
use crate::world::tile::TileKind;
use crate::rendering::pixel_art::EntityTextures;

// ── Stone tile harvest profile ────────────────────────────────────────────────
// Plants carry their own harvest data via PlantKind methods; stone uses this
// small inline struct until TileKind gets the same treatment.

struct StoneProfile {
    work_ticks:      u8,
    base_yield_qty:  u8,
    bonus_yields:    &'static [(Good, u8, u8)], // (good, qty, percent_chance)
    xp:              u8,
}

const STONE: StoneProfile = StoneProfile {
    work_ticks:     30,
    base_yield_qty:  2,
    bonus_yields:   &[(Good::Coal, 1, 5), (Good::Iron, 1, 2)],
    xp:              2,
};

// ── gather_system ─────────────────────────────────────────────────────────────

pub fn gather_system(
    mut commands: Commands,
    chunk_map:    Res<ChunkMap>,
    clock:        Res<SimClock>,
    textures:     Res<EntityTextures>,
    mut plant_map:          ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut faction_registry:   ResMut<FactionRegistry>,
    mut plant_query: Query<(&mut crate::simulation::plants::Plant, &mut Sprite)>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&mut AgentMemory>,
        Option<&mut ActivePlan>,
        Option<&FactionMember>,
    )>,
) {
    for (mut ai, mut agent, mut skills, slot, lod, mut memory_opt, mut plan_opt, faction_member) in
        agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if ai.state != AiState::Working { continue; }
        if ai.job_id != JobKind::Gather as u16 { continue; }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;

        let faction_id = faction_member
            .map(|fm| fm.faction_id)
            .filter(|&id| id != SOLO);

        if let Some(entity) = plant_map.0.get(&(tx, ty)).copied() {
            // ── Plant harvest ────────────────────────────────────────────────

            let Ok((mut plant, mut sprite)) = plant_query.get_mut(entity) else {
                plant_map.0.remove(&(tx, ty));
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                continue;
            };
            if plant.stage != GrowthStage::Mature {
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                continue;
            }

            let kind     = plant.kind;
            let has_tool = agent.has_tool();

            if ai.work_progress < kind.harvest_work_ticks() { continue; }
            ai.work_progress = 0;

            // Faction multipliers & activity log
            let (food_mul, wood_mul, _) = faction_muls(&mut faction_registry, faction_id, kind.harvest_activity());

            let (yield_good, base_qty) = kind.harvest_yield(has_tool);
            let yield_mul = match yield_good {
                Good::Food => food_mul,
                Good::Wood => wood_mul,
                _          => 1.0,
            };
            let qty = (base_qty as f32 * yield_mul).round().max(1.0) as u8;
            agent.add_good(yield_good, qty);

            for &(good, extra_qty) in kind.harvest_extra_yields() {
                agent.add_good(good, extra_qty);
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
                commands.entity(entity).despawn();
            } else {
                plant.stage = GrowthStage::Seedling;
                plant.growth_ticks = 0;
                sprite.image = get_plant_texture(&textures, kind, plant.stage);
            }

        } else {
            // ── Tile harvest (stone) ─────────────────────────────────────────

            let tile_kind = chunk_map.tile_kind_at(tx, ty);
            if tile_kind == Some(TileKind::Stone) {
                if ai.work_progress < STONE.work_ticks { continue; }
                ai.work_progress = 0;

                let (_, _, stone_mul) = faction_muls(&mut faction_registry, faction_id, ActivityKind::StoneMining);
                let qty = (STONE.base_yield_qty as f32 * stone_mul).round().max(1.0) as u8;
                agent.add_good(Good::Stone, qty);

                for &(good, bonus_qty, chance) in STONE.bonus_yields {
                    if fastrand::u8(..100) < chance {
                        agent.add_good(good, bonus_qty);
                        if let Some(id) = faction_id {
                            if let Some(fd) = faction_registry.factions.get_mut(&id) {
                                let act = match good {
                                    Good::Coal => Some(ActivityKind::CoalMining),
                                    Good::Iron => Some(ActivityKind::IronMining),
                                    _          => None,
                                };
                                if let Some(a) = act { fd.activity_log.increment(a); }
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
            } else {
                // Not a stone tile and not a plant -> target is invalid or already harvested
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
            }
        }

        // ── Inventory full → go idle ──────────────────────────────────────────

        if agent.is_inventory_full() {
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
        }
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
            return (fd.food_yield_multiplier(), fd.wood_yield_multiplier(), fd.stone_yield_multiplier());
        }
    }
    (1.0, 1.0, 1.0)
}

fn spawn_ground_drop(commands: &mut Commands, tx: i32, ty: i32, good: Good, qty: u8) {
    let (dx, dy) = adjacent_offset();
    let pos = tile_to_world(tx + dx, ty + dy);
    commands.spawn((
        GroundItem { item: Item::new_commodity(good), qty },
        Transform::from_xyz(pos.x, pos.y - 8.0, 0.3),
        GlobalTransform::default(),
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
