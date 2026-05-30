//! Visible water currents (Phase 3 of `plans/swimming.md`).
//!
//! Spawns an animated flow-streak sprite on each flowing-river tile: a
//! short bright dash that scrolls downstream along the current direction
//! and fades at the ends, so a river visibly *moves*. Streaks are indexed
//! per chunk and reconciled against the `WaterCurrentField` every frame —
//! robust to the spawn-area chunks that pre-load without a
//! `ChunkLoadedEvent`.

use crate::collections::AHashMap;
use bevy::prelude::*;

use crate::rendering::projection::{ProjectedAnchor, ProjectionState};
use crate::world::chunk::{ChunkCoord, ChunkMap, Z_MAX, Z_MIN};
use crate::world::terrain::TILE_SIZE;
use crate::world::water_current::{CurrentSource, WaterCurrentField};

/// Animation data for one flow-streak sprite.
#[derive(Component)]
pub struct CurrentStreak {
    /// Logical tile-centre world position the streak scrolls around.
    base: Vec2,
    /// Unit flow direction.
    dir: Vec2,
    /// Flow speed `0..1` — scales the scroll rate.
    speed: f32,
    /// Per-tile phase offset so neighbouring streaks aren't synchronised.
    phase_offset: f32,
}

/// Per-chunk index of spawned streak entities.
#[derive(Resource, Default)]
pub struct CurrentStreakIndex {
    by_chunk: AHashMap<ChunkCoord, Vec<Entity>>,
}

/// How far (in tiles) a streak travels over one scroll cycle.
const STREAK_TRAVEL: f32 = 0.85;
/// Base scroll cycles per second (scaled up by current speed).
const STREAK_SCROLL_RATE: f32 = 0.45;
/// Peak streak alpha (mid-cycle); fades to 0 at the cycle ends.
const STREAK_MAX_ALPHA: f32 = 0.7;

/// Reconcile flow-streak sprites against the `WaterCurrentField`: drop
/// streaks for unloaded chunks, and (re)spawn every chunk the field
/// marked dirty (initial build + neighbour-rebuild edge-tangent shifts).
pub fn water_current_render_system(
    mut commands: Commands,
    mut field: ResMut<WaterCurrentField>,
    chunk_map: Res<ChunkMap>,
    mut index: ResMut<CurrentStreakIndex>,
) {
    // Despawn streaks for chunks no longer in the field (unloaded).
    let stale: Vec<ChunkCoord> = index
        .by_chunk
        .keys()
        .filter(|c| field.chunk_tiles(**c).is_none())
        .copied()
        .collect();
    for coord in stale {
        despawn_chunk_streaks(&mut commands, &mut index, coord);
    }
    // (Re)spawn every dirty chunk.
    for coord in field.take_dirty() {
        despawn_chunk_streaks(&mut commands, &mut index, coord);
        let Some(tiles) = field.chunk_tiles(coord) else {
            continue;
        };
        let mut streaks: Vec<Entity> = Vec::new();
        for &(tx, ty) in tiles {
            let Some(cur) = field.at(tx, ty) else {
                continue;
            };
            if cur.source != CurrentSource::RiverChannel || cur.dir == Vec2::ZERO {
                continue;
            }
            let base = Vec2::new(
                tx as f32 * TILE_SIZE + TILE_SIZE * 0.5,
                ty as f32 * TILE_SIZE + TILE_SIZE * 0.5,
            );
            let angle = cur.dir.y.atan2(cur.dir.x);
            let len = TILE_SIZE * (0.3 + 0.35 * cur.speed);
            let surf_z = chunk_map.surface_z_at(tx, ty).clamp(Z_MIN, Z_MAX) as i8;
            // Deterministic per-tile phase so a river reads as continuous
            // flow rather than every dash pulsing in lockstep.
            let phase_offset = ((tx.wrapping_mul(7) ^ ty.wrapping_mul(13)) as f32 * 0.131).fract();
            let e = commands
                .spawn((
                    CurrentStreak {
                        base,
                        dir: cur.dir,
                        speed: cur.speed,
                        phase_offset: phase_offset.abs(),
                    },
                    Sprite::from_color(
                        Color::srgba(0.92, 0.98, 1.0, 0.0),
                        Vec2::new(len, 2.5),
                    ),
                    Transform::from_xyz(base.x, base.y, 0.35)
                        .with_rotation(Quat::from_rotation_z(angle)),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ProjectedAnchor::Static { z: surf_z },
                    ProjectionState::default(),
                ))
                .id();
            streaks.push(e);
        }
        if !streaks.is_empty() {
            index.by_chunk.insert(coord, streaks);
        }
    }
}

fn despawn_chunk_streaks(
    commands: &mut Commands,
    index: &mut CurrentStreakIndex,
    coord: ChunkCoord,
) {
    if let Some(streaks) = index.by_chunk.remove(&coord) {
        for e in streaks {
            if let Some(ec) = commands.get_entity(e) {
                ec.despawn_recursive();
            }
        }
    }
}

/// Scroll each streak downstream and fade it at the cycle ends so the
/// river visibly flows.
pub fn animate_current_streaks_system(
    time: Res<Time>,
    mut q: Query<(&CurrentStreak, &mut Transform, &mut Sprite)>,
) {
    let t = time.elapsed_secs();
    for (streak, mut transform, mut sprite) in q.iter_mut() {
        let rate = STREAK_SCROLL_RATE * (0.4 + 0.6 * streak.speed);
        let phase = (t * rate + streak.phase_offset).fract();
        // Slide from −half-travel to +half-travel along the flow.
        let offset = streak.dir * ((phase - 0.5) * STREAK_TRAVEL * TILE_SIZE);
        transform.translation.x = streak.base.x + offset.x;
        transform.translation.y = streak.base.y + offset.y;
        // Triangle-ish fade: invisible at the ends, brightest mid-cycle.
        let alpha = (phase * std::f32::consts::PI).sin().max(0.0) * STREAK_MAX_ALPHA;
        sprite.color.set_alpha(alpha);
    }
}
