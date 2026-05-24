//! Incremental 7-level excavation shared by Mine (`gather` stone/ore branch)
//! and Dig Down (`dig.rs`). Each work cycle calls [`advance`] which credits one
//! level on the target cell, pays a flat per-level yield, and at level 7 calls
//! [`carve::finalize_carved_tile`] to apply the real tile mutation.
//!
//! See `plans/incremental-mining.md`.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::carve::{finalize_carved_tile, ore_yield_resource_id};
use crate::simulation::tools::{ToolKit, ToolRequirement, ToolUseKind};
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::{TileCarvedEvent, TileChangedEvent};
use crate::world::globe::Globe;
use crate::world::terrain::{tile_at_3d, WorldGen};
use crate::world::tile::TileKind;

/// Levels 1-6 are partial; level 7 triggers the carve and clears the level
/// cache. The on-tile flag stores 0..=7.
pub const EXCAVATION_LEVEL_MAX: u8 = 7;

/// Bare-hands cap on stone-like tiles. With no Pick in `ToolKit`, a worker can
/// chip a stone column up to here and no further. Soil-like tiles are unbound.
pub const HAND_DEPTH_LIMIT: u8 = 3;

/// Work ticks per level. Same total work for a fully-pickaxed carve
/// (7·LEVEL_WORK_TICKS = 84 vs the old DIG_WORK_TICKS=30 / STONE.work_ticks=30
/// single-pop), but spread across visible level transitions. Tool tier scales
/// this via [`tools::work_speed_mult`] in the executor.
pub const LEVEL_WORK_TICKS: u8 = 12;

/// Which excavation mode owns a `(tile, z)` cell. `Mine` removes the wall at
/// agent.z; `DigDown` removes the floor (and optionally head if both are
/// stone-like) one level below.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum ExcavationMode {
    Mine,
    DigDown,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ExcavationKey {
    pub tile: (i32, i32),
    pub z: i8,
    pub mode: ExcavationMode,
}

#[derive(Copy, Clone, Debug)]
pub struct ExcavationCell {
    /// 1..=6 while partial; never stored at 0 or 7 (cells at 0 are absent from
    /// the map, and at 7 they flip to `completed_carve = true`).
    pub level: u8,
    /// Set once the level-7 carve has actually finalised on this cell. Survives
    /// chunk stream-out via [`restamp_excavation_on_chunk_load`].
    pub completed_carve: bool,
}

/// Durable, off-chunk source of truth for partial and completed incremental
/// excavations. Chunks regenerate from `Globe + seed` on stream-in, so without
/// this resource any in-flight excavation would silently revert.
#[derive(Resource, Default)]
pub struct ExcavationMap {
    pub cells: AHashMap<ExcavationKey, ExcavationCell>,
}

impl ExcavationMap {
    pub fn level_at(&self, key: &ExcavationKey) -> u8 {
        self.cells.get(key).map(|c| c.level).unwrap_or(0)
    }
}

/// Standard pick-tool requirement. Centralised so dig/gather/UI all gate
/// identically.
pub fn pick_requirement() -> ToolRequirement {
    ToolRequirement::any(ToolUseKind::Mine)
}

/// Helper for HTN / gather_claims candidate filtering: is this stone-like
/// tile workable past its current level by the given toolkit? Returns true
/// for non-stone tiles (no pick needed) and for fresh/early stone tiles a
/// pickless worker can still chip; returns false when the partial level
/// would block the worker on the first cycle.
///
/// Use site (future): autonomous HTN GatherStone candidate selection should
/// call this to skip tiles where the worker would `TargetInvalid` on arrival.
/// Today the executor handles the case via finish_gather + MethodHistory
/// penalty (natural bias against thrashing).
pub fn tile_workable_by(
    toolkit: Option<&ToolKit>,
    kind: TileKind,
    current_level: u8,
) -> bool {
    current_level < excavation_depth_cap(toolkit, kind)
}

/// How deep can this worker excavate a tile of `kind`? Soil/grass need no
/// pick (depth-cap = 7). Stone-like tiles cap at [`HAND_DEPTH_LIMIT`] for
/// bare hands; any Pick unlocks 7. Absent `ToolKit` reads as fixture-armed.
pub fn excavation_depth_cap(toolkit: Option<&ToolKit>, kind: TileKind) -> u8 {
    if !kind.is_stone_like() {
        return EXCAVATION_LEVEL_MAX;
    }
    match toolkit {
        None => EXCAVATION_LEVEL_MAX,
        Some(tk) if tk.satisfies(&pick_requirement()) => EXCAVATION_LEVEL_MAX,
        Some(_) => HAND_DEPTH_LIMIT,
    }
}

/// Flat 1 unit per level. Stone-like tiles drop stone; Ore drops the
/// per-ore resource. Soil/grass yield nothing (the tile reshapes but produces
/// no commodity).
pub fn level_yield(kind: TileKind, ore: crate::world::tile::OreKind) -> Option<(ResourceId, u32)> {
    if kind == TileKind::Ore {
        return Some((ore_yield_resource_id(ore), 1));
    }
    if kind.is_stone_like() {
        let id = *core_ids::Stone
            .get()
            .expect("core_ids: level_yield called before init_core_ids()");
        return Some((id, 1));
    }
    None
}

/// Result of one excavation cycle on a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// Cell advanced to `new_level`, still in 1..=6.
    Levelled { new_level: u8 },
    /// Cell reached level 7; tile was finalised via [`carve::finalize_carved_tile`].
    Carved,
}

/// Apply one excavation cycle to `key`. Updates `ExcavationMap`, mirrors the
/// level into `TileData.flags` for the worked tile, emits `TileChangedEvent`,
/// and at level 7 emits `TileCarvedEvent` and calls `finalize_carved_tile`.
///
/// Yield (when non-zero) is returned via the `yields` out-vec so the caller
/// can route through `Carrier::try_pick_up` / ground spillover the same way
/// today's gather/dig do.
///
/// **Does not gate on tools** — caller must consult [`excavation_depth_cap`]
/// and refuse to advance past it.
pub fn advance(
    map: &mut ExcavationMap,
    chunk_map: &mut ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    key: ExcavationKey,
    tile_changed: &mut EventWriter<TileChangedEvent>,
    tile_carved: &mut EventWriter<TileCarvedEvent>,
    yields: &mut Vec<(ResourceId, u32)>,
) -> AdvanceOutcome {
    let (tx, ty) = key.tile;
    let worked_z = key.z as i32;

    // Read the material we're chipping for yield. `tile_at_3d` reads the
    // procedural source (not the cache) so deep ore yields its real resource
    // instead of cache-projected Wall.
    let worked = tile_at_3d(chunk_map, gen, globe, tx, ty, worked_z);

    // Look up the current level (0 if absent).
    let prev = map.cells.get(&key).copied();
    let prev_level = prev.map(|c| c.level).unwrap_or(0);
    let new_level = prev_level.saturating_add(1).min(EXCAVATION_LEVEL_MAX);

    // Pay this step's yield (flat 1 unit per level when the material yields).
    if let Some(y) = level_yield(worked.kind, worked.ore_kind()) {
        yields.push(y);
    }

    // The level bit caches onto the *agent-walkable surface tile* so movement
    // / sprite refresh see the partial state. For Mine that's the wall itself
    // (worked_z); for Dig Down the surface sits one above the floor we're
    // chipping (worked_z + 1).
    let cache_z = match key.mode {
        ExcavationMode::Mine => worked_z,
        ExcavationMode::DigDown => worked_z + 1,
    };

    if new_level < EXCAVATION_LEVEL_MAX {
        // Partial step: bump the map, mirror into TileData.flags on the
        // agent-walkable surface tile, emit TileChangedEvent only.
        map.cells.insert(
            key,
            ExcavationCell {
                level: new_level,
                completed_carve: false,
            },
        );
        let mut td = chunk_map.tile_at(tx, ty, cache_z);
        td.set_excavation_level(new_level);
        chunk_map.set_tile(tx, ty, cache_z, td);
        tile_changed.send(TileChangedEvent { tx, ty });
        AdvanceOutcome::Levelled { new_level }
    } else {
        // Level 7: finalise the carve and clear the partial cache. The map
        // entry stays so the restamp pass knows to re-apply the carve on
        // chunk reload.
        map.cells.insert(
            key,
            ExcavationCell {
                level: EXCAVATION_LEVEL_MAX,
                completed_carve: true,
            },
        );

        match key.mode {
            ExcavationMode::DigDown => {
                // Dig Down works against the floor; head sits at z+1 (the
                // tile the worker is currently standing on, surface_z).
                // `finalize_carved_tile` opens both head + floor.
                let target_floor_z = worked_z;
                finalize_carved_tile(chunk_map, gen, globe, tx, ty, target_floor_z, tile_changed);
                tile_carved.send(TileCarvedEvent {
                    tx,
                    ty,
                    new_floor_z: target_floor_z,
                });
            }
            ExcavationMode::Mine => {
                // Mine works against a wall at agent.z. The floor below
                // (z-1) is whatever surface tile already exists. Carving
                // routes through `finalize_carved_tile` with the floor
                // one below the worked wall.
                let target_floor_z = worked_z - 1;
                finalize_carved_tile(chunk_map, gen, globe, tx, ty, target_floor_z, tile_changed);
                tile_carved.send(TileCarvedEvent {
                    tx,
                    ty,
                    new_floor_z: target_floor_z,
                });
            }
        }
        AdvanceOutcome::Carved
    }
}

/// Restamp every `ExcavationMap` entry inside chunks that just streamed back
/// in. Completed carves re-apply via `finalize_carved_tile`; partials write
/// the level bit back onto the freshly-generated `TileData`.
///
/// Ordered after [`construction::restamp_walls_on_chunk_load`] (so walls are
/// re-stamped before a Mine excavation tries to read them) and before
/// `restamp_runtime_water_on_chunk_load` (so completed carves open the column
/// before water lays in).
pub fn restamp_excavation_on_chunk_load(
    mut events: EventReader<crate::world::chunk_streaming::ChunkLoadedEvent>,
    map: Res<ExcavationMap>,
    mut chunk_map: ResMut<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut tile_changed: EventWriter<TileChangedEvent>,
) {
    use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
    if map.cells.is_empty() {
        events.clear();
        return;
    }
    for ev in events.read() {
        let chunk: ChunkCoord = ev.coord;
        for (key, cell) in map.cells.iter() {
            let cx = key.tile.0.div_euclid(CHUNK_SIZE as i32);
            let cy = key.tile.1.div_euclid(CHUNK_SIZE as i32);
            if cx != chunk.0 || cy != chunk.1 {
                continue;
            }
            let (tx, ty) = key.tile;
            if cell.completed_carve {
                let target_floor_z = match key.mode {
                    ExcavationMode::DigDown => key.z as i32,
                    ExcavationMode::Mine => (key.z as i32) - 1,
                };
                finalize_carved_tile(
                    &mut chunk_map,
                    &gen,
                    &globe,
                    tx,
                    ty,
                    target_floor_z,
                    &mut tile_changed,
                );
            } else if cell.level > 0 && cell.level < EXCAVATION_LEVEL_MAX {
                let cache_z = match key.mode {
                    ExcavationMode::Mine => key.z as i32,
                    ExcavationMode::DigDown => (key.z as i32) + 1,
                };
                let mut td = chunk_map.tile_at(tx, ty, cache_z);
                td.set_excavation_level(cell.level);
                chunk_map.set_tile(tx, ty, cache_z, td);
                tile_changed.send(TileChangedEvent { tx, ty });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::tile::OreKind;

    #[test]
    fn level_yield_stone_one_unit() {
        // Stone-like tiles yield exactly one unit per level (raw, before
        // mining_activity faction multipliers in the executor).
        // Smoke-test against the cache-only enum dispatch; core_ids must be
        // initialised by the binary before this is exercised at runtime — the
        // test path skips by checking the get() arm.
        if core_ids::Stone.get().is_none() {
            return; // not initialised in unit-test context
        }
        for k in [
            TileKind::Stone,
            TileKind::Granite,
            TileKind::Limestone,
            TileKind::Sandstone,
            TileKind::Basalt,
            TileKind::Wall,
        ] {
            let y = level_yield(k, OreKind::None).expect("stone-like has yield");
            assert_eq!(y.1, 1, "level yield for {:?} should be 1", k);
        }
    }

    #[test]
    fn level_yield_soil_none() {
        for k in [
            TileKind::Grass,
            TileKind::Dirt,
            TileKind::Loam,
            TileKind::Sand,
            TileKind::Marsh,
        ] {
            assert!(level_yield(k, OreKind::None).is_none(), "{:?} yields nothing", k);
        }
    }

    #[test]
    fn excavation_depth_cap_soil_always_full() {
        // No tool, any tool, no toolkit at all — soil always reaches 7.
        for k in [TileKind::Grass, TileKind::Dirt, TileKind::Loam] {
            assert_eq!(excavation_depth_cap(None, k), EXCAVATION_LEVEL_MAX);
            let empty = ToolKit::default();
            assert_eq!(excavation_depth_cap(Some(&empty), k), EXCAVATION_LEVEL_MAX);
        }
    }

    #[test]
    fn excavation_depth_cap_stone_gates_on_pick() {
        // No toolkit reads as fixture-armed (7).
        assert_eq!(excavation_depth_cap(None, TileKind::Stone), EXCAVATION_LEVEL_MAX);
        // Empty toolkit (no pick) caps at HAND_DEPTH_LIMIT.
        let empty = ToolKit::default();
        assert_eq!(
            excavation_depth_cap(Some(&empty), TileKind::Stone),
            HAND_DEPTH_LIMIT
        );
    }
}
