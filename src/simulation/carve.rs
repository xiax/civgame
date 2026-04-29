use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::tile::{TileData, TileKind};
use bevy::prelude::*;

/// Yield per Wall/Stone block carved away.
pub const STONE_PER_BLOCK: u32 = 2;

/// Open up (tx, ty) so an agent can stand at foot-Z = `target_floor_z`.
///
/// - Headspace at (tx, ty, target_floor_z + 1): if Wall/Stone, set to Air.
/// - Floor at (tx, ty, target_floor_z): if Wall/Stone, set to Dirt.
///
/// Other floor kinds (Grass, Dirt, Stone-already-mineable) are left in
/// place — the agent can already stand on them. Returns the number of
/// blocks broken (0..=2). Emits a TileChangedEvent if anything changed.
pub fn carve_tile(
    chunk_map: &mut ChunkMap,
    tx: i32,
    ty: i32,
    target_floor_z: i32,
    events: &mut EventWriter<TileChangedEvent>,
) -> u32 {
    let mut blocks_broken = 0u32;
    let mut changed = false;

    let head_z = target_floor_z + 1;
    let head = chunk_map.tile_at(tx, ty, head_z);
    match head.kind {
        TileKind::Wall | TileKind::Stone => {
            chunk_map.set_tile(
                tx,
                ty,
                head_z,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            blocks_broken += 1;
            changed = true;
        }
        TileKind::Air | TileKind::Ramp => {} // already open
        _ => {
            // Some other solid (Dirt, Grass, etc. somehow at headspace).
            // Carve to Air to make headroom; no stone yield.
            chunk_map.set_tile(
                tx,
                ty,
                head_z,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            changed = true;
        }
    }

    let floor = chunk_map.tile_at(tx, ty, target_floor_z);
    match floor.kind {
        TileKind::Wall | TileKind::Stone => {
            chunk_map.set_tile(
                tx,
                ty,
                target_floor_z,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
            blocks_broken += 1;
            changed = true;
        }
        _ => {} // already a passable floor or open space — leave it
    }

    if changed {
        events.send(TileChangedEvent {
            tx: tx as i16,
            ty: ty as i16,
        });
    }

    blocks_broken
}

/// Inverse of `carve_tile`. Raises the surface at (tx, ty) by writing
/// Dirt at `target_floor_z` and clearing headspace at `target_floor_z + 1`.
/// Returns 1 if anything changed (caller deducts the fill material from
/// the agent's inventory), 0 if the floor was already filled.
pub fn fill_tile(
    chunk_map: &mut ChunkMap,
    tx: i32,
    ty: i32,
    target_floor_z: i32,
    events: &mut EventWriter<TileChangedEvent>,
) -> u32 {
    let mut changed = false;
    let mut filled = 0u32;

    let floor = chunk_map.tile_at(tx, ty, target_floor_z);
    match floor.kind {
        TileKind::Air | TileKind::Water => {
            chunk_map.set_tile(
                tx,
                ty,
                target_floor_z,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
            filled = 1;
            changed = true;
        }
        _ => {} // already solid floor
    }

    let head_z = target_floor_z + 1;
    let head = chunk_map.tile_at(tx, ty, head_z);
    if !matches!(head.kind, TileKind::Air | TileKind::Ramp) {
        chunk_map.set_tile(
            tx,
            ty,
            head_z,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        changed = true;
    }

    if changed {
        events.send(TileChangedEvent {
            tx: tx as i16,
            ty: ty as i16,
        });
    }

    filled
}
