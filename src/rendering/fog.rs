use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::rendering::camera::CameraViewZ;
use crate::rendering::color_map::{shaded_tile_color, z_bucket};
use crate::simulation::faction::{FactionMember, PlayerFaction, PlayerFactionMarker};
use crate::simulation::line_of_sight::has_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::PersonAI;
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::{
    resolve_render_tile, TileMaterials, TileSpriteIndex, TileSprite, RENDERABLE_KINDS,
};
use crate::world::globe::Globe;
use crate::world::terrain::{WorldGen, TILE_SIZE};
use crate::world::tile::TileKind;

/// Fog of war state: which tiles the player faction can currently see / has ever seen.
#[derive(Resource, Default)]
pub struct FogMap {
    pub visible: AHashSet<(i16, i16)>,
    pub explored: AHashSet<(i16, i16)>,
    /// Tiles whose fog state changed this frame — processed by apply_fog_to_tiles_system.
    pub dirty_tiles: Vec<(i16, i16)>,
}

impl FogMap {
    pub fn is_visible(&self, pos: (i16, i16)) -> bool {
        self.visible.contains(&pos)
    }

    pub fn is_explored(&self, pos: (i16, i16)) -> bool {
        self.explored.contains(&pos)
    }
}

/// Darkened (35 % brightness) tile material variants for the
/// explored-but-not-currently-visible fog state.
#[derive(Resource, Default)]
pub struct FogTileMaterials {
    pub materials: AHashMap<(u8, i32), Handle<ColorMaterial>>,
    pub tile_mesh: Handle<Mesh>,
}

impl FogTileMaterials {
    pub fn handle_for(&self, kind: TileKind, z: i32) -> Handle<ColorMaterial> {
        self.materials
            .get(&(kind as u8, z_bucket(z)))
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
        for bucket in bucket_min..=bucket_max {
            let z = bucket * 4 + 2;
            let base = shaded_tile_color(kind, z).to_srgba();
            let fog_color = Color::srgb(base.red * 0.35, base.green * 0.35, base.blue * 0.35);
            let handle = materials.add(ColorMaterial::from_color(fog_color));
            fog_materials.materials.insert((kind as u8, bucket), handle);
        }
    }
}

const VIEW_RADIUS: i32 = 15;
const VIEW_RADIUS_SQ: i32 = VIEW_RADIUS * VIEW_RADIUS;

/// Update: recompute which tiles the player faction can see this frame.
pub fn fog_update_system(
    player_faction: Res<PlayerFaction>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    agent_query: Query<(&Transform, &PersonAI, &FactionMember, &LodLevel)>,
    landmark_query: Query<&Transform, With<PlayerFactionMarker>>,
    mut fog_map: ResMut<FogMap>,
) {
    let old_visible = std::mem::take(&mut fog_map.visible);
    fog_map.dirty_tiles.clear();

    let mut new_visible: AHashSet<(i16, i16)> = AHashSet::default();

    // Player-owned landmarks (FactionCenter) are always considered visible so
    // they stay bright even when no persons are nearby.
    for transform in landmark_query.iter() {
        let lx = (transform.translation.x / TILE_SIZE).floor() as i16;
        let ly = (transform.translation.y / TILE_SIZE).floor() as i16;
        new_visible.insert((lx, ly));
    }

    for (transform, ai, member, lod) in agent_query.iter() {
        if member.faction_id != player_faction.faction_id {
            continue;
        }

        let ax = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ay = (transform.translation.y / TILE_SIZE).floor() as i32;

        if *lod == LodLevel::Dormant {
            // Dormant agents skip the expensive LOS scan but still mark their
            // own tile so a lone settler far from the camera stays bright.
            new_visible.insert((ax as i16, ay as i16));
            continue;
        }

        let az = ai.current_z;

        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dy * dy > VIEW_RADIUS_SQ {
                    continue;
                }
                let ttx = ax + dx;
                let tty = ay + dy;

                let raw_z = chunk_map.surface_z_at(ttx, tty);
                let tz = raw_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;

                let in_los = dx * dx + dy * dy <= 1
                    || has_los(&chunk_map, &door_map, (ax, ay, az), (ttx, tty, tz));
                if in_los {
                    new_visible.insert((ttx as i16, tty as i16));
                }
            }
        }
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
        let (render_kind, render_z, base_vis) = resolve_render_tile(
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
    tile_pos: (i16, i16),
    base_vis: Visibility,
    render_kind: TileKind,
    render_z: i32,
    tile_materials: &TileMaterials,
    fog_tile_materials: &FogTileMaterials,
    mat: &mut MeshMaterial2d<ColorMaterial>,
) -> Visibility {
    if base_vis == Visibility::Hidden {
        return Visibility::Hidden;
    }
    if fog_map.is_visible(tile_pos) {
        mat.0 = tile_materials.handle_for(render_kind, render_z);
        Visibility::Visible
    } else if fog_map.is_explored(tile_pos) {
        mat.0 = fog_tile_materials.handle_for(render_kind, render_z);
        Visibility::Visible
    } else {
        Visibility::Hidden
    }
}
