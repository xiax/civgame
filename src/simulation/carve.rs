use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::Globe;
use crate::world::terrain::{tile_at_3d, WorldGen};
use crate::world::tile::{OreKind, TileData, TileKind};
use bevy::prelude::*;

/// Yield per Wall/Stone block carved away. Ore tiles use the same per-block qty.
pub const STONE_PER_BLOCK: u32 = 2;

/// Map an ore kind to the resource that pops out of the rock when its tile is mined.
pub fn ore_yield_resource_id(ore: OreKind) -> ResourceId {
    let id = match ore {
        OreKind::Coal => core_ids::Coal.get(),
        OreKind::Iron => core_ids::Iron.get(),
        OreKind::Copper => core_ids::Copper.get(),
        OreKind::Tin => core_ids::Tin.get(),
        OreKind::Gold => core_ids::Gold.get(),
        OreKind::Silver => core_ids::Silver.get(),
        OreKind::None => core_ids::Stone.get(),
    };
    *id.expect("core_ids: ore_yield_resource_id called before init_core_ids()")
}

/// Yields produced by a single carve call. At most two entries (head + floor).
pub type CarveYield = Vec<(ResourceId, u32)>;

fn yield_for_tile(data: TileData) -> Option<(ResourceId, u32)> {
    if data.kind == TileKind::Ore {
        return Some((ore_yield_resource_id(data.ore_kind()), STONE_PER_BLOCK));
    }
    if data.kind.is_stone_like() {
        // Phase F (knowledge-system overhaul): a Limestone tile yields the
        // dedicated `limestone` resource so it can feed the `Burn Lime` craft
        // recipe (FIRED_POTTERY-gated). All other stone lithologies still
        // resolve to generic `stone`. Yield qty preserves the per-lithology
        // count (`stone_yield_count`).
        let id = if data.kind == TileKind::Limestone {
            *core_ids::Limestone
                .get()
                .expect("core_ids: yield_for_tile called before init_core_ids()")
        } else {
            *core_ids::Stone
                .get()
                .expect("core_ids: yield_for_tile called before init_core_ids()")
        };
        let qty = data.kind.stone_yield_count().max(STONE_PER_BLOCK);
        return Some((id, qty));
    }
    None
}

/// Open up (tx, ty) so an agent can stand at foot-Z = `target_floor_z`.
///
/// - Headspace at (tx, ty, target_floor_z + 1): if Wall/Stone/Ore, set to Air.
/// - Floor at (tx, ty, target_floor_z): if Wall/Stone/Ore, set to Dirt.
///
/// Returns the per-block (ResourceId, qty) drops. The actual material is read
/// via `tile_at_3d` so that uncarved subsurface ore yields the right resource
/// rather than the cache-only "Wall everywhere" approximation. Emits a
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

    let head_z = target_floor_z + 1;
    let head = tile_at_3d(chunk_map, gen, globe, tx, ty, head_z);
    if head.kind.is_stone_like() {
        if let Some(y) = yield_for_tile(head) {
            yields.push(y);
        }
    }

    let floor = tile_at_3d(chunk_map, gen, globe, tx, ty, target_floor_z);
    if floor.kind.is_stone_like() {
        if let Some(y) = yield_for_tile(floor) {
            yields.push(y);
        }
    }

    finalize_carved_tile(chunk_map, gen, globe, tx, ty, target_floor_z, events);

    yields
}

/// Non-yielding body of [`carve_tile`]: opens head + floor, emits
/// `TileChangedEvent`. Incremental excavation pays per-level yields itself and
/// calls this once at level 7 to apply the final tile mutation. One-shot
/// callers (wells, terraform, wall destruction) use [`carve_tile`] which
/// wraps this and pays the head + floor block yield.
pub fn finalize_carved_tile(
    chunk_map: &mut ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32,
    ty: i32,
    target_floor_z: i32,
    events: &mut EventWriter<TileChangedEvent>,
) {
    let mut changed = false;

    let head_z = target_floor_z + 1;
    let head = tile_at_3d(chunk_map, gen, globe, tx, ty, head_z);
    match head.kind {
        k if k.is_stone_like() => {
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
            // Carve to Air to make headroom.
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
        k if k.is_stone_like() => {
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
        _ => {
            // Floor is already passable (procedural Dirt from topsoil, etc.) —
            // write a delta with the existing kind so `surface_kind` cache
            // reflects the real kind instead of the Air placeholder left by
            // the head carve. Without this, surface_tile_kind returns Air
            // and the right-click menu hides Dig Down on subsequent clicks.
            chunk_map.set_tile(tx, ty, target_floor_z, floor);
            changed = true;
        }
    }

    if changed {
        events.send(TileChangedEvent {
            tx: tx as i32,
            ty: ty as i32,
        });
    }
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
        TileKind::Air | TileKind::Water | TileKind::River => {
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
            tx: tx as i32,
            ty: ty as i32,
        });
    }

    filled
}
