use ahash::AHashMap;
use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::jobs::JobKind;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::world::chunk::ChunkMap;
use crate::world::terrain::tile_to_world;
use crate::world::tile::{TileKind, TileData};

pub const TICKS_BUILD_WALL: u8 = 60;
pub const TICKS_BUILD_BED:  u8 = 80;

/// Global toggle: when false, agents skip the Build goal entirely.
#[derive(Resource)]
pub struct AutonomousBuildingToggle(pub bool);

/// Maps tile positions to the bed entity placed there.
#[derive(Resource, Default)]
pub struct BedMap(pub AHashMap<(i16, i16), Entity>);

/// Marker placed on bed entities.
#[derive(Component)]
pub struct Bed;

/// What kind of structure an agent is trying to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSiteKind {
    Wall,
    Bed,
}

/// Count how many of the 4 cardinal directions have a wall (or higher-z terrain)
/// within 3 tiles. Score range: 0–4.
pub fn enclosure_score(chunk_map: &ChunkMap, tx: i32, ty: i32) -> u8 {
    let agent_z = chunk_map.surface_z_at(tx, ty);
    let mut score = 0u8;
    for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)] {
        for step in 1..=3i32 {
            let nx = tx + dx * step;
            let ny = ty + dy * step;
            let kind_wall = chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall);
            let z_higher  = chunk_map.surface_z_at(nx, ny) > agent_z;
            if kind_wall || z_higher {
                score += 1;
                break;
            }
        }
    }
    score
}

/// Scan Chebyshev rings outward from `camp_home` and return the first passable
/// non-wall tile that is a valid place to build a wall.
pub fn find_wall_build_site(
    chunk_map:  &ChunkMap,
    camp_home:  (i16, i16),
    max_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);

    for ring_r in 1..=max_radius {
        for dy in -ring_r..=ring_r {
            for dx in -ring_r..=ring_r {
                if dx.abs().max(dy.abs()) != ring_r { continue; }

                let tx = hx + dx;
                let ty = hy + dy;

                let Some(kind) = chunk_map.tile_kind_at(tx, ty) else { continue };
                if !kind.is_passable() || kind == TileKind::Wall { continue; }

                // Rings 1-3: take any passable tile to establish a perimeter.
                if ring_r <= 3 {
                    return Some((tx as i16, ty as i16));
                }

                // Outer rings: only take tiles adjacent to an existing wall (gap-fill).
                let adj_wall = [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)].iter().any(|(ddx, ddy)| {
                    chunk_map.tile_kind_at(tx + ddx, ty + ddy) == Some(TileKind::Wall)
                });
                if adj_wall {
                    return Some((tx as i16, ty as i16));
                }
            }
        }
    }
    None
}

/// Find the closest passable tile within max_radius of camp_home that:
/// - already has enclosure_score >= 1 (some walls or terrain around it)
/// - has no bed placed yet
pub fn find_bed_build_site(
    chunk_map:  &ChunkMap,
    bed_map:    &BedMap,
    camp_home:  (i16, i16),
    max_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -max_radius..=max_radius {
        for dx in -max_radius..=max_radius {
            let tx = hx + dx;
            let ty = hy + dy;

            let Some(kind) = chunk_map.tile_kind_at(tx, ty) else { continue };
            if !kind.is_passable() { continue; }
            if bed_map.0.contains_key(&(tx as i16, ty as i16)) { continue; }

            if enclosure_score(chunk_map, tx, ty) >= 1 {
                let dist = dx.abs() + dy.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx as i16, ty as i16));
                }
            }
        }
    }
    best
}

/// Handles agents building walls (JobKind::Construct) and beds (JobKind::ConstructBed).
/// Runs in Sequential set after gather_system.
pub fn construction_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut bed_map:   ResMut<BedMap>,
    clock: Res<SimClock>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&mut ActivePlan>,
    )>,
) {
    for (mut ai, mut agent, mut skills, slot, lod, mut plan_opt) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if ai.state != AiState::Working { continue; }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;

        // ── Wall building ─────────────────────────────────────────────────────
        if ai.job_id == JobKind::Construct as u16 {
            // Abort if target tile is no longer valid (already walled by another agent)
            match chunk_map.tile_kind_at(tx, ty) {
                Some(k) if k.is_passable() => {}
                _ => {
                    ai.state = AiState::Idle;
                    ai.job_id = PersonAI::UNEMPLOYED;
                    ai.target_entity = None;
                    ai.work_progress = 0;
                    continue;
                }
            }

            if ai.work_progress < TICKS_BUILD_WALL { continue; }

            if agent.quantity_of(Good::Wood) < 2 {
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
                continue;
            }

            ai.work_progress = 0;
            agent.remove_good(Good::Wood, 2);
            skills.gain_xp(SkillKind::Building, 3);

            // Place wall one z-level above the floor so the floor tile remains
            // conceptually present (matches the "one z-level higher = wall" convention).
            let surf_z = chunk_map.surface_z_at(tx, ty);
            chunk_map.set_tile(tx, ty, surf_z + 1, TileData {
                kind:      TileKind::Wall,
                elevation: 0,
                fertility: 0,
                flags:     0b0001,  // has_building
            });

            if let Some(ref mut plan) = plan_opt {
                plan.reward_acc += 1.0;
            }

            ai.state  = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;

        // ── Bed building ──────────────────────────────────────────────────────
        } else if ai.job_id == JobKind::ConstructBed as u16 {
            // Abort if bed already placed here by another agent
            if bed_map.0.contains_key(&(tx as i16, ty as i16)) {
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
                continue;
            }
            match chunk_map.tile_kind_at(tx, ty) {
                Some(k) if k.is_passable() => {}
                _ => {
                    ai.state = AiState::Idle;
                    ai.job_id = PersonAI::UNEMPLOYED;
                    ai.target_entity = None;
                    ai.work_progress = 0;
                    continue;
                }
            }

            if ai.work_progress < TICKS_BUILD_BED { continue; }

            if agent.quantity_of(Good::Wood) < 3 {
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
                continue;
            }

            ai.work_progress = 0;
            agent.remove_good(Good::Wood, 3);
            skills.gain_xp(SkillKind::Building, 5);

            let world_pos = tile_to_world(tx, ty);
            let bed_entity = commands.spawn((
                Bed,
                Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            )).id();

            bed_map.0.insert((tx as i16, ty as i16), bed_entity);

            if let Some(ref mut plan) = plan_opt {
                plan.reward_acc += 2.0;
            }

            ai.state  = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
        }
    }
}
