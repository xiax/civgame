use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::rendering::camera::CameraViewZ;
use crate::rendering::color_map::{shaded_ore_tile_color, shaded_tile_color, z_bucket};
use crate::simulation::construction::{Wall, WallMap};
use crate::simulation::faction::{FactionMember, PlayerFaction, PlayerFactionMarker};
use crate::simulation::line_of_sight::has_vision_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::PersonAI;
use crate::simulation::vision::{ActiveLookout, CachedVisionSet};
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::{
    resolve_render_tile, TileMaterials, TileSprite, TileSpriteIndex, RENDERABLE_KINDS,
    RENDERABLE_ORES,
};
use crate::world::globe::Globe;
use crate::world::terrain::{WorldGen, TILE_SIZE};
use crate::world::tile::{OreKind, TileKind};

/// Fog of war state: which tiles the player faction can currently see / has ever seen.
#[derive(Resource, Default)]
pub struct FogMap {
    pub visible: AHashSet<(i32, i32)>,
    pub explored: AHashSet<(i32, i32)>,
    /// Tiles whose fog state changed this frame — processed by apply_fog_to_tiles_system.
    pub dirty_tiles: Vec<(i32, i32)>,
}

impl FogMap {
    pub fn is_visible(&self, pos: (i32, i32)) -> bool {
        self.visible.contains(&pos)
    }

    pub fn is_explored(&self, pos: (i32, i32)) -> bool {
        self.explored.contains(&pos)
    }
}

/// Darkened (35 % brightness) tile material variants for the
/// explored-but-not-currently-visible fog state.
#[derive(Resource, Default)]
pub struct FogTileMaterials {
    pub materials: AHashMap<(u8, u8, i32), Handle<ColorMaterial>>,
    pub tile_mesh: Handle<Mesh>,
}

impl FogTileMaterials {
    pub fn handle_for(&self, kind: TileKind, ore: OreKind, z: i32) -> Handle<ColorMaterial> {
        self.materials
            .get(&(kind as u8, ore as u8, z_bucket(z)))
            .cloned()
            .unwrap_or_default()
    }
}

/// PostStartup: create darkened tile material variants.
pub fn setup_fog_tile_materials(
    mut fog_materials: ResMut<FogTileMaterials>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    tile_materials: Res<TileMaterials>,
) {
    let bucket_min = (Z_MIN as i32).div_euclid(4);
    let bucket_max = 15_i32.div_euclid(4);

    fog_materials.tile_mesh = tile_materials.tile_mesh.clone();

    for &kind in RENDERABLE_KINDS {
        if kind == TileKind::Ore {
            for &ore in RENDERABLE_ORES {
                for bucket in bucket_min..=bucket_max {
                    let z = bucket * 4 + 2;
                    let base = shaded_ore_tile_color(ore, z).to_srgba();
                    let fog_color =
                        Color::srgb(base.red * 0.35, base.green * 0.35, base.blue * 0.35);
                    let handle = materials.add(ColorMaterial::from_color(fog_color));
                    fog_materials
                        .materials
                        .insert((kind as u8, ore as u8, bucket), handle);
                }
            }
            continue;
        }
        for bucket in bucket_min..=bucket_max {
            let z = bucket * 4 + 2;
            let base = shaded_tile_color(kind, z).to_srgba();
            let fog_color = Color::srgb(base.red * 0.35, base.green * 0.35, base.blue * 0.35);
            let handle = materials.add(ColorMaterial::from_color(fog_color));
            fog_materials
                .materials
                .insert((kind as u8, OreKind::None as u8, bucket), handle);
        }
    }
}

/// Update: recompute which tiles the player faction can see this frame.
///
/// Per-agent sweeps run live every frame at `effective_vision_radius`
/// (`STANDARD_VIEW_RADIUS = 15` normally; `LOOKOUT_VIEW_RADIUS = 50`
/// while `ActiveLookout` is attached). Static vision sources
/// (settlements, camps, active lookouts) contribute via their cached
/// `CachedVisionSet.tiles` — a pure set-union here, no LOS work.
/// LOS uses `has_vision_los` so the player faction can see through its
/// own constructed walls and doors. See `plans/lookout-base.md`.
///
/// Phase 3d (multiplayer): in `NetMode::Client` the agent_query is
/// empty (the client doesn't own live Person/PersonAI entities), so
/// fog would degrade to "only landmarks". To keep client fog correct
/// we additionally scan `ReplicatedEntity` stubs flagged
/// `ReplicatedEntityKind::Person` and run the same per-agent LOS sweep
/// over their replicated `tile` / `z` / `faction_id`. On the host the
/// query is empty (no `ReplicatedEntity` exists) so the union is free.
#[allow(clippy::too_many_arguments)]
pub fn fog_update_system(
    player_faction: Res<PlayerFaction>,
    chunk_map: Res<ChunkMap>,
    wall_map: Res<WallMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    wall_q: Query<&Wall>,
    agent_query: Query<(&Transform, &PersonAI, &FactionMember, &LodLevel, Option<&ActiveLookout>)>,
    landmark_query: Query<&Transform, With<PlayerFactionMarker>>,
    cached_sources: Query<&CachedVisionSet>,
    replicated_persons: Query<&crate::net::client::ReplicatedEntity>,
    mut fog_map: ResMut<FogMap>,
) {
    let old_visible = std::mem::take(&mut fog_map.visible);
    fog_map.dirty_tiles.clear();

    let mut new_visible: AHashSet<(i32, i32)> = AHashSet::default();
    let observer = player_faction.faction_id;

    // Player-owned landmarks (FactionCenter) are always considered visible so
    // they stay bright even when no persons are nearby.
    for transform in landmark_query.iter() {
        let lx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ly = (transform.translation.y / TILE_SIZE).floor() as i32;
        new_visible.insert((lx, ly));
    }

    for (transform, ai, member, lod, active_lookout) in agent_query.iter() {
        if member.faction_id != observer {
            continue;
        }

        let ax = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ay = (transform.translation.y / TILE_SIZE).floor() as i32;

        if *lod == LodLevel::Dormant {
            // Dormant agents skip the expensive LOS scan but still mark their
            // own tile so a lone settler far from the camera stays bright.
            new_visible.insert((ax as i32, ay as i32));
            continue;
        }

        // An agent under `ActiveLookout` runs its sweep via the cached
        // source (joined below) — skip the live scan here so we don't
        // duplicate work every frame at radius 50.
        if active_lookout.is_some() {
            new_visible.insert((ax, ay));
            continue;
        }

        let az = ai.current_z;
        let view_radius =
            crate::simulation::vision::effective_vision_radius(None) as i32;
        let view_radius_sq = view_radius * view_radius;

        for dy in -view_radius..=view_radius {
            for dx in -view_radius..=view_radius {
                if dx * dx + dy * dy > view_radius_sq {
                    continue;
                }
                let ttx = ax + dx;
                let tty = ay + dy;

                let raw_z = chunk_map.surface_z_at(ttx, tty);
                let tz = raw_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;

                let in_los = dx * dx + dy * dy <= 1
                    || has_vision_los(
                        &chunk_map,
                        &wall_map,
                        &door_map,
                        &wall_q,
                        (ax, ay, az),
                        (ttx, tty, tz),
                        observer,
                    );
                if in_los {
                    new_visible.insert((ttx as i32, tty as i32));
                }
            }
        }
    }

    // Phase 3d: replicated agent stubs (client only). Same per-agent
    // sweep as live agents but reading `ReplicatedEntity.{tile, z,
    // faction_id}` instead of `Transform` + `PersonAI` + `FactionMember`.
    // No `LodLevel` on stubs — treat as full-LOD. No `ActiveLookout` —
    // active-lookout state isn't replicated yet, defer to standard
    // radius. On the host this iterator is empty so the union is free.
    for rep in replicated_persons.iter() {
        if !matches!(rep.kind, crate::net::client::ReplicatedEntityKind::Person) {
            continue;
        }
        if rep.faction_id != observer {
            continue;
        }
        let (ax, ay) = rep.tile;
        new_visible.insert((ax, ay));
        let az = rep.z;
        let view_radius = crate::simulation::vision::effective_vision_radius(None) as i32;
        let view_radius_sq = view_radius * view_radius;
        for dy in -view_radius..=view_radius {
            for dx in -view_radius..=view_radius {
                if dx * dx + dy * dy > view_radius_sq {
                    continue;
                }
                let ttx = ax + dx;
                let tty = ay + dy;
                let raw_z = chunk_map.surface_z_at(ttx, tty);
                let tz = raw_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
                let in_los = dx * dx + dy * dy <= 1
                    || has_vision_los(
                        &chunk_map,
                        &wall_map,
                        &door_map,
                        &wall_q,
                        (ax, ay, az),
                        (ttx, tty, tz),
                        observer,
                    );
                if in_los {
                    new_visible.insert((ttx, tty));
                }
            }
        }
    }

    // Union the cached visible sets of every player-faction static source
    // (lookouts, settlements, camps). One set-union per source — the LOS
    // raycast lives in `recompute_dirty_vision_sets_system`.
    for cache in cached_sources.iter() {
        if cache.faction != observer {
            continue;
        }
        new_visible.extend(cache.tiles.iter().copied());
    }

    // Tiles that changed state this frame.
    for &pos in new_visible.symmetric_difference(&old_visible) {
        fog_map.dirty_tiles.push(pos);
        fog_map.explored.insert(pos);
    }

    fog_map.visible = new_visible;
}

/// PostUpdate: apply fog-state changes to tile sprite materials/visibility.
/// Only processes dirty_tiles to keep per-frame cost proportional to movement.
pub fn apply_fog_to_tiles_system(
    fog_map: Res<FogMap>,
    sprite_index: Res<TileSpriteIndex>,
    tile_materials: Res<TileMaterials>,
    fog_tile_materials: Res<FogTileMaterials>,
    chunk_map: Res<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    camera_view_z: Res<CameraViewZ>,
    mut query: Query<(&mut MeshMaterial2d<ColorMaterial>, &mut Visibility), With<TileSprite>>,
) {
    if fog_map.dirty_tiles.is_empty() {
        return;
    }

    for &(tx, ty) in &fog_map.dirty_tiles {
        let Some(&entity) = sprite_index.by_tile.get(&(tx, ty)) else {
            continue;
        };
        let Ok((mut mat, mut vis)) = query.get_mut(entity) else {
            continue;
        };

        let surf_z = chunk_map.surface_z_at(tx as i32, ty as i32);
        let (render_kind, render_ore, render_z, base_vis) = resolve_render_tile(
            &chunk_map,
            &gen,
            &globe,
            tx as i32,
            ty as i32,
            surf_z,
            camera_view_z.0,
        );

        let new_vis = apply_fog_to_material(
            &fog_map,
            (tx, ty),
            base_vis,
            render_kind,
            render_ore,
            render_z,
            &tile_materials,
            &fog_tile_materials,
            &mut mat,
        );
        if *vis != new_vis {
            *vis = new_vis;
        }
    }
}

/// Shared helper: given a tile's base visibility and fog state, set the correct
/// material handle and return the final Visibility value.
pub fn apply_fog_to_material(
    fog_map: &FogMap,
    tile_pos: (i32, i32),
    base_vis: Visibility,
    render_kind: TileKind,
    render_ore: OreKind,
    render_z: i32,
    tile_materials: &TileMaterials,
    fog_tile_materials: &FogTileMaterials,
    mat: &mut MeshMaterial2d<ColorMaterial>,
) -> Visibility {
    if base_vis == Visibility::Hidden {
        return Visibility::Hidden;
    }
    if fog_map.is_visible(tile_pos) {
        mat.0 = tile_materials.handle_for(render_kind, render_ore, render_z);
        Visibility::Visible
    } else if fog_map.is_explored(tile_pos) {
        mat.0 = fog_tile_materials.handle_for(render_kind, render_ore, render_z);
        Visibility::Visible
    } else {
        Visibility::Hidden
    }
}
