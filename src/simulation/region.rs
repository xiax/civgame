//! Mega-chunks and `SettledRegions` — the unit of player settlement and
//! world-map switching.
//!
//! A mega-chunk is `MEGACHUNK_SIZE_CHUNKS² = 16×16 = 256` chunks, i.e. a
//! 512×512 tile region. It's a coarse partition of the global tile grid that
//! the player can settle in. Mega-chunks have no biome of their own — biomes
//! are derived per-tile from the climate field — so a single mega-chunk can
//! contain mixed biomes and a biome can span many mega-chunks.

use crate::world::chunk::CHUNK_SIZE;
use crate::world::globe::MEGACHUNK_SIZE_CHUNKS;
use crate::world::terrain::TILE_SIZE;
use ahash::AHashMap;
use bevy::prelude::*;

/// One focus point for chunk streaming. Chunks within `chunk_radius` of
/// `world_pos` are loaded; chunks within camera-tagged focus get sprites
/// spawned, others get only chunk data + sim.
#[derive(Clone, Copy, Debug)]
pub struct FocusPoint {
    pub world_pos: Vec2,
    pub chunk_radius: i32,
    /// True for the camera focus (the only one that gets sprites). False for
    /// off-camera settled-region centres (data + sim only).
    pub is_camera: bool,
}

#[derive(Resource, Default, Debug)]
pub struct SimulationFocus {
    pub points: Vec<FocusPoint>,
}

impl SimulationFocus {
    /// Test whether any focus point covers the given chunk coord within
    /// `extra` extra chunks. `extra=0` matches the focus's own radius;
    /// `extra=4` matches the unload radius (LOAD_RADIUS + 4).
    pub fn covers(&self, cx: i32, cy: i32, extra: i32) -> bool {
        for p in &self.points {
            let pcx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let pcy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let dx = (cx - pcx).abs();
            let dy = (cy - pcy).abs();
            if dx.max(dy) <= p.chunk_radius + extra {
                return true;
            }
        }
        false
    }

    /// Distance (in chunks) to the nearest camera-flagged focus, if any.
    pub fn distance_to_camera(&self, cx: i32, cy: i32) -> Option<i32> {
        let mut best: Option<i32> = None;
        for p in &self.points {
            if !p.is_camera {
                continue;
            }
            let pcx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let pcy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let dx = (cx - pcx).abs();
            let dy = (cy - pcy).abs();
            let d = dx.max(dy);
            best = Some(best.map(|b| b.min(d)).unwrap_or(d));
        }
        best
    }
}

pub type RegionId = u32;

/// Tile-width of one mega-chunk side.
pub const MEGACHUNK_TILES: i32 = MEGACHUNK_SIZE_CHUNKS * CHUNK_SIZE as i32;

/// Helpers for converting between mega-chunk, chunk, and tile coordinates.
pub struct MegaChunkCoord;

impl MegaChunkCoord {
    /// Mega-chunk that contains the given chunk coord.
    pub fn from_chunk(cx: i32, cy: i32) -> (i32, i32) {
        (
            cx.div_euclid(MEGACHUNK_SIZE_CHUNKS),
            cy.div_euclid(MEGACHUNK_SIZE_CHUNKS),
        )
    }

    /// Mega-chunk that contains the given world-tile.
    pub fn from_tile(tx: i32, ty: i32) -> (i32, i32) {
        (
            tx.div_euclid(MEGACHUNK_TILES),
            ty.div_euclid(MEGACHUNK_TILES),
        )
    }

    /// World-tile coordinate of the centre tile of a mega-chunk.
    pub fn center_tile(mx: i32, my: i32) -> (i32, i32) {
        let half = MEGACHUNK_TILES / 2;
        (mx * MEGACHUNK_TILES + half, my * MEGACHUNK_TILES + half)
    }

    /// Chunk-coord range covered by a mega-chunk, as `(cx0, cy0, cx1, cy1)`
    /// (exclusive upper).
    pub fn chunk_range(mx: i32, my: i32) -> (i32, i32, i32, i32) {
        let cx0 = mx * MEGACHUNK_SIZE_CHUNKS;
        let cy0 = my * MEGACHUNK_SIZE_CHUNKS;
        (
            cx0,
            cy0,
            cx0 + MEGACHUNK_SIZE_CHUNKS,
            cy0 + MEGACHUNK_SIZE_CHUNKS,
        )
    }
}

#[derive(Clone, Debug)]
pub struct SettledRegion {
    pub megachunk: (i32, i32),
    pub founding_tick: u64,
    pub name: String,
    /// Last camera-world-position the player viewed in this region.
    pub camera_bookmark: Vec2,
    pub player_owned: bool,
}

#[derive(Resource, Default)]
pub struct SettledRegions {
    pub by_id: AHashMap<RegionId, SettledRegion>,
    /// Reverse lookup: mega-chunk → region id.
    pub by_megachunk: AHashMap<(i32, i32), RegionId>,
    pub next_id: RegionId,
}

impl SettledRegions {
    pub fn settle(
        &mut self,
        megachunk: (i32, i32),
        founding_tick: u64,
        name: String,
        camera_bookmark: Vec2,
        player_owned: bool,
    ) -> RegionId {
        if let Some(&id) = self.by_megachunk.get(&megachunk) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.by_megachunk.insert(megachunk, id);
        self.by_id.insert(
            id,
            SettledRegion {
                megachunk,
                founding_tick,
                name,
                camera_bookmark,
                player_owned,
            },
        );
        id
    }

    pub fn is_settled(&self, megachunk: (i32, i32)) -> bool {
        self.by_megachunk.contains_key(&megachunk)
    }
}

/// When a player-faction agent's tile crosses into an unsettled mega-chunk,
/// settle it. The agent walks across organically — no teleport, no special
/// state. The new region appears in the world-map switcher next frame and
/// gets its own `SimulationFocus` point so its chunks stream around it.
pub fn detect_edge_crossing_system(
    mut settled: ResMut<SettledRegions>,
    clock: Res<crate::simulation::schedule::SimClock>,
    player_faction: Res<crate::simulation::faction::PlayerFaction>,
    mut log_events: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    persons: Query<
        (
            Entity,
            &Transform,
            &crate::simulation::faction::FactionMember,
        ),
        With<crate::simulation::person::Person>,
    >,
) {
    use crate::ui::activity_log::{ActivityEntryKind, ActivityLogEvent};
    use crate::world::terrain::TILE_SIZE;

    for (entity, transform, member) in &persons {
        if member.faction_id != player_faction.faction_id {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let mc = MegaChunkCoord::from_tile(tx, ty);
        if settled.is_settled(mc) {
            continue;
        }
        let next_idx = settled.by_id.len();
        let name = format!("Outpost {}", next_idx);
        settled.settle(
            mc,
            clock.tick,
            name.clone(),
            transform.translation.truncate(),
            true,
        );
        log_events.send(ActivityLogEvent {
            tick: clock.tick,
            actor: entity,
            faction_id: member.faction_id,
            kind: ActivityEntryKind::RegionSettled {
                megachunk: mc,
                region_name: name,
            },
        });
        info!("New region settled at mega-chunk {:?}", mc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn megachunk_coord_roundtrip() {
        let (mx, my) = MegaChunkCoord::from_tile(700, -50);
        assert_eq!((mx, my), (1, -1));
        let (cx, cy) = MegaChunkCoord::center_tile(mx, my);
        assert!(cx >= mx * MEGACHUNK_TILES && cx < (mx + 1) * MEGACHUNK_TILES);
        assert!(cy >= my * MEGACHUNK_TILES && cy < (my + 1) * MEGACHUNK_TILES);
    }

    #[test]
    fn settle_idempotent() {
        let mut sr = SettledRegions::default();
        let id1 = sr.settle((3, 4), 0, "Foo".into(), Vec2::ZERO, true);
        let id2 = sr.settle((3, 4), 5, "Bar".into(), Vec2::ZERO, false);
        assert_eq!(id1, id2);
        assert_eq!(sr.by_id.len(), 1);
    }
}
