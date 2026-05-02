use crate::economy::goods::Good;
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::Globe;
use crate::world::terrain::{tile_at_3d, WorldGen};
use crate::world::tile::{OreKind, TileData, TileKind};
use bevy::prelude::*;

/// Yield per Wall/Stone block carved away. Ore tiles use the same per-block qty.
pub const STONE_PER_BLOCK: u32 = 2;

/// Map an ore kind to the Good that pops out of the rock when its tile is mined.
pub fn ore_yield_good(ore: OreKind) -> Good {
    match ore {
        OreKind::Coal => Good::Coal,
        OreKind::Iron => Good::Iron,
        OreKind::Copper => Good::Copper,
        OreKind::Tin => Good::Tin,
        OreKind::Gold => Good::Gold,
        OreKind::Silver => Good::Silver,
        OreKind::None => Good::Stone,
    }
}

/// Yields produced by a single carve call. At most two entries (head + floor).
pub type CarveYield = Vec<(Good, u32)>;

fn yield_for_tile(data: TileData) -> Option<(Good, u32)> {
    match data.kind {
        TileKind::Wall | TileKind::Stone => Some((Good::Stone, STONE_PER_BLOCK)),
        TileKind::Ore => Some((ore_yield_good(data.ore_kind()), STONE_PER_BLOCK)),
        _ => None,
    }
}

/// Open up (tx, ty) so an agent can stand at foot-Z = `target_floor_z`.
///
/// - Headspace at (tx, ty, target_floor_z + 1): if Wall/Stone/Ore, set to Air.
/// - Floor at (tx, ty, target_floor_z): if Wall/Stone/Ore, set to Dirt.
///
/// Returns the per-block (Good, qty) drops. The actual material is read via
/// `tile_at_3d` so that uncarved subsurface ore yields the right Good rather
/// than the cache-only "Wall everywhere" approximation. Emits a
/// `TileChangedEvent` if anything changed.
pub fn carve_tile(
    chunk_map: &mut ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32,
    ty: i32,
    target_floor_z: i32,
    events: &mut EventWriter<TileChangedEvent>,
) -> CarveYield {
    let mut yields: CarveYield = Vec::with_capacity(2);
    let mut changed = false;

    let head_z = target_floor_z + 1;
    let head = tile_at_3d(chunk_map, gen, globe, tx, ty, head_z);
    match head.kind {
        TileKind::Wall | TileKind::Stone | TileKind::Ore => {
            if let Some(y) = yield_for_tile(head) {
                yields.push(y);
            }
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
        TileKind::Air | TileKind::Ramp => {} // already open
        _ => {
            // Some other solid (Dirt, Grass, etc. somehow at headspace).
            // Carve to Air to make headroom; no yield.
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

    let floor = tile_at_3d(chunk_map, gen, globe, tx, ty, target_floor_z);
    match floor.kind {
        TileKind::Wall | TileKind::Stone | TileKind::Ore => {
            if let Some(y) = yield_for_tile(floor) {
                yields.push(y);
            }
            chunk_map.set_tile(
                tx,
                ty,
                target_floor_z,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
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

    yields
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
